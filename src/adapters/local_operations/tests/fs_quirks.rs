// SPDX-License-Identifier: MIT

//! Emulates filesystems that lack a primitive. A scope rooted at a temporary
//! directory applies its quirks to every seam call whose directory lies
//! inside it; other calls reach the system unchanged. Separate scopes act as
//! separate mounts.

use std::{
    ffi::{OsStr, OsString},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use gtk::{gio, glib};
use rustix::{
    fs::{AtFlags, FsWord, Mode, RenameFlags},
    io::Errno,
};

use super::super::fs_ops::{Entry, EntryStat, FsOps, System, assert_off_main_thread};
use crate::adapters::volume::{MountKey, forget_test_mount_capabilities};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Op {
    Rename,
    /// A rename with `RENAME_EXCHANGE`.
    Exchange,
    Link,
    Unlink,
    Mkdir,
    Symlink,
    Readlink,
    Stat,
    StatDir,
    OpenParent,
    Create,
    Names,
    MountKey,
    FsType,
    CanTrash,
    Trash,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum Support {
    #[default]
    Supported,
    Rejected(Errno),
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum Exchange {
    #[default]
    Supported,
    Rejected,
    /// Overwrites the target instead of swapping, as some FUSE daemons do.
    IgnoredOverwrites,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum OverEmptyDirectory {
    #[default]
    Supported,
    NotEmpty,
    Eperm,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum Trash {
    #[default]
    Supported,
    Unsupported,
}

pub(crate) type Racer = Box<dyn FnMut() + Send>;

/// Patterns match a name exactly, or by prefix when they end in `*`.
#[derive(Default)]
pub(crate) struct Quirks {
    pub rename_flags: Support,
    pub exchange: Exchange,
    pub links: Support,
    pub collisions_as_eperm: bool,
    pub rename_over_empty_dir: OverEmptyDirectory,
    pub case_insensitive: bool,
    /// Names rejected with `EINVAL` by rename, mkdir, symlink and create,
    /// and with `EPERM` by link, as on vfat.
    pub invalid_name: Option<fn(&OsStr) -> bool>,
    pub fs_type: Option<FsWord>,
    pub trash: Trash,
    /// Performs the call, then reports this errno once.
    pub lost_reply: Vec<(Op, &'static str, Errno)>,
    /// Fails the matching call after skipping the given number of matches, once.
    pub fail: Vec<(Op, &'static str, usize, Errno)>,
    /// Runs once, before the matching call after skipping the given number of matches.
    pub before: Vec<(Op, &'static str, usize, Racer)>,
}

impl Quirks {
    pub(crate) fn without_rename_flags() -> Self {
        Self {
            rename_flags: Support::Rejected(Errno::INVAL),
            ..Self::default()
        }
    }

    /// No `RENAME_NOREPLACE`, no hard links, and no rename over an empty folder.
    pub(crate) fn without_placement() -> Self {
        Self {
            rename_flags: Support::Rejected(Errno::INVAL),
            links: Support::Rejected(Errno::PERM),
            rename_over_empty_dir: OverEmptyDirectory::NotEmpty,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Call {
    pub op: Op,
    pub names: Vec<OsString>,
}

struct Scope {
    id: u64,
    root: PathBuf,
    state: Mutex<State>,
}

struct State {
    quirks: Quirks,
    calls: Vec<Call>,
}

static SCOPES: Mutex<Vec<Arc<Scope>>> = Mutex::new(Vec::new());
static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);

fn scopes() -> MutexGuard<'static, Vec<Arc<Scope>>> {
    SCOPES.lock().unwrap_or_else(|error| error.into_inner())
}

/// Keeps a scope registered until dropped.
pub(crate) struct FsQuirks {
    scope: Arc<Scope>,
}

impl FsQuirks {
    pub(crate) fn install(root: &Path, quirks: Quirks) -> Self {
        let scope = Arc::new(Scope {
            id: NEXT_SCOPE.fetch_add(1, Ordering::Relaxed),
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            state: Mutex::new(State {
                quirks,
                calls: Vec::new(),
            }),
        });
        scopes().push(scope.clone());
        Self { scope }
    }

    pub(crate) fn key(&self) -> MountKey {
        MountKey::Test(self.scope.id)
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.scope.state().calls.clone()
    }

    pub(crate) fn count(&self, op: Op) -> usize {
        self.scope
            .state()
            .calls
            .iter()
            .filter(|call| call.op == op)
            .count()
    }

    pub(crate) fn clear_calls(&self) {
        self.scope.state().calls.clear();
    }
}

impl Drop for FsQuirks {
    fn drop(&mut self) {
        scopes().retain(|scope| !Arc::ptr_eq(scope, &self.scope));
        forget_test_mount_capabilities(self.scope.id);
    }
}

fn matches(pattern: &str, names: &[&OsStr]) -> bool {
    names.iter().any(|name| match pattern.strip_suffix('*') {
        Some(prefix) => name.as_encoded_bytes().starts_with(prefix.as_bytes()),
        None => name.as_encoded_bytes() == pattern.as_bytes(),
    })
}

impl Scope {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn run<T>(
        &self,
        op: Op,
        names: &[&OsStr],
        perform: impl FnOnce(&Self) -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        let racer = {
            let mut state = self.state();
            state.calls.push(Call {
                op,
                names: names.iter().map(|name| name.to_os_string()).collect(),
            });
            let index = state
                .quirks
                .before
                .iter()
                .position(|(racer_op, pattern, _, _)| *racer_op == op && matches(pattern, names));
            match index {
                Some(index) if state.quirks.before[index].2 == 0 => {
                    Some(state.quirks.before.remove(index).3)
                }
                Some(index) => {
                    state.quirks.before[index].2 -= 1;
                    None
                }
                None => None,
            }
        };
        if let Some(mut racer) = racer {
            racer();
        }
        let fault = {
            let mut state = self.state();
            let mut fault = None;
            if let Some(index) = state
                .quirks
                .fail
                .iter()
                .position(|(fail_op, pattern, _, _)| *fail_op == op && matches(pattern, names))
            {
                let (_, _, skip, errno) = &mut state.quirks.fail[index];
                if *skip == 0 {
                    fault = Some(*errno);
                    state.quirks.fail.remove(index);
                } else {
                    *skip -= 1;
                }
            }
            fault
        };
        if let Some(errno) = fault {
            return Err(errno);
        }
        let result = perform(self);
        if result.is_ok() {
            let mut state = self.state();
            if let Some(index) = state
                .quirks
                .lost_reply
                .iter()
                .position(|(lost_op, pattern, _)| *lost_op == op && matches(pattern, names))
            {
                return Err(state.quirks.lost_reply.remove(index).2);
            }
        }
        result
    }

    fn quirk<T>(&self, read: impl FnOnce(&Quirks) -> T) -> T {
        read(&self.state().quirks)
    }

    fn invalid(&self, name: &OsStr) -> bool {
        self.quirk(|quirks| quirks.invalid_name)
            .is_some_and(|invalid| invalid(name))
    }

    /// The differently-cased existing name a case-insensitive lookup of
    /// `entry` would find.
    fn alias(&self, entry: Entry<'_>) -> Option<OsString> {
        if !self.quirk(|quirks| quirks.case_insensitive) || System.stat(entry, false).is_ok() {
            return None;
        }
        System
            .names(Entry {
                dir: entry.dir,
                name: OsStr::new("."),
            })
            .ok()?
            .into_iter()
            .find(|name| name != entry.name && name.eq_ignore_ascii_case(entry.name))
    }

    fn occupied(&self, entry: Entry<'_>) -> bool {
        System.stat(entry, false).is_ok() || self.alias(entry).is_some()
    }

    fn collision(&self) -> Errno {
        if self.quirk(|quirks| quirks.collisions_as_eperm) {
            Errno::PERM
        } else {
            Errno::EXIST
        }
    }

    fn creating<T>(&self, at: Entry<'_>, create: impl FnOnce() -> Result<T, Errno>) -> Result<T, Errno> {
        if self.invalid(at.name) {
            return Err(Errno::INVAL);
        }
        if self.alias(at).is_some() {
            return Err(self.collision());
        }
        create().map_err(|error| {
            if error == Errno::EXIST {
                self.collision()
            } else {
                error
            }
        })
    }

    fn rename(&self, from: Entry<'_>, to: Entry<'_>, flags: RenameFlags) -> Result<(), Errno> {
        let (rename_flags, exchange, over_empty) = self.quirk(|quirks| {
            (
                quirks.rename_flags,
                quirks.exchange,
                quirks.rename_over_empty_dir,
            )
        });
        if flags.contains(RenameFlags::NOREPLACE) {
            if self.occupied(to) {
                return Err(Errno::EXIST);
            }
            if let Support::Rejected(error) = rename_flags {
                return Err(error);
            }
        }
        if self.invalid(to.name) {
            return Err(Errno::INVAL);
        }
        if flags.contains(RenameFlags::EXCHANGE) {
            match exchange {
                Exchange::Supported => {}
                Exchange::Rejected => return Err(Errno::INVAL),
                Exchange::IgnoredOverwrites => {
                    return System.rename(from, to, RenameFlags::empty());
                }
            }
        }
        if flags.is_empty() {
            let source_is_directory = System.stat(from, false).is_ok_and(|stat| stat.is_directory());
            let target_is_empty_directory = System.stat(to, false).is_ok_and(|stat| stat.is_directory())
                && System.names(to).is_ok_and(|names| names.is_empty());
            if source_is_directory && target_is_empty_directory {
                match over_empty {
                    OverEmptyDirectory::Supported => {}
                    OverEmptyDirectory::NotEmpty => return Err(Errno::NOTEMPTY),
                    OverEmptyDirectory::Eperm => return Err(Errno::PERM),
                }
            }
            if let Some(alias) = self.alias(to) {
                return System.rename(
                    from,
                    Entry {
                        dir: to.dir,
                        name: &alias,
                    },
                    flags,
                );
            }
        }
        System.rename(from, to, flags)
    }

    fn link(&self, from: Entry<'_>, to: Entry<'_>) -> Result<(), Errno> {
        if self.invalid(to.name) {
            return Err(Errno::PERM);
        }
        if self.occupied(to) {
            return Err(self.collision());
        }
        if let Support::Rejected(error) = self.quirk(|quirks| quirks.links) {
            return Err(error);
        }
        System.link(from, to).map_err(|error| {
            if error == Errno::EXIST {
                self.collision()
            } else {
                error
            }
        })
    }

    fn resolved<T>(&self, at: Entry<'_>, read: impl FnOnce(Entry<'_>) -> T) -> T {
        match self.alias(at) {
            Some(alias) => read(Entry {
                dir: at.dir,
                name: &alias,
            }),
            None => read(at),
        }
    }
}

fn path_of(dir: BorrowedFd<'_>) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", dir.as_raw_fd())).ok()
}

fn scope_for(dir: BorrowedFd<'_>) -> Option<Arc<Scope>> {
    let path = path_of(dir)?;
    scopes()
        .iter()
        .filter(|scope| path.starts_with(&scope.root))
        .max_by_key(|scope| scope.root.as_os_str().len())
        .cloned()
}

fn scope_for_path(path: &Path) -> Option<Arc<Scope>> {
    let parent = path.parent()?.canonicalize().ok()?;
    scopes()
        .iter()
        .filter(|scope| parent.starts_with(&scope.root))
        .max_by_key(|scope| scope.root.as_os_str().len())
        .cloned()
}

fn same_scope(left: &Option<Arc<Scope>>, right: &Option<Arc<Scope>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

pub(crate) struct Scoped;

pub(crate) static SCOPED: Scoped = Scoped;

impl FsOps for Scoped {
    fn rename(&self, from: Entry<'_>, to: Entry<'_>, flags: RenameFlags) -> Result<(), Errno> {
        assert_off_main_thread();
        let source = scope_for(from.dir);
        let target = scope_for(to.dir);
        if !same_scope(&source, &target) {
            return Err(Errno::XDEV);
        }
        let op = if flags.contains(RenameFlags::EXCHANGE) {
            Op::Exchange
        } else {
            Op::Rename
        };
        match target {
            Some(scope) => scope.run(op, &[from.name, to.name], |scope| {
                scope.rename(from, to, flags)
            }),
            None => System.rename(from, to, flags),
        }
    }

    fn link(&self, from: Entry<'_>, to: Entry<'_>) -> Result<(), Errno> {
        assert_off_main_thread();
        let source = scope_for(from.dir);
        let target = scope_for(to.dir);
        if !same_scope(&source, &target) {
            return Err(Errno::XDEV);
        }
        match target {
            Some(scope) => scope.run(Op::Link, &[from.name, to.name], |scope| {
                scope.link(from, to)
            }),
            None => System.link(from, to),
        }
    }

    fn unlink(&self, at: Entry<'_>, flags: AtFlags) -> Result<(), Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::Unlink, &[at.name], |scope| {
                scope.resolved(at, |at| System.unlink(at, flags))
            }),
            None => System.unlink(at, flags),
        }
    }

    fn mkdir(&self, at: Entry<'_>, mode: Mode) -> Result<(), Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::Mkdir, &[at.name], |scope| {
                scope.creating(at, || System.mkdir(at, mode))
            }),
            None => System.mkdir(at, mode),
        }
    }

    fn symlink(&self, target: &OsStr, at: Entry<'_>) -> Result<(), Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::Symlink, &[at.name], |scope| {
                scope.creating(at, || System.symlink(target, at))
            }),
            None => System.symlink(target, at),
        }
    }

    fn readlink(&self, at: Entry<'_>) -> Result<OsString, Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::Readlink, &[at.name], |scope| {
                scope.resolved(at, |at| System.readlink(at))
            }),
            None => System.readlink(at),
        }
    }

    fn stat(&self, at: Entry<'_>, force_sync: bool) -> Result<EntryStat, Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::Stat, &[at.name], |scope| {
                scope.resolved(at, |at| System.stat(at, force_sync))
            }),
            None => System.stat(at, force_sync),
        }
    }

    fn stat_dir(&self, dir: BorrowedFd<'_>) -> Result<EntryStat, Errno> {
        assert_off_main_thread();
        match scope_for(dir) {
            Some(scope) => scope.run(Op::StatDir, &[], |_| System.stat_dir(dir)),
            None => System.stat_dir(dir),
        }
    }

    fn open_parent(&self, dir: BorrowedFd<'_>) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        match scope_for(dir) {
            Some(scope) => scope.run(Op::OpenParent, &[], |_| System.open_parent(dir)),
            None => System.open_parent(dir),
        }
    }

    fn create_exclusive(&self, at: Entry<'_>, mode: Mode) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::Create, &[at.name], |scope| {
                scope.creating(at, || System.create_exclusive(at, mode))
            }),
            None => System.create_exclusive(at, mode),
        }
    }

    fn names(&self, dir: Entry<'_>) -> Result<Vec<OsString>, Errno> {
        assert_off_main_thread();
        match scope_for(dir.dir) {
            Some(scope) => scope.run(Op::Names, &[dir.name], |_| System.names(dir)),
            None => System.names(dir),
        }
    }

    fn mount_key(&self, dir: BorrowedFd<'_>) -> MountKey {
        assert_off_main_thread();
        match scope_for(dir) {
            Some(scope) => scope
                .run(Op::MountKey, &[], |scope| Ok(MountKey::Test(scope.id)))
                .unwrap_or(MountKey::Test(scope.id)),
            None => System.mount_key(dir),
        }
    }

    fn fs_type(&self, dir: BorrowedFd<'_>) -> Option<FsWord> {
        assert_off_main_thread();
        match scope_for(dir) {
            Some(scope) => scope
                .run(Op::FsType, &[], |scope| {
                    Ok(scope
                        .quirk(|quirks| quirks.fs_type)
                        .or_else(|| System.fs_type(dir)))
                })
                .ok()
                .flatten(),
            None => System.fs_type(dir),
        }
    }

    fn can_trash(&self, path: &Path) -> bool {
        assert_off_main_thread();
        match scope_for_path(path) {
            Some(scope) => scope
                .run(Op::CanTrash, &[], |scope| {
                    Ok(match scope.quirk(|quirks| quirks.trash) {
                        Trash::Supported => System.can_trash(path),
                        Trash::Unsupported => false,
                    })
                })
                .unwrap_or(false),
            None => System.can_trash(path),
        }
    }

    fn trash(&self, path: &Path, cancellable: &gio::Cancellable) -> Result<(), glib::Error> {
        assert_off_main_thread();
        let Some(scope) = scope_for_path(path) else {
            return System.trash(path, cancellable);
        };
        let name = path.file_name().unwrap_or_default();
        let mut outcome = Ok(());
        let _ = scope.run(Op::Trash, &[name], |scope| {
            outcome = match scope.quirk(|quirks| quirks.trash) {
                Trash::Supported => System.trash(path, cancellable),
                Trash::Unsupported => Err(glib::Error::new(
                    gio::IOErrorEnum::NotSupported,
                    "Trash is not supported here",
                )),
            };
            Ok(())
        });
        outcome
    }
}
