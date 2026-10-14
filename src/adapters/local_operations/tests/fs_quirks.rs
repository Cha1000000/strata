// SPDX-License-Identifier: MIT

//! Emulates filesystems that lack a primitive. A scope rooted at a temporary
//! directory applies its quirks to every seam call whose directory lies
//! inside it; other calls reach the system unchanged. Separate scopes act as
//! separate mounts.

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use gtk::{gio, glib};
use rustix::{
    fs::{AtFlags, FileType, FsWord, Mode, RenameFlags},
    io::Errno,
};

use super::super::copy_engine::forget_learned_copy_support;
use super::super::fs_ops::{Entry, EntryStat, FsOps, System, assert_off_main_thread};
use crate::adapters::volume::{MountKey, MountType, forget_test_mount_capabilities};

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
    StatFd,
    OpenParent,
    Create,
    Names,
    MountKey,
    FsType,
    CanTrash,
    Trash,
    OpenSource,
    OpenTmpfile,
    LinkFd,
    Clone,
    CopyRange,
    Read,
    Write,
    SetLen,
    SeekData,
    SeekHole,
    SyncData,
    SyncFs,
    SyncDir,
    Close,
    ChownFd,
    ChmodFd,
    SetTimes,
    XattrNames,
    GetXattr,
    SetXattr,
    MountType,
    DeviceFormat,
    OpenDir,
    List,
    CreateNode,
    OpenFifo,
    CreateReadonly,
    OpenWrite,
    FileFlags,
    SetLinkTimes,
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
    /// GLib #4060: FALSE without an error, and nothing moved.
    FalseWithoutError,
    /// TRUE, but nothing moved.
    SucceedsWithoutMoving,
    /// Offered, but moving an item there is not supported.
    Refused,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum TmpfileLink {
    #[default]
    Any,
    /// `AT_EMPTY_PATH` needs privileges, as before Linux 6.10.
    ProcOnly,
    /// Neither form works, as before 6.10 without `/proc`.
    Unavailable,
}

/// Opening a name of a file fails with `EINVAL` where another name of it
/// is in the way, as the SMB client's one lease per file makes a second
/// open fail, or a Samba server defer it forever.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum NameOpens {
    #[default]
    Any,
    /// Once another name was opened at all, which pins that a copy never
    /// opens a later name.
    FirstOnly,
    /// While another name is open.
    OneAtATime,
}

pub(crate) type Racer = Box<dyn FnMut() + Send>;
pub(crate) type Preset = fn() -> Quirks;

/// Patterns match a name exactly, or by prefix when they end in `*`.
#[derive(Default)]
pub(crate) struct Quirks {
    pub rename_flags: Support,
    pub exchange: Exchange,
    pub links: Support,
    pub collisions_as_eperm: bool,
    pub rename_over_empty_dir: OverEmptyDirectory,
    pub case_insensitive: bool,
    /// Keeps answering for a name found through a case alias until a forced
    /// revalidation, as the SMB client's lookup cache does for about a second.
    pub stale_aliases: bool,
    /// A case alias reports its own inode number, as libfuse without `use_ino` does.
    pub alias_inodes: bool,
    /// Each name a hard link adds reports its own inode number, which a rename
    /// keeps, as libfuse without `use_ino` (sshfs) and cifs without server
    /// inode numbers do.
    pub link_inodes: bool,
    /// Every entry reports one link, as sshfs does, whose SFTP attributes have none.
    pub single_link_count: bool,
    /// A rename whose reply is lost leaves its target name reporting the inode
    /// number it had, as libfuse without `use_ino` moves a node only on success.
    pub lost_renames_keep_inodes: bool,
    /// Names rejected with `EINVAL` by rename, mkdir, symlink and create,
    /// and with `EPERM` by link, as on vfat.
    pub invalid_name: Option<fn(&OsStr) -> bool>,
    pub fs_type: Option<FsWord>,
    pub trash: Trash,
    /// Reports the first inode number seen at each matching name for whatever
    /// is there later, as ext4, NFS and CIFS hand a freed number straight out,
    /// with a birth time that changes only when [`recreated`] marks a new item.
    pub reused_inodes: Option<&'static str>,
    /// As on NFSv3, ntfs-3g and exfat-fuse.
    pub no_birth_time: bool,
    /// The change time reports the modification time, as on FAT and exFAT,
    /// which store none.
    pub ctime_is_mtime: bool,
    /// Performs the call, then reports this errno once.
    pub lost_reply: Vec<(Op, &'static str, Errno)>,
    /// Fails the matching call after skipping the given number of matches, once.
    pub fail: Vec<(Op, &'static str, usize, Errno)>,
    /// Runs once, before the matching call after skipping the given number of matches.
    pub before: Vec<(Op, &'static str, usize, Racer)>,
    /// Runs once, after the matching call after skipping the given number of matches.
    pub after: Vec<(Op, &'static str, usize, Racer)>,
    /// The mountinfo type and superblock options `mount_type` reports.
    pub mount_type: Option<(&'static str, &'static str)>,
    /// What udev's database records the scope's device as holding.
    pub device_format: Option<&'static str>,
    pub tmpfile: Support,
    pub tmpfile_link: TmpfileLink,
    /// The most bytes one read or write moves.
    pub short_io: Option<usize>,
    /// Closing a written file sets its times to the close time, as NFSv4.2
    /// write delegations and FUSE writeback caches do.
    pub times_reset_at_close: bool,
    pub chown: Support,
    /// Entries report a group the user is not in.
    pub foreign_group: bool,
    /// Refuses setting xattrs whose names start with the prefix.
    pub xattrs_refused: Vec<(&'static str, Errno)>,
    /// Listed and read on every file, besides its real xattrs.
    pub extra_xattrs: Vec<(&'static str, &'static [u8])>,
    /// `mknodat` of a regular file, which FUSE daemons without `mknod` refuse.
    pub node_create: Support,
    /// `mknodat` of a named pipe, which FAT, exFAT and SMB refuse.
    pub fifos: Support,
    /// Unlinking a file this process holds open renames it to `.nfs*`
    /// instead, as the NFS client does until the last close.
    pub silly_rename: bool,
    /// Files report a size of 0 whatever they hold, so `copy_file_range`
    /// copies nothing from them, as virtual files on FUSE (lxcfs) do.
    pub virtual_sizes: bool,
    /// `FICLONE`, which btrfs and XFS hosts would otherwise perform.
    pub reflink: Support,
    /// `copy_file_range` copies nothing past this offset, as when a source
    /// shrinks after it was opened.
    pub copy_range_end: Option<u64>,
    /// Entries matching the name report this mode, as another user's file
    /// readable through its group or others does to the user copying it.
    pub reported_mode: Option<(&'static str, u32)>,
    /// Which names of a file with several open.
    pub name_opens: NameOpens,
    /// Giving a folder write access back right after taking it does
    /// nothing until a stat that revalidates comes between, as the SMB
    /// client without POSIX extensions decides from attributes it cached.
    pub stale_read_only: bool,
    /// Lists `system.*` names, such as ACLs, before the others, as tmpfs
    /// and NFSv4 do whatever order the host's filesystem keeps.
    pub acls_listed_first: bool,
}

impl Quirks {
    pub(crate) fn without_rename_flags() -> Self {
        Self {
            rename_flags: Support::Rejected(Errno::INVAL),
            ..Self::default()
        }
    }

    /// No `RENAME_NOREPLACE`, no hard links, and no rename over an empty folder.
    pub(crate) fn vboxsf() -> Self {
        Self {
            rename_flags: Support::Rejected(Errno::INVAL),
            links: Support::Rejected(Errno::PERM),
            rename_over_empty_dir: OverEmptyDirectory::NotEmpty,
            ..Self::default()
        }
    }

    /// Hard links work, so folders never take the reserved-name rung, and
    /// nothing else places a folder without `RENAME_NOREPLACE`.
    pub(crate) fn without_folder_placement() -> Self {
        Self {
            rename_over_empty_dir: OverEmptyDirectory::NotEmpty,
            ..Self::without_rename_flags()
        }
    }

    /// No rung that cannot replace: no `RENAME_NOREPLACE` or rename over an
    /// empty folder, and links fail with an errno that says nothing about the
    /// mount, so the reserved-name rung stays unconfirmed.
    pub(crate) fn without_placement() -> Self {
        Self {
            links: Support::Rejected(Errno::MLINK),
            ..Self::without_folder_placement()
        }
    }

    pub(crate) fn ext4() -> Self {
        Self {
            mount_type: Some(("ext4", "rw")),
            fs_type: Some(0xEF53),
            ..Self::default()
        }
    }

    /// NOREPLACE fails with `EINVAL`, and a write delegation sets the
    /// times of a written file when it is closed.
    pub(crate) fn nfs42() -> Self {
        Self {
            mount_type: Some(("nfs4", "rw,vers=4.2")),
            fs_type: Some(0x6969),
            rename_flags: Support::Rejected(Errno::INVAL),
            times_reset_at_close: true,
            ..Self::default()
        }
    }

    pub(crate) fn nfs3() -> Self {
        Self {
            mount_type: Some(("nfs", "rw,vers=3")),
            times_reset_at_close: false,
            xattrs_refused: vec![("", Errno::OPNOTSUPP)],
            no_birth_time: true,
            ..Self::nfs42()
        }
    }

    /// No hard links, named pipes or xattrs, and names compared without case.
    pub(crate) fn vfat() -> Self {
        Self {
            mount_type: Some(("vfat", "rw,flush")),
            fs_type: Some(0x4d44),
            links: Support::Rejected(Errno::PERM),
            fifos: Support::Rejected(Errno::PERM),
            case_insensitive: true,
            xattrs_refused: vec![("", Errno::OPNOTSUPP)],
            ..Self::default()
        }
    }

    pub(crate) fn sshfs() -> Self {
        Self {
            mount_type: Some(("fuse.sshfs", "rw")),
            fs_type: Some(0x6573_5546),
            xattrs_refused: vec![("", Errno::OPNOTSUPP)],
            rename_flags: Support::Rejected(Errno::INVAL),
            collisions_as_eperm: true,
            link_inodes: true,
            single_link_count: true,
            lost_renames_keep_inodes: true,
            no_birth_time: true,
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
    inodes: HashMap<PathBuf, u64>,
    births: HashMap<PathBuf, i64>,
    /// Real and reported inode number per name a hard link added, or a lost
    /// rename reply left reporting its old number.
    link_names: HashMap<PathBuf, (u64, u64)>,
    links_made: u64,
    stale: HashMap<PathBuf, EntryStat>,
    written: HashSet<(u64, u64)>,
    /// Names whose entries report another inode number, as after an eviction.
    renumbered: HashSet<PathBuf>,
    /// Writebacks lost so far, and how many each open folder has reported.
    writebacks_lost: u64,
    reported_losses: HashMap<RawFd, u64>,
    /// Each name a file was opened by, by device and inode, with the
    /// descriptor that opened it.
    opened_names: Vec<((u64, u64), PathBuf, RawFd)>,
    /// Folders made read-only that no revalidating stat has seen since.
    read_only_unseen: HashSet<PathBuf>,
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
                inodes: HashMap::new(),
                births: HashMap::new(),
                link_names: HashMap::new(),
                links_made: 0,
                stale: HashMap::new(),
                written: HashSet::new(),
                renumbered: HashSet::new(),
                writebacks_lost: 0,
                reported_losses: HashMap::new(),
                opened_names: Vec::new(),
                read_only_unseen: HashSet::new(),
            }),
        });
        scopes().push(scope.clone());
        forget_learned_copy_support();
        Self { scope }
    }

    pub(crate) fn key(&self) -> MountKey {
        MountKey::Test(self.scope.id)
    }

    /// Loses a writeback in this scope, from a racer: as errseq does,
    /// `syncfs` reports it once through each folder opened before, and
    /// never through one opened after.
    pub(crate) fn writeback_loser(&self) -> impl Fn() + Send + Sync + 'static {
        let scope = self.scope.clone();
        move || scope.state().writebacks_lost += 1
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.scope.state().calls.clone()
    }

    /// Reads the calls logged so far, from a racer in another scope.
    pub(crate) fn reader(&self) -> impl Fn() -> Vec<Call> + Send + Sync + 'static {
        let scope = self.scope.clone();
        move || scope.state().calls.clone()
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

    pub(crate) fn used_name(&self, pattern: &str) -> bool {
        self.scope.state().calls.iter().any(|call| {
            let names: Vec<&OsStr> = call.names.iter().map(OsString::as_os_str).collect();
            matches(pattern, &names)
        })
    }
}

impl Drop for FsQuirks {
    fn drop(&mut self) {
        scopes().retain(|scope| !Arc::ptr_eq(scope, &self.scope));
        forget_test_mount_capabilities(self.scope.id);
        forget_learned_copy_support();
    }
}

/// The racer for a call to `op` naming `names`, once its skips are used up.
fn take_racer(
    racers: &mut Vec<(Op, &'static str, usize, Racer)>,
    op: Op,
    names: &[&OsStr],
) -> Option<Racer> {
    let index = racers
        .iter()
        .position(|(racer_op, pattern, _, _)| *racer_op == op && matches(pattern, names))?;
    if racers[index].2 == 0 {
        return Some(racers.remove(index).3);
    }
    racers[index].2 -= 1;
    None
}

/// Whether descriptor `fd` still has the file `key` open by `name`.
fn still_open(fd: RawFd, name: &Path, key: (u64, u64)) -> bool {
    use std::os::unix::fs::MetadataExt;
    let link = format!("/proc/self/fd/{fd}");
    std::fs::read_link(&link).is_ok_and(|opened| opened == name)
        && std::fs::metadata(&link).is_ok_and(|file| (file.dev(), file.ino()) == key)
}

/// A bare `*` also matches calls that name nothing.
fn matches(pattern: &str, names: &[&OsStr]) -> bool {
    pattern == "*"
        || names.iter().any(|name| match pattern.strip_suffix('*') {
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
            take_racer(&mut state.quirks.before, op, names)
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
        let racer = take_racer(&mut self.state().quirks.after, op, names);
        if let Some(mut racer) = racer {
            racer();
        }
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

    fn reported(&self, path: Option<PathBuf>, mut stat: EntryStat) -> EntryStat {
        let mut state = self.state();
        if let Some(&(inode, reported)) = path.as_ref().and_then(|path| state.link_names.get(path))
            && inode == stat.identity.inode
        {
            stat.identity.inode = reported;
        }
        if path
            .as_ref()
            .is_some_and(|path| state.renumbered.contains(path))
        {
            stat.identity.inode = !stat.identity.inode;
        }
        if state.quirks.single_link_count {
            stat.nlink = 1;
        }
        if let (Some((pattern, mode)), Some(name)) = (
            state.quirks.reported_mode,
            path.as_deref().and_then(Path::file_name),
        ) && matches(pattern, &[name])
        {
            stat.mode = mode;
        }
        if let (Some(pattern), Some(path)) = (state.quirks.reused_inodes, path)
            && path
                .file_name()
                .is_some_and(|name| matches(pattern, &[name]))
        {
            stat.identity.inode = *state
                .inodes
                .entry(path.clone())
                .or_insert(stat.identity.inode);
            stat.identity.birth = Some((*state.births.entry(path).or_default(), 0));
        }
        if state.quirks.no_birth_time {
            stat.identity.birth = None;
        }
        if state.quirks.ctime_is_mtime {
            stat.ctime = stat.mtime;
        }
        if state.quirks.foreign_group {
            stat.gid = stat.gid.wrapping_add(1);
        }
        if state.quirks.virtual_sizes && stat.kind == FileType::RegularFile {
            stat.size = 0;
        }
        stat
    }

    fn mark_written(&self, fd: BorrowedFd<'_>) {
        if self.quirk(|quirks| quirks.times_reset_at_close)
            && let Ok(stat) = System.stat_fd(fd)
        {
            self.state()
                .written
                .insert((stat.identity.device, stat.identity.inode));
        }
    }

    /// Applies `times_reset_at_close` just before `fd` is closed.
    fn closing(&self, fd: BorrowedFd<'_>) {
        let written = System.stat_fd(fd).is_ok_and(|stat| {
            self.state()
                .written
                .remove(&(stat.identity.device, stat.identity.inode))
        });
        if written {
            let now = rustix::fs::Timespec {
                tv_sec: 0,
                tv_nsec: rustix::fs::UTIME_NOW,
            };
            let _ = rustix::fs::futimens(
                fd,
                &rustix::fs::Timestamps {
                    last_access: now,
                    last_modification: now,
                },
            );
        }
    }

    fn quirk<T>(&self, read: impl FnOnce(&Quirks) -> T) -> T {
        read(&self.state().quirks)
    }

    fn invalid(&self, name: &OsStr) -> bool {
        self.quirk(|quirks| quirks.invalid_name)
            .is_some_and(|invalid| invalid(name))
    }

    /// The differently-cased existing name a case-insensitive lookup of
    /// `entry` would find. Lookups revalidate, so an already case-insensitive
    /// FUSE mount cannot answer from its cache for a name that has moved.
    fn alias(&self, entry: Entry<'_>) -> Option<OsString> {
        if !self.quirk(|quirks| quirks.case_insensitive) || System.stat(entry, true).is_ok() {
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

    fn stat(
        &self,
        at: Entry<'_>,
        path: Option<&Path>,
        force_sync: bool,
    ) -> Result<EntryStat, Errno> {
        let (stale_aliases, alias_inodes) =
            self.quirk(|quirks| (quirks.stale_aliases, quirks.alias_inodes));
        let path = path.filter(|_| stale_aliases);
        if let Some(path) = path {
            let mut state = self.state();
            if force_sync {
                state.stale.remove(path);
            } else if let Some(stat) = state.stale.get(path) {
                return Ok(*stat);
            }
        }
        let exact = System.stat(at, force_sync);
        let Some(alias) = self.alias_if_missing(at, &exact) else {
            return exact;
        };
        let mut stat = System.stat(
            Entry {
                dir: at.dir,
                name: &alias,
            },
            force_sync,
        )?;
        if alias_inodes {
            stat.identity.inode = !stat.identity.inode;
        }
        if let Some(path) = path {
            self.state().stale.insert(path.to_path_buf(), stat);
        }
        Ok(stat)
    }

    fn stale(&self, entry: Entry<'_>) -> bool {
        path_of(entry.dir).is_some_and(|dir| self.state().stale.contains_key(&dir.join(entry.name)))
    }

    fn occupied(&self, entry: Entry<'_>) -> bool {
        System.stat(entry, true).is_ok() || self.alias(entry).is_some()
    }

    fn collision(&self) -> Errno {
        if self.quirk(|quirks| quirks.collisions_as_eperm) {
            Errno::PERM
        } else {
            Errno::EXIST
        }
    }

    fn creating<T>(
        &self,
        at: Entry<'_>,
        create: impl FnOnce() -> Result<T, Errno>,
    ) -> Result<T, Errno> {
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
            if self.occupied(to) || self.stale(to) {
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
            let source_is_directory = System
                .stat(from, false)
                .is_ok_and(|stat| stat.is_directory());
            let target_is_empty_directory =
                System.stat(to, false).is_ok_and(|stat| stat.is_directory())
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
        })?;
        if self.quirk(|quirks| quirks.link_inodes)
            && let (Some(path), Ok(stat)) = (path_of(to.dir), System.stat(to, false))
        {
            let mut state = self.state();
            state.links_made += 1;
            let reported = u64::MAX - state.links_made;
            state
                .link_names
                .insert(path.join(to.name), (stat.identity.inode, reported));
        }
        Ok(())
    }

    /// Carries the inode number a link name reports along with a rename.
    fn renamed(&self, from: Entry<'_>, to: Entry<'_>, flags: RenameFlags) {
        let (Some(from), Some(to)) = (
            path_of(from.dir).map(|dir| dir.join(from.name)),
            path_of(to.dir).map(|dir| dir.join(to.name)),
        ) else {
            return;
        };
        let mut state = self.state();
        let moved = state.link_names.remove(&from);
        let displaced = state.link_names.remove(&to);
        if let Some(moved) = moved {
            state.link_names.insert(to, moved);
        }
        if let Some(displaced) = displaced.filter(|_| flags.contains(RenameFlags::EXCHANGE)) {
            state.link_names.insert(from, displaced);
        }
    }

    fn inode_kept_on_lost_reply(&self, to: Entry<'_>) -> Option<u64> {
        if !self.quirk(|quirks| quirks.lost_renames_keep_inodes) {
            return None;
        }
        System.stat(to, false).ok().map(|stat| stat.identity.inode)
    }

    fn keep_inode(&self, to: Entry<'_>, kept: u64) {
        if let (Some(dir), Ok(stat)) = (path_of(to.dir), System.stat(to, false))
            && stat.identity.inode != kept
        {
            self.state()
                .link_names
                .insert(dir.join(to.name), (stat.identity.inode, kept));
        }
    }

    fn unlinked(&self, at: Entry<'_>) {
        if let Some(path) = path_of(at.dir) {
            self.state().link_names.remove(&path.join(at.name));
        }
    }

    /// Calls `read` with `at`, or with its alias where nothing has that
    /// exact name.
    fn resolved<T>(
        &self,
        at: Entry<'_>,
        read: impl Fn(Entry<'_>) -> Result<T, Errno>,
    ) -> Result<T, Errno> {
        let exact = read(at);
        match self.alias_if_missing(at, &exact) {
            Some(alias) => read(Entry {
                dir: at.dir,
                name: &alias,
            }),
            None => exact,
        }
    }

    /// The alias of `at` once a call naming it exactly found nothing. A real
    /// case-insensitive lookup finds the entry whichever case it was renamed
    /// to meanwhile, so the exact name is tried first.
    fn alias_if_missing<T>(&self, at: Entry<'_>, exact: &Result<T, Errno>) -> Option<OsString> {
        matches!(exact, Err(Errno::NOENT))
            .then(|| self.alias(at))
            .flatten()
    }
}

/// The inode number of the regular file at `entry`, when this process holds
/// a descriptor for it.
fn open_in_this_process(entry: Entry<'_>) -> Option<u64> {
    let stat = System.stat(entry, false).ok()?;
    if stat.kind != FileType::RegularFile {
        return None;
    }
    let held = std::fs::read_dir("/proc/self/fd")
        .ok()?
        .filter_map(Result::ok)
        .any(|fd| {
            rustix::fs::stat(fd.path()).is_ok_and(|open| {
                open.st_dev == stat.identity.device && open.st_ino == stat.identity.inode
            })
        });
    held.then_some(stat.identity.inode)
}

fn path_of(dir: BorrowedFd<'_>) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", dir.as_raw_fd())).ok()
}

/// The basename an fd-only call logs; `#<inode> (deleted)` for an unnamed file.
fn fd_name(fd: BorrowedFd<'_>) -> OsString {
    path_of(fd)
        .and_then(|path| path.file_name().map(OsStr::to_os_string))
        .unwrap_or_default()
}

/// Runs an fd-only call through the scope holding `fd`, logging its name and `extra`.
fn on_fd<T>(
    fd: BorrowedFd<'_>,
    op: Op,
    extra: Option<&OsStr>,
    perform: impl FnOnce(Option<&Scope>) -> Result<T, Errno>,
) -> Result<T, Errno> {
    match scope_for(fd) {
        Some(scope) => {
            let name = fd_name(fd);
            let mut names = vec![name.as_os_str()];
            names.extend(extra);
            scope.run(op, &names, |scope| perform(Some(scope)))
        }
        None => perform(None),
    }
}

/// A data call that moves bytes from `source` into `target`, which fails
/// with `EXDEV` across scopes as it does across mounts.
fn between(
    op: Op,
    source: BorrowedFd<'_>,
    target: BorrowedFd<'_>,
    perform: impl FnOnce() -> Result<usize, Errno>,
) -> Result<usize, Errno> {
    let same = same_scope(&scope_for(source), &scope_for(target));
    on_fd(target, op, None, |scope| {
        if !same {
            return Err(Errno::XDEV);
        }
        if let Some(scope) = scope {
            scope.mark_written(target);
        }
        perform()
    })
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

/// Gives whatever is now at `path` a new birth time under `reused_inodes`.
pub(crate) fn recreated(path: &Path) {
    let (Some(scope), Some(name)) = (scope_for_path(path), path.file_name()) else {
        return;
    };
    if let Some(parent) = path.parent().and_then(|parent| parent.canonicalize().ok()) {
        *scope.state().births.entry(parent.join(name)).or_default() += 1;
    }
}

/// Makes whatever is at `path` report another inode number from now on, as
/// libfuse without `use_ino` and cifs without server inode numbers do once
/// the kernel evicts an entry.
pub(crate) fn renumbered(path: &Path) {
    let (Some(scope), Some(name)) = (scope_for_path(path), path.file_name()) else {
        return;
    };
    if let Some(parent) = path.parent().and_then(|parent| parent.canonicalize().ok()) {
        scope.state().renumbered.insert(parent.join(name));
    }
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
            Some(scope) => {
                let kept = scope.inode_kept_on_lost_reply(to);
                let renamed = scope.run(op, &[from.name, to.name], |scope| {
                    scope.rename(from, to, flags)?;
                    scope.renamed(from, to, flags);
                    Ok(())
                });
                if let (Err(_), Some(kept)) = (renamed, kept) {
                    scope.keep_inode(to, kept);
                }
                renamed
            }
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
                if scope.quirk(|quirks| quirks.silly_rename)
                    && let Some(inode) = open_in_this_process(at)
                {
                    let aside = OsString::from(format!(".nfs{inode:016x}"));
                    let aside = Entry {
                        dir: at.dir,
                        name: &aside,
                    };
                    return System.rename(at, aside, RenameFlags::NOREPLACE);
                }
                scope.resolved(at, |at| System.unlink(at, flags))?;
                scope.unlinked(at);
                Ok(())
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
            Some(scope) => {
                let path = path_of(at.dir).map(|dir| dir.join(at.name));
                if force_sync
                    && scope.quirk(|quirks| quirks.stale_read_only)
                    && let Some(path) = path.as_deref().and_then(|path| path.canonicalize().ok())
                {
                    scope.state().read_only_unseen.remove(&path);
                }
                scope
                    .run(Op::Stat, &[at.name], |scope| {
                        scope.stat(at, path.as_deref(), force_sync)
                    })
                    .map(|stat| scope.reported(path, stat))
            }
            None => System.stat(at, force_sync),
        }
    }

    fn stat_fd(&self, fd: BorrowedFd<'_>) -> Result<EntryStat, Errno> {
        assert_off_main_thread();
        match scope_for(fd) {
            Some(scope) => scope
                .run(Op::StatFd, &[], |_| System.stat_fd(fd))
                .map(|stat| scope.reported(path_of(fd), stat)),
            None => System.stat_fd(fd),
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
                        Trash::FalseWithoutError
                        | Trash::SucceedsWithoutMoving
                        | Trash::Refused => true,
                    })
                })
                .unwrap_or(false),
            None => System.can_trash(path),
        }
    }

    fn trash(&self, path: &Path, cancellable: &gio::Cancellable) -> Result<bool, glib::Error> {
        assert_off_main_thread();
        let Some(scope) = scope_for_path(path) else {
            return System.trash(path, cancellable);
        };
        let name = path.file_name().unwrap_or_default();
        let mut outcome = Ok(true);
        let _ = scope.run(Op::Trash, &[name], |scope| {
            outcome = match scope.quirk(|quirks| quirks.trash) {
                Trash::Supported => System.trash(path, cancellable),
                Trash::Unsupported | Trash::Refused => Err(glib::Error::new(
                    gio::IOErrorEnum::NotSupported,
                    "Trash is not supported here",
                )),
                Trash::FalseWithoutError => Ok(false),
                Trash::SucceedsWithoutMoving => Ok(true),
            };
            Ok(())
        });
        outcome
    }

    fn open_source(
        &self,
        at: Entry<'_>,
        listed: bool,
    ) -> Result<Option<(OwnedFd, EntryStat)>, Errno> {
        assert_off_main_thread();
        let Some(scope) = scope_for(at.dir) else {
            return System.open_source(at, listed);
        };
        let path = path_of(at.dir).map(|dir| dir.join(at.name));
        let opened = scope.run(Op::OpenSource, &[at.name], |scope| {
            let opened = scope.resolved(at, |at| System.open_source(at, listed))?;
            let name_opens = scope.quirk(|quirks| quirks.name_opens);
            if let (Some((fd, stat)), Some(path)) = (&opened, &path)
                && name_opens != NameOpens::Any
            {
                let key = (stat.identity.device, stat.identity.inode);
                let mut state = scope.state();
                if name_opens == NameOpens::OneAtATime {
                    state
                        .opened_names
                        .retain(|&(key, ref name, fd)| still_open(fd, name, key));
                }
                if state
                    .opened_names
                    .iter()
                    .any(|(other, name, _)| *other == key && name != path)
                {
                    return Err(Errno::INVAL);
                }
                state.opened_names.push((key, path.clone(), fd.as_raw_fd()));
            }
            Ok(opened)
        })?;
        Ok(opened.map(|(fd, stat)| (fd, scope.reported(path, stat))))
    }

    fn open_tmpfile(&self, dir: BorrowedFd<'_>, mode: Mode) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        match scope_for(dir) {
            Some(scope) => scope.run(Op::OpenTmpfile, &[], |scope| {
                match scope.quirk(|quirks| quirks.tmpfile) {
                    Support::Rejected(error) => Err(error),
                    Support::Supported => System.open_tmpfile(dir, mode),
                }
            }),
            None => System.open_tmpfile(dir, mode),
        }
    }

    fn link_fd(&self, fd: BorrowedFd<'_>, to: Entry<'_>, through_proc: bool) -> Result<(), Errno> {
        assert_off_main_thread();
        let target = scope_for(to.dir);
        if !same_scope(&scope_for(fd), &target) {
            return Err(Errno::XDEV);
        }
        let Some(scope) = target else {
            return System.link_fd(fd, to, through_proc);
        };
        scope.run(Op::LinkFd, &[to.name], |scope| {
            scope.creating(to, || {
                let (links, tmpfile_link) =
                    scope.quirk(|quirks| (quirks.links, quirks.tmpfile_link));
                if let Support::Rejected(error) = links {
                    return Err(error);
                }
                match (tmpfile_link, through_proc) {
                    (TmpfileLink::ProcOnly, false) | (TmpfileLink::Unavailable, _) => {
                        Err(Errno::NOENT)
                    }
                    _ => System.link_fd(fd, to, through_proc),
                }
            })
        })
    }

    fn reflink(&self, target: BorrowedFd<'_>, source: BorrowedFd<'_>) -> Result<(), Errno> {
        assert_off_main_thread();
        let refused =
            scope_for(target).and_then(|scope| match scope.quirk(|quirks| quirks.reflink) {
                Support::Rejected(error) => Some(error),
                Support::Supported => None,
            });
        between(Op::Clone, source, target, || match refused {
            Some(error) => Err(error),
            None => System.reflink(target, source).map(|()| 0),
        })
        .map(drop)
    }

    fn copy_range(
        &self,
        source: BorrowedFd<'_>,
        source_offset: u64,
        target: BorrowedFd<'_>,
        target_offset: u64,
        len: usize,
    ) -> Result<usize, Errno> {
        assert_off_main_thread();
        let end = scope_for(source).and_then(|scope| {
            scope.quirk(|quirks| {
                if quirks.virtual_sizes {
                    Some(0)
                } else {
                    quirks.copy_range_end
                }
            })
        });
        let len = end.map_or(len, |end| {
            len.min(usize::try_from(end.saturating_sub(source_offset)).unwrap_or(len))
        });
        between(Op::CopyRange, source, target, || {
            if len == 0 {
                return Ok(0);
            }
            System.copy_range(source, source_offset, target, target_offset, len)
        })
    }

    fn read_at(&self, fd: BorrowedFd<'_>, buffer: &mut [u8], offset: u64) -> Result<usize, Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::Read, None, |scope| {
            let limit = scope
                .and_then(|scope| scope.quirk(|quirks| quirks.short_io))
                .unwrap_or(buffer.len())
                .min(buffer.len());
            System.read_at(fd, &mut buffer[..limit], offset)
        })
    }

    fn write_at(&self, fd: BorrowedFd<'_>, buffer: &[u8], offset: u64) -> Result<usize, Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::Write, None, |scope| {
            let limit = scope
                .and_then(|scope| scope.quirk(|quirks| quirks.short_io))
                .unwrap_or(buffer.len())
                .min(buffer.len());
            if let Some(scope) = scope {
                scope.mark_written(fd);
            }
            System.write_at(fd, &buffer[..limit], offset)
        })
    }

    fn set_len(&self, fd: BorrowedFd<'_>, len: u64) -> Result<(), Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::SetLen, None, |_| System.set_len(fd, len))
    }

    fn seek_data(&self, fd: BorrowedFd<'_>, offset: u64) -> Result<u64, Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::SeekData, None, |_| System.seek_data(fd, offset))
    }

    fn seek_hole(&self, fd: BorrowedFd<'_>, offset: u64) -> Result<u64, Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::SeekHole, None, |_| System.seek_hole(fd, offset))
    }

    fn sync_data(&self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::SyncData, None, |_| System.sync_data(fd))
    }

    fn sync_fs(&self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::SyncFs, None, |scope| {
            if let Some(scope) = scope {
                let mut state = scope.state();
                let lost = state.writebacks_lost;
                let reported = state.reported_losses.entry(fd.as_raw_fd()).or_default();
                if lost > *reported {
                    *reported = lost;
                    return Err(Errno::IO);
                }
            }
            System.sync_fs(fd)
        })
    }

    fn sync_dir(&self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::SyncDir, None, |_| System.sync_dir(fd))
    }

    fn close(&self, fd: OwnedFd) -> Result<(), Errno> {
        assert_off_main_thread();
        let Some(scope) = scope_for(fd.as_fd()) else {
            return System.close(fd);
        };
        let name = fd_name(fd.as_fd());
        // A fault leaves `fd` to its drop, which closes it.
        scope.run(Op::Close, &[&name], move |scope| {
            scope.closing(fd.as_fd());
            System.close(fd)
        })
    }

    fn chown_fd(
        &self,
        fd: BorrowedFd<'_>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<(), Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::ChownFd, None, |scope| {
            match scope.map(|scope| scope.quirk(|quirks| quirks.chown)) {
                Some(Support::Rejected(error)) => Err(error),
                _ => System.chown_fd(fd, uid, gid),
            }
        })
    }

    fn chmod_fd(&self, fd: BorrowedFd<'_>, mode: u32) -> Result<(), Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::ChmodFd, None, |scope| {
            if let (Some(scope), Some(path)) = (scope, path_of(fd))
                && scope.quirk(|quirks| quirks.stale_read_only)
                && path.is_dir()
            {
                let mut state = scope.state();
                if mode & 0o200 == 0 {
                    state.read_only_unseen.insert(path);
                } else if state.read_only_unseen.contains(&path) {
                    return Ok(());
                }
            }
            System.chmod_fd(fd, mode)
        })
    }

    fn set_times_fd(
        &self,
        fd: BorrowedFd<'_>,
        atime: (i64, u32),
        mtime: (i64, u32),
    ) -> Result<(), Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::SetTimes, None, |_| {
            System.set_times_fd(fd, atime, mtime)
        })
    }

    fn xattr_names(&self, fd: BorrowedFd<'_>) -> Result<Vec<OsString>, Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::XattrNames, None, |scope| {
            let extra = scope.map_or_else(Vec::new, |scope| {
                scope.quirk(|quirks| quirks.extra_xattrs.clone())
            });
            let mut names = match System.xattr_names(fd) {
                Ok(names) => names,
                Err(_) if !extra.is_empty() => Vec::new(),
                Err(error) => return Err(error),
            };
            names.extend(extra.iter().map(|(name, _)| OsString::from(name)));
            if scope.is_some_and(|scope| scope.quirk(|quirks| quirks.acls_listed_first)) {
                names.sort_by_key(|name| !name.as_encoded_bytes().starts_with(b"system."));
            }
            Ok(names)
        })
    }

    fn get_xattr(&self, fd: BorrowedFd<'_>, name: &OsStr) -> Result<Vec<u8>, Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::GetXattr, Some(name), |scope| {
            let extra = scope.and_then(|scope| {
                scope.quirk(|quirks| {
                    quirks
                        .extra_xattrs
                        .iter()
                        .find(|(extra, _)| OsStr::new(extra) == name)
                        .map(|(_, value)| value.to_vec())
                })
            });
            extra.map_or_else(|| System.get_xattr(fd, name), Ok)
        })
    }

    fn set_xattr(&self, fd: BorrowedFd<'_>, name: &OsStr, value: &[u8]) -> Result<(), Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::SetXattr, Some(name), |scope| {
            let refused = scope.and_then(|scope| {
                scope.quirk(|quirks| {
                    quirks
                        .xattrs_refused
                        .iter()
                        .find(|(prefix, _)| name.as_encoded_bytes().starts_with(prefix.as_bytes()))
                        .map(|(_, error)| *error)
                })
            });
            refused.map_or_else(|| System.set_xattr(fd, name, value), Err)
        })
    }

    fn mount_type(&self, fd: BorrowedFd<'_>) -> Option<MountType> {
        assert_off_main_thread();
        match scope_for(fd) {
            Some(scope) => scope
                .run(Op::MountType, &[], |scope| {
                    Ok(scope
                        .quirk(|quirks| quirks.mount_type)
                        .map(|(fs_type, super_options)| MountType {
                            fs_type: fs_type.to_owned(),
                            super_options: super_options.to_owned(),
                        })
                        .or_else(|| System.mount_type(fd)))
                })
                .ok()
                .flatten(),
            None => System.mount_type(fd),
        }
    }

    fn device_format(&self, fd: BorrowedFd<'_>) -> Result<Option<String>, Errno> {
        assert_off_main_thread();
        match scope_for(fd) {
            Some(scope) => scope.run(Op::DeviceFormat, &[], |scope| {
                Ok(scope
                    .quirk(|quirks| quirks.device_format)
                    .map(str::to_owned))
            }),
            None => System.device_format(fd),
        }
    }

    fn open_dir(&self, at: Entry<'_>) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::OpenDir, &[at.name], |scope| {
                let opened = scope.resolved(at, |at| System.open_dir(at))?;
                let mut state = scope.state();
                let lost = state.writebacks_lost;
                state.reported_losses.insert(opened.as_raw_fd(), lost);
                Ok(opened)
            }),
            None => System.open_dir(at),
        }
    }

    fn list(&self, dir: BorrowedFd<'_>) -> Result<Vec<(OsString, FileType)>, Errno> {
        assert_off_main_thread();
        on_fd(dir, Op::List, None, |_| System.list(dir))
    }

    fn create_node(&self, at: Entry<'_>, kind: FileType, mode: Mode) -> Result<(), Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::CreateNode, &[at.name], |scope| {
                let support = scope.quirk(|quirks| match kind {
                    FileType::Fifo => quirks.fifos,
                    _ => quirks.node_create,
                });
                scope.creating(at, || match support {
                    Support::Rejected(error) => Err(error),
                    Support::Supported => System.create_node(at, kind, mode),
                })
            }),
            None => System.create_node(at, kind, mode),
        }
    }

    fn open_fifo(&self, at: Entry<'_>) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::OpenFifo, &[at.name], |scope| {
                scope.resolved(at, |at| System.open_fifo(at))
            }),
            None => System.open_fifo(at),
        }
    }

    fn create_readonly(&self, at: Entry<'_>, mode: Mode) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::CreateReadonly, &[at.name], |scope| {
                scope.creating(at, || System.create_readonly(at, mode))
            }),
            None => System.create_readonly(at, mode),
        }
    }

    fn open_write(&self, at: Entry<'_>) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::OpenWrite, &[at.name], |scope| {
                scope.resolved(at, |at| System.open_write(at))
            }),
            None => System.open_write(at),
        }
    }

    fn file_flags(&self, fd: BorrowedFd<'_>) -> Result<u32, Errno> {
        assert_off_main_thread();
        on_fd(fd, Op::FileFlags, None, |_| System.file_flags(fd))
    }

    fn set_link_times(
        &self,
        at: Entry<'_>,
        atime: (i64, u32),
        mtime: (i64, u32),
    ) -> Result<(), Errno> {
        assert_off_main_thread();
        match scope_for(at.dir) {
            Some(scope) => scope.run(Op::SetLinkTimes, &[at.name], |scope| {
                scope.resolved(at, |at| System.set_link_times(at, atime, mtime))
            }),
            None => System.set_link_times(at, atime, mtime),
        }
    }
}
