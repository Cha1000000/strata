// SPDX-License-Identifier: MIT

//! Copies native items through descriptors on worker threads. A copy never
//! replaces an entry, and shows a partial item under its final name only
//! where the destination cannot place a stage.

mod data;
mod links;
mod metadata;
mod restrictive;
pub(super) mod stage;
mod tree;

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};

use gtk::{gio, glib};
use rustix::{
    fs::{AtFlags, FileType, Mode},
    io::Errno,
};

use super::{
    LocalFileIdentity, PinnedEntry, Reported, StageContent, StagedSibling, TransferProgressTracker,
    bounded_local_worker_count, cancelled_local_operation, copy_failure_after_cleanup,
    fs_ops::{Entry, EntryStat, FsOps, fs},
    io_error, local_delete_worker_count, local_device_is_rotational, open_local_parent_directory,
    os_error_detail, parallel_delete_local_blocking_with_workers, placement,
    placement::{COPY_PREFIX, ENOTSUPP, PlaceError, STAGE_PREFIX},
    placement_error, run_cancellable,
};
use crate::adapters::volume::{
    Family, MountCapabilities, MountKey, family, learn_mount_capability, mount_capabilities,
};
use data::Data;
use metadata::Snapshot;
pub(super) use restrictive::Restrictive;

/// Counters that workers advance and the main thread's follower reads.
#[derive(Default)]
pub(super) struct SharedProgress {
    bytes: AtomicU64,
    files: AtomicUsize,
    current: Mutex<Option<String>>,
}

impl SharedProgress {
    pub(super) fn reported(&self) -> Reported {
        Reported {
            bytes: self.bytes.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
        }
    }

    /// The file a worker started since the last call.
    pub(super) fn take_current(&self) -> Option<String> {
        lock(&self.current).take()
    }

    fn add_bytes(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn add_file(&self) {
        self.files.fetch_add(1, Ordering::Relaxed);
    }

    fn start_file(&self, name: &OsStr) {
        *lock(&self.current) = Some(name.to_string_lossy().into_owned());
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(super) enum Destination {
    /// A new item at this path, which must be free.
    New(PathBuf),
    /// Into the existing folder at this path, without replacing its entries.
    Merge(PathBuf),
}

#[derive(Clone, Copy, Default)]
pub(super) struct Options {
    pub fat_family: bool,
    /// Worked out from the source and destination devices when `None`.
    pub workers: Option<usize>,
}

/// Copies the native item at `source`, and returns what it added to `progress`.
pub(super) async fn copy_local(
    source: PathBuf,
    destination: Destination,
    options: Options,
    progress: Option<Rc<TransferProgressTracker>>,
    cancellable: &gio::Cancellable,
) -> Result<Reported, glib::Error> {
    let copied = following(
        source,
        options,
        progress,
        cancellable,
        move |copy, source| match destination {
            Destination::New(target) => copy_new(copy, source, &target, options.workers),
            Destination::Merge(folder) => merge(copy, source, &folder, options.workers),
        },
    )
    .await?;
    Ok(copied.1)
}

/// Copies the native item at `source` into a hidden sibling of `target`,
/// which Replace publishes.
pub(super) async fn copy_to_stage(
    source: PathBuf,
    target: PathBuf,
    options: Options,
    progress: Option<Rc<TransferProgressTracker>>,
    cancellable: &gio::Cancellable,
) -> Result<StagedSibling, glib::Error> {
    following(
        source,
        options,
        progress,
        cancellable,
        move |copy, source| {
            let staged = copy_stage(copy, source, &target, options.workers)?;
            match copy.check_cancelled() {
                Ok(()) => Ok(staged),
                Err(failure) => Err(copy.discarding(staged, failure)),
            }
        },
    )
    .await
    .map(|(staged, _)| staged)
}

/// Runs `work` on a worker thread while `progress` follows it.
async fn following<T: Send + 'static>(
    source: PathBuf,
    options: Options,
    progress: Option<Rc<TransferProgressTracker>>,
    cancellable: &gio::Cancellable,
    work: impl FnOnce(&Arc<Copy>, &Source) -> Result<T, Failure> + Send + 'static,
) -> Result<(T, Reported), glib::Error> {
    let shared = Arc::new(SharedProgress::default());
    let follower = progress.map(|progress| {
        if let Some(name) = source.file_name() {
            progress.set_current_file(name.to_string_lossy().into_owned());
        }
        progress.follow(&shared)
    });
    let counters = shared.clone();
    let copied = run_cancellable(cancellable, move |cancelled| {
        let (parent, name) = split(&source)?;
        let source = Source { parent, name };
        let copy = Copy::new(
            fs(),
            source.name.clone(),
            cancelled,
            counters,
            true,
            options.fat_family,
        );
        work(&copy, &source).map_err(|failure| failure.into_error(&source.name))
    })
    .await;
    let reported = follower.map_or_else(|| shared.reported(), |follower| follower.stop());
    let copied = copied.map_err(|_| io_error("Copy worker stopped unexpectedly"))??;
    Ok((copied, reported))
}

/// Copies `staged` to `name` beside it, where the mount cannot place the stage.
pub(super) async fn copy_from_stage(
    staged: &StagedSibling,
    name: OsString,
    cancellable: &gio::Cancellable,
) -> Result<(), glib::Error> {
    let source = Source {
        parent: staged.parent.clone(),
        name: staged.name.clone(),
    };
    let identity = staged.identity;
    let copied = run_cancellable(cancellable, move |cancelled| {
        let copy = Copy::new(fs(), name.clone(), cancelled, Arc::default(), false, false);
        let parent = source.parent.clone();
        from_stage(&copy, &source, identity, &parent, &name)
            .map_err(|failure| failure.into_error(&name))
    })
    .await;
    copied.map_err(|_| io_error("Copy worker stopped unexpectedly"))?
}

/// Gives the owner back the folders a stage locked them out of, so the stage
/// can be removed.
pub(super) fn unlock_stage(staged: &StagedSibling) {
    if staged.restrictive.is_empty() {
        return;
    }
    let fs = fs();
    let Ok(root) = fs.open_dir(Entry::new(&*staged.parent, &staged.name)) else {
        return;
    };
    if fs
        .stat_fd(root.as_fd())
        .is_ok_and(|stat| stat.identity == staged.identity)
    {
        let stable = stable_inodes(Mount::new(fs, root.as_fd()).family);
        restrictive::loosen(fs, root.as_fd(), &staged.restrictive, stable);
    }
}

/// An item to copy, by name in its pinned parent.
struct Source {
    parent: Arc<OwnedFd>,
    name: OsString,
}

impl Source {
    fn entry(&self) -> Entry<'_> {
        Entry::new(&*self.parent, &self.name)
    }
}

fn split(path: &Path) -> Result<(Arc<OwnedFd>, OsString), glib::Error> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io_error(crate::i18n::tr("Invalid local filesystem target")));
    };
    let parent = open_local_parent_directory(parent).map_err(io_error)?;
    Ok((Arc::new(parent), name.to_os_string()))
}

fn copy_new(
    copy: &Arc<Copy>,
    source: &Source,
    target: &Path,
    workers: Option<usize>,
) -> Result<(), Failure> {
    let (parent, name) = split(target).map_err(Failure::Error)?;
    let stat = copy.fs.stat(source.entry(), false)?;
    match stat.kind {
        FileType::RegularFile => FileCopy::new(copy, parent.as_fd())
            .copy_from(source.entry(), FileTarget::Final(&name))
            .map(drop),
        FileType::Symlink => {
            copy.check_cancelled()?;
            let mount = Mount::new(copy.fs, parent.as_fd());
            tree::copy_link(
                copy,
                source.entry(),
                Entry::new(&*parent, &name),
                &mount,
                None,
            )
        }
        FileType::Directory => {
            let workers = workers.unwrap_or_else(|| worker_count(source, &parent));
            if placement::can_place(copy.fs, parent.as_fd(), true) == Some(false) {
                return direct_folder(copy, source, stat.identity, &parent, &name, workers);
            }
            let staged = stage_folder(copy, source, stat.identity, &parent, COPY_PREFIX, workers)?;
            publish_folder(copy, staged, &name, workers)
        }
        _ => Err(Failure::NotCopyable),
    }
}

fn copy_stage(
    copy: &Arc<Copy>,
    source: &Source,
    target: &Path,
    workers: Option<usize>,
) -> Result<StagedSibling, Failure> {
    let (parent, _) = split(target).map_err(Failure::Error)?;
    let stat = copy.fs.stat(source.entry(), false)?;
    match stat.kind {
        FileType::RegularFile => {
            let (name, identity) = FileCopy::new(copy, parent.as_fd())
                .copy_from(source.entry(), FileTarget::Hidden)?;
            Ok(StagedSibling {
                parent,
                name,
                identity,
                restrictive: Vec::new(),
            })
        }
        FileType::Symlink => {
            copy.check_cancelled()?;
            let link = tree::read_link(copy.fs, source.entry())?;
            let staged = StagedSibling::create(parent, STAGE_PREFIX, StageContent::Symlink(link))?;
            // sshfs sets a link's times on what it points to.
            if Mount::new(copy.fs, staged.parent.as_fd()).family != Family::Fuse {
                let link = Entry::new(&*staged.parent, &staged.name);
                if let Err(error) =
                    best_effort(copy.fs.set_link_times(link, stat.atime, stat.mtime))
                {
                    return Err(copy.discarding(staged, error.into()));
                }
            }
            copy.add_file(stat.size);
            Ok(staged)
        }
        FileType::Directory => {
            let workers = workers.unwrap_or_else(|| worker_count(source, &parent));
            stage_folder(copy, source, stat.identity, &parent, STAGE_PREFIX, workers)
        }
        _ => Err(Failure::NotCopyable),
    }
}

fn worker_count(source: &Source, target: &OwnedFd) -> usize {
    let available = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let rotational = [source.parent.as_fd(), target.as_fd()]
        .into_iter()
        .any(|fd| local_device_is_rotational(fd) == Some(true));
    bounded_local_worker_count(available, rotational)
}

/// Copies the folder `source` into a fresh hidden stage in `parent`.
fn stage_folder(
    copy: &Arc<Copy>,
    source: &Source,
    expected: LocalFileIdentity,
    parent: &Arc<OwnedFd>,
    prefix: &str,
    workers: usize,
) -> Result<StagedSibling, Failure> {
    copy.check_cancelled()?;
    let mut staged = StagedSibling::create(parent.clone(), prefix, StageContent::Directory)?;
    let root = Root {
        parent: parent.clone(),
        name: staged.name.clone(),
        identity: staged.identity,
    };
    match fill(copy, source, expected, &root, workers, None) {
        Ok(restrictive) => {
            staged.restrictive = restrictive;
            Ok(staged)
        }
        Err(failure) => Err(copy.discarding(staged, failure)),
    }
}

/// Puts a folder stage at `name`, or copies it there where the mount cannot
/// place it.
fn publish_folder(
    copy: &Arc<Copy>,
    staged: StagedSibling,
    name: &OsStr,
    workers: usize,
) -> Result<(), Failure> {
    if let Err(failure) = copy.check_cancelled() {
        return Err(copy.discarding(staged, failure));
    }
    let placed = stage::publish(
        copy.fs,
        staged.parent.as_fd(),
        &staged.name,
        staged.identity,
        name,
    );
    match placed {
        Ok(_) => Ok(()),
        // Either name may hold the copy, so neither is removed.
        Err(PlaceError::Unconfirmed) => Err(Failure::Place(PlaceError::Unconfirmed)),
        Err(PlaceError::Unsupported) => {
            let from = copy.uncounted();
            let source = Source {
                parent: staged.parent.clone(),
                name: staged.name.clone(),
            };
            let copied = direct_folder(
                &from,
                &source,
                staged.identity,
                &staged.parent,
                name,
                workers,
            );
            let discarded = copy.discard(staged);
            match (copied, discarded) {
                (Ok(()), Ok(())) => Ok(()),
                (Ok(()), Err(error)) => Err(Failure::Error(io_error(rust_i18n::t!(
                    "The item was copied, but its staging copy could not be removed: %{error}",
                    error = crate::services::gio_error_detail(&error)
                )))),
                (Err(failure), discarded) => Err(copy.after_cleanup(failure, discarded)),
            }
        }
        Err(error) => Err(copy.discarding(staged, Failure::Place(error))),
    }
}

/// A folder this copy made, which only it writes into.
struct Root {
    parent: Arc<OwnedFd>,
    name: OsString,
    identity: LocalFileIdentity,
}

/// Copies the folder `source` into the empty folder `root`, links the later
/// names of linked files, then gives the folders that would lock their owner
/// out their exact modes. `created` records each entry made beneath `root`.
fn fill(
    copy: &Arc<Copy>,
    source: &Source,
    expected: LocalFileIdentity,
    root: &Root,
    workers: usize,
    created: Option<&Arc<Created>>,
) -> Result<Vec<Restrictive>, Failure> {
    let fs = copy.fs;
    let parent = fs.stat_fd(root.parent.as_fd())?;
    let mount = Mount::new(fs, root.parent.as_fd());
    let folder = tree::Folder {
        source: (source.parent.clone(), source.name.clone(), Some(expected)),
        target: (root.parent.clone(), root.name.clone(), root.identity),
        parent,
        path: PathBuf::new(),
        naming: tree::Naming::Decide,
        mount,
        link_scope: 0,
        completion: None,
    };
    let tree::Walked {
        mut restrictive,
        links,
    } = tree::walk(copy, workers, created.cloned(), |tree| {
        tree.probe_links_in(&mount, root.parent.clone());
        tree::TreeJob::Folder(tree, folder)
    })
    .map_err(Failure::Error)?;
    if restrictive.is_empty() && links.is_empty() {
        return Ok(restrictive);
    }
    let opened = fs.open_dir(Entry::new(&*root.parent, &root.name))?;
    if !same_entry(&fs.stat_fd(opened.as_fd())?, root.identity, mount.family) {
        return Err(Failure::Place(PlaceError::Changed));
    }
    links.make(copy, opened.as_fd(), mount.family, created.map(|created| &**created))?;
    let stable = stable_inodes(mount.family);
    if let Err(error) = restrictive::tighten(fs, opened.as_fd(), &mut restrictive, stable) {
        restrictive::loosen(fs, opened.as_fd(), &restrictive, stable);
        return Err(error.into());
    }
    Ok(restrictive)
}

/// Entries a copy made beneath a root at its final name, by path beneath
/// it, so a failed copy removes only what it made.
#[derive(Default)]
struct Created(Mutex<Vec<(PathBuf, LocalFileIdentity)>>);

impl Created {
    fn add(&self, path: PathBuf, identity: LocalFileIdentity) {
        lock(&self.0).push((path, identity));
    }

    /// Removes each entry still as it was made, deepest first, then `root`
    /// unless something else is left in it.
    fn remove(&self, fs: &dyn FsOps, root: &Root, family: Family) -> Result<(), glib::Error> {
        let root_entry = Entry::new(&*root.parent, &root.name);
        let Ok(dir) = fs.open_dir(root_entry) else {
            return Ok(());
        };
        if !fs
            .stat_fd(dir.as_fd())
            .is_ok_and(|stat| same_entry(&stat, root.identity, family))
        {
            return Ok(());
        }
        let mut made = std::mem::take(&mut *lock(&self.0));
        made.sort_by(|(left, _), (right, _)| {
            let depth = |path: &Path| path.components().count();
            depth(right)
                .cmp(&depth(left))
                .then_with(|| left.parent().cmp(&right.parent()))
        });
        let mut folder: Option<(&Path, OwnedFd)> = None;
        for (path, identity) in &made {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                continue;
            };
            if folder.as_ref().is_none_or(|(opened, _)| *opened != parent) {
                folder = fs
                    .open_dir(Entry {
                        dir: dir.as_fd(),
                        name: restrictive::beneath(parent),
                    })
                    .ok()
                    .map(|fd| (parent, fd));
            }
            if let Some((_, fd)) = &folder {
                let flags = if identity.kind == FileType::Directory {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                };
                placement::remove_if_unchanged(fs, Entry::new(fd, name), *identity, flags);
            }
        }
        drop((folder, dir));
        match fs.stat(root_entry, true) {
            Ok(current) if current.identity == root.identity => fs
                .unlink(root_entry, AtFlags::REMOVEDIR)
                .map_err(|error| io_error(os_error_detail(error))),
            _ => Ok(()),
        }
    }
}

/// Copies the folder `source` to `name`, making each entry at its final
/// name, and removes what it made if the copy fails.
fn direct_folder(
    copy: &Arc<Copy>,
    source: &Source,
    expected: LocalFileIdentity,
    parent: &Arc<OwnedFd>,
    name: &OsStr,
    workers: usize,
) -> Result<(), Failure> {
    let fs = copy.fs;
    copy.check_cancelled()?;
    let target = Entry::new(&**parent, name);
    if let Err(error) = fs.mkdir(target, Mode::from_raw_mode(0o700)) {
        return Err(if tree::taken(fs, target, error) {
            Failure::Place(PlaceError::Exists)
        } else {
            error.into()
        });
    }
    let identity = fs
        .open_dir(target)
        .and_then(|fd| fs.stat_fd(fd.as_fd()))?
        .identity;
    let root = Root {
        parent: parent.clone(),
        name: name.to_os_string(),
        identity,
    };
    let created = Arc::new(Created::default());
    fill(copy, source, expected, &root, workers, Some(&created))
        .map(drop)
        .map_err(|failure| {
            let family = Mount::new(fs, parent.as_fd()).family;
            let removed = created.remove(fs, &root, family);
            copy.after_cleanup(failure, removed)
        })
}

/// The item a stage holds, copied to `name` beside it.
fn from_stage(
    copy: &Arc<Copy>,
    source: &Source,
    identity: LocalFileIdentity,
    parent: &Arc<OwnedFd>,
    name: &OsStr,
) -> Result<(), Failure> {
    match identity.kind {
        FileType::Directory => {
            let workers = worker_count(source, parent);
            direct_folder(copy, source, identity, parent, name, workers)
        }
        FileType::RegularFile => {
            let parent_stat = copy.fs.stat_fd(parent.as_fd())?;
            FileCopy::new(copy, parent.as_fd())
                .create_from(source.entry(), identity, None, name, &parent_stat)
                .map(drop)
        }
        // The ladder recreates symlinks itself.
        _ => Err(Failure::Place(PlaceError::Unsupported)),
    }
}

fn merge(
    copy: &Arc<Copy>,
    source: &Source,
    folder: &Path,
    workers: Option<usize>,
) -> Result<(), Failure> {
    let (parent, name) = split(folder).map_err(Failure::Error)?;
    let workers = workers.unwrap_or_else(|| worker_count(source, &parent));
    let merged = tree::Merged {
        source: (source.parent.clone(), source.name.clone()),
        target: (parent.clone(), name.clone(), None),
        path: PathBuf::new(),
    };
    let tree::Walked {
        mut restrictive,
        links,
    } = tree::walk(copy, workers, None, |tree| tree::TreeJob::Merge(tree, merged))
        .map_err(Failure::Error)?;
    if restrictive.is_empty() && links.is_empty() {
        return Ok(());
    }
    let root = copy.fs.open_dir(Entry::new(&*parent, &name))?;
    let family = Mount::new(copy.fs, root.as_fd()).family;
    links.make(copy, root.as_fd(), family, None)?;
    restrictive::tighten(
        copy.fs,
        root.as_fd(),
        &mut restrictive,
        stable_inodes(family),
    )
    .map_err(Failure::Os)
}

/// Why a copy stopped.
enum Failure {
    Cancelled,
    Os(Errno),
    Place(PlaceError),
    Changed,
    NotCopyable,
    Error(glib::Error),
}

impl From<Errno> for Failure {
    fn from(error: Errno) -> Self {
        Self::Os(error)
    }
}

impl From<PlaceError> for Failure {
    fn from(error: PlaceError) -> Self {
        match error {
            PlaceError::Io(error) => Self::Os(error),
            error => Self::Place(error),
        }
    }
}

impl Failure {
    /// The failure as an error about `name`, a child of the item.
    fn named(self, name: &OsStr) -> Self {
        Self::Error(self.into_error(name))
    }

    fn into_error(self, name: &OsStr) -> glib::Error {
        let name = name.to_string_lossy();
        match self {
            Self::Cancelled => cancelled_local_operation(),
            Self::Os(error) => {
                let message = rust_i18n::t!(
                    "Could not copy %{name}: %{error}",
                    name = name,
                    error = os_error_detail(error)
                );
                match error {
                    Errno::NOSPC | Errno::DQUOT => {
                        glib::Error::new(gio::IOErrorEnum::NoSpace, &message)
                    }
                    _ => io_error(message),
                }
            }
            Self::Place(error) => placement_error(error),
            Self::Changed => io_error(rust_i18n::t!(
                "%{name} changed while it was being copied",
                name = name
            )),
            Self::NotCopyable => io_error(rust_i18n::t!(
                "Cannot copy %{name}: it is not a regular file, directory, or symbolic link",
                name = name
            )),
            Self::Error(error) => error,
        }
    }
}

/// The device or the connection is failing, so no step may skip the error.
fn is_fatal(error: Errno) -> bool {
    matches!(
        error,
        Errno::IO
            | Errno::STALE
            | Errno::NOTCONN
            | Errno::CONNABORTED
            | Errno::CONNRESET
            | Errno::TIMEDOUT
            | Errno::HOSTDOWN
            | Errno::HOSTUNREACH
            | Errno::NETDOWN
            | Errno::NETUNREACH
    )
}

/// Only device, handle and connection failures stop a metadata step.
fn best_effort(result: Result<(), Errno>) -> Result<(), Errno> {
    match result {
        Err(error) if is_fatal(error) => Err(error),
        _ => Ok(()),
    }
}

fn is_unsupported(error: Errno) -> bool {
    matches!(error, Errno::OPNOTSUPP | Errno::NOTTY | ENOTSUPP)
}

/// FUSE daemons without `use_ino` number a node afresh once the kernel has
/// evicted it.
fn stable_inodes(family: Family) -> bool {
    family != Family::Fuse
}

/// Whether `current` is still the entry recorded as `identity`.
fn same_entry(current: &EntryStat, identity: LocalFileIdentity, family: Family) -> bool {
    if stable_inodes(family) {
        current.identity == identity
    } else {
        current.kind == identity.kind
    }
}

/// A mount with what was learned about it, keyed as placement keys it.
#[derive(Clone, Copy)]
struct Mount {
    key: MountKey,
    family: Family,
}

impl Mount {
    fn new(fs: &dyn FsOps, dir: BorrowedFd<'_>) -> Self {
        let key = fs.mount_key(dir);
        let family = mount_capabilities(key).family.unwrap_or_else(|| {
            let family = fs
                .mount_type(dir)
                .map_or(Family::Other, |fs_type| family(&fs_type));
            learn_mount_capability(key, |capabilities| capabilities.family = Some(family));
            family
        });
        Self { key, family }
    }

    fn capabilities(&self) -> MountCapabilities {
        mount_capabilities(self.key)
    }

    fn learn(&self, learn: impl FnOnce(&mut MountCapabilities)) {
        learn_mount_capability(self.key, learn);
    }
}

const TMPFILE_LINK_UNKNOWN: u8 = 0;
const TMPFILE_LINK_EMPTY_PATH: u8 = 1;
const TMPFILE_LINK_PROC: u8 = 2;
const TMPFILE_LINK_UNAVAILABLE: u8 = 3;

/// How this process links unnamed files: `AT_EMPTY_PATH` needs Linux 6.10
/// unprivileged, and the `/proc` form needs `/proc`.
static TMPFILE_LINK: AtomicU8 = AtomicU8::new(TMPFILE_LINK_UNKNOWN);

#[cfg(test)]
pub(super) fn forget_learned_copy_support() {
    TMPFILE_LINK.store(TMPFILE_LINK_UNKNOWN, Ordering::Relaxed);
    data::forget_learned();
}

fn link_tmpfile(fs: &dyn FsOps, fd: BorrowedFd<'_>, to: Entry<'_>) -> Result<(), Errno> {
    let known = TMPFILE_LINK.load(Ordering::Relaxed);
    if known != TMPFILE_LINK_PROC {
        match fs.link_fd(fd, to, false) {
            Ok(()) => {
                TMPFILE_LINK.store(TMPFILE_LINK_EMPTY_PATH, Ordering::Relaxed);
                return Ok(());
            }
            // Once the plain form has worked, ENOENT is about the folder.
            Err(Errno::NOENT) if known == TMPFILE_LINK_UNKNOWN => {}
            Err(error) => return Err(error),
        }
    }
    match fs.link_fd(fd, to, true) {
        Ok(()) => {
            TMPFILE_LINK.store(TMPFILE_LINK_PROC, Ordering::Relaxed);
            Ok(())
        }
        // Unless the folder itself was removed meanwhile.
        Err(Errno::NOENT)
            if known == TMPFILE_LINK_UNKNOWN
                && fs.stat_fd(to.dir).is_ok_and(|folder| folder.nlink > 0) =>
        {
            TMPFILE_LINK.store(TMPFILE_LINK_UNAVAILABLE, Ordering::Relaxed);
            Err(Errno::NOENT)
        }
        Err(error) => Err(error),
    }
}

/// What every worker of one copy shares.
struct Copy {
    fs: &'static dyn FsOps,
    /// The name errors about the item itself give.
    item: OsString,
    cancelled: Arc<AtomicBool>,
    progress: Arc<SharedProgress>,
    /// False for a copy from a stage, whose bytes were counted already.
    counted: bool,
    fat_family: bool,
    data: Mutex<Vec<Data>>,
    /// Names refused one by one, such as `security.*` as non-root.
    refused_xattrs: Mutex<HashSet<OsString>>,
    /// The target refused `mknodat`, so claims create and close instead.
    nodes_refused: AtomicBool,
}

impl Copy {
    fn new(
        fs: &'static dyn FsOps,
        item: OsString,
        cancelled: Arc<AtomicBool>,
        progress: Arc<SharedProgress>,
        counted: bool,
        fat_family: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            item,
            cancelled,
            progress,
            counted,
            fat_family,
            data: Mutex::default(),
            refused_xattrs: Mutex::default(),
            nodes_refused: AtomicBool::new(false),
        })
    }

    /// The same copy repeating bytes it already counted, as from a stage.
    fn uncounted(&self) -> Arc<Self> {
        Self::new(
            self.fs,
            self.item.clone(),
            self.cancelled.clone(),
            self.progress.clone(),
            false,
            false,
        )
    }

    /// Counts a completed file, with the `bytes` it made without copying
    /// data, unless this copy repeats one already counted.
    fn add_file(&self, bytes: u64) {
        if self.counted {
            self.progress.add_bytes(bytes);
            self.progress.add_file();
        }
    }

    fn check_cancelled(&self) -> Result<(), Failure> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Failure::Cancelled);
        }
        Ok(())
    }

    /// Runs `work` with a worker's reusable data buffer.
    fn with_data<T>(&self, work: impl FnOnce(&mut Data) -> T) -> T {
        let mut data = lock(&self.data).pop().unwrap_or_default();
        let result = work(&mut data);
        lock(&self.data).push(data);
        result
    }

    /// Copies all of `source`, counting the bytes as progress when `counted`.
    fn copy_data(
        &self,
        data: &mut Data,
        target_mount: &Mount,
        (source, source_stat): (BorrowedFd<'_>, &EntryStat),
        target: BorrowedFd<'_>,
        parent: &EntryStat,
        counted: bool,
    ) -> Result<(), Failure> {
        let route = data::Route::new(self.fs, target_mount, source, source_stat, parent);
        let progress = (counted && self.counted).then_some(&*self.progress);
        data.copy(
            self.fs,
            &self.cancelled,
            target_mount,
            &route,
            (source, target),
            source_stat.size,
            |bytes| {
                if let Some(progress) = progress {
                    progress.add_bytes(bytes);
                }
            },
        )
    }

    fn apply_metadata(
        &self,
        fd: BorrowedFd<'_>,
        created: &EntryStat,
        snapshot: &Snapshot,
        parent: &EntryStat,
        mount: &Mount,
    ) -> Result<(), Failure> {
        snapshot
            .apply(self.fs, fd, created, parent, mount, &self.refused_xattrs)
            .map_err(Failure::Os)
    }

    /// NFSv4 write delegations set the times at close, and FUSE daemons with
    /// a writeback cache and some SMB servers may too, so a closed file gets
    /// its times again: always on NFSv4, and where a check finds them lost.
    fn settle_times(
        &self,
        entry: Entry<'_>,
        identity: LocalFileIdentity,
        snapshot: &Snapshot,
        family: Family,
    ) -> Result<(), Failure> {
        let lost = match family {
            Family::Nfs4 => true,
            Family::Fuse | Family::Smb => match self.fs.stat(entry, true) {
                Ok(current) => !snapshot.times_match(&current),
                Err(error) if is_fatal(error) => return Err(error.into()),
                Err(_) => false,
            },
            _ => false,
        };
        if !lost {
            return Ok(());
        }
        let fs = self.fs;
        let reopened = match fs.open_source(entry, false) {
            // A mode that gives the owner no read, as a copy of another
            // user's file readable through its group or others may have.
            Err(Errno::ACCESS) => fs.open_write(entry).and_then(|fd| {
                let stat = fs.stat_fd(fd.as_fd())?;
                Ok(Some((fd, stat)))
            }),
            opened => opened,
        };
        let reopened = match reopened {
            Ok(Some((fd, stat))) if same_entry(&stat, identity, family) => fd,
            // Setting times by name needs only ownership.
            Err(Errno::ACCESS) => {
                return match fs.stat(entry, true) {
                    Ok(current) if same_entry(&current, identity, family) => {
                        Ok(snapshot.apply_times_at(fs, entry)?)
                    }
                    Err(error) if is_fatal(error) => Err(error.into()),
                    _ => Ok(()),
                };
            }
            Err(error) if is_fatal(error) => return Err(error.into()),
            _ => return Ok(()),
        };
        snapshot.apply_times(fs, reopened.as_fd())?;
        match fs.close(reopened) {
            Err(error) if is_fatal(error) => Err(error.into()),
            _ => Ok(()),
        }
    }

    /// Removes a folder stage this copy made, giving its locked folders back
    /// to the owner first.
    fn discard(&self, staged: StagedSibling) -> Result<(), glib::Error> {
        unlock_stage(&staged);
        let roots = vec![PinnedEntry {
            parent: staged.parent,
            name: staged.name,
            identity: Some(staged.identity),
        }];
        let workers = local_delete_worker_count(&roots);
        parallel_delete_local_blocking_with_workers(
            roots,
            Arc::new(AtomicBool::new(false)),
            workers,
        )
        .map_err(io_error)
    }

    fn discarding(&self, staged: StagedSibling, failure: Failure) -> Failure {
        let discarded = self.discard(staged);
        self.after_cleanup(failure, discarded)
    }

    fn after_cleanup(&self, failure: Failure, cleanup: Result<(), glib::Error>) -> Failure {
        match cleanup {
            Ok(()) => failure,
            Err(cleanup) => Failure::Error(copy_failure_after_cleanup(
                failure.into_error(&self.item),
                Err(cleanup),
            )),
        }
    }
}

/// An open source file and its snapshot from that descriptor.
struct Opened {
    fd: OwnedFd,
    stat: EntryStat,
}

/// `name` could not be opened as what it was listed as.
fn changed(name: &OsStr, error: Errno) -> Failure {
    Failure::Error(io_error(rust_i18n::t!(
        "%{name} changed while it was being copied: %{error}",
        name = name.to_string_lossy(),
        error = os_error_detail(error)
    )))
}

/// The regular file just listed at `at`, opened without following it.
fn open_file(fs: &dyn FsOps, at: Entry<'_>) -> Result<Opened, Failure> {
    match fs.open_source(at, true) {
        Ok(Some((fd, stat))) => Ok(Opened { fd, stat }),
        Ok(None) => Err(Failure::Changed),
        Err(error) => Err(changed(at.name, error)),
    }
}

/// Where a top-level file ends up.
#[derive(Clone, Copy)]
enum FileTarget<'a> {
    Final(&'a OsStr),
    /// Under a hidden `.strata-replacement-*` name, left unpublished.
    Hidden,
}

impl FileTarget<'_> {
    fn stage_prefix(self) -> &'static str {
        match self {
            Self::Final(_) => COPY_PREFIX,
            Self::Hidden => STAGE_PREFIX,
        }
    }
}

/// A stage this copy created, which only it may remove.
struct Stage {
    name: OsString,
    created: EntryStat,
}

/// One top-level file copied into `dir`.
struct FileCopy<'a> {
    copy: &'a Copy,
    dir: BorrowedFd<'a>,
    target: Mount,
    data: Data,
}

impl<'a> FileCopy<'a> {
    fn new(copy: &'a Copy, dir: BorrowedFd<'a>) -> Self {
        Self {
            copy,
            dir,
            target: Mount::new(copy.fs, dir),
            data: Data::default(),
        }
    }

    fn at<'b>(&self, name: &'b OsStr) -> Entry<'b>
    where
        'a: 'b,
    {
        Entry {
            dir: self.dir,
            name,
        }
    }

    /// Returns the name the copy took and its identity there.
    fn copy_from(
        &mut self,
        at: Entry<'_>,
        target: FileTarget<'_>,
    ) -> Result<(OsString, LocalFileIdentity), Failure> {
        let fs = self.copy.fs;
        self.copy.check_cancelled()?;
        let source = open_file(fs, at)?;
        let parent = fs.stat_fd(self.dir)?;
        if stage::tmpfile_allowed(self.target.family)
            && self.target.capabilities().tmpfile != Some(false)
            && TMPFILE_LINK.load(Ordering::Relaxed) != TMPFILE_LINK_UNAVAILABLE
        {
            match fs.open_tmpfile(self.dir, Mode::from_raw_mode(0o600)) {
                Ok(fd) => {
                    self.target
                        .learn(|capabilities| capabilities.tmpfile = Some(true));
                    return self.through_tmpfile(&source, fd, &parent, target);
                }
                // Before Linux 3.11 the flag reads as O_DIRECTORY.
                Err(error) if is_unsupported(error) || error == Errno::ISDIR => {
                    self.target
                        .learn(|capabilities| capabilities.tmpfile = Some(false));
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.through_named_stage(&source, &parent, target)
    }

    fn link(&self, tmpfile: BorrowedFd<'_>, target: FileTarget<'_>) -> Result<OsString, Errno> {
        let fs = self.copy.fs;
        match target {
            FileTarget::Final(name) => {
                link_tmpfile(fs, tmpfile, self.at(name)).map(|()| name.to_os_string())
            }
            FileTarget::Hidden => placement::reserve_hidden_name(STAGE_PREFIX, |name| {
                link_tmpfile(fs, tmpfile, self.at(name)).map_err(|error| {
                    match placement::file_creation_error(error) {
                        PlaceError::Exists => PlaceError::Exists,
                        _ => PlaceError::Io(error),
                    }
                })
            })
            .map(|(name, ())| name)
            .map_err(|error| match error {
                PlaceError::Io(error) => error,
                _ => Errno::EXIST,
            }),
        }
    }

    fn through_tmpfile(
        &mut self,
        source: &Opened,
        tmpfile: OwnedFd,
        parent: &EntryStat,
        target: FileTarget<'_>,
    ) -> Result<(OsString, LocalFileIdentity), Failure> {
        let fs = self.copy.fs;
        self.copy.copy_data(
            &mut self.data,
            &self.target,
            (source.fd.as_fd(), &source.stat),
            tmpfile.as_fd(),
            parent,
            true,
        )?;
        let snapshot = Snapshot::read(fs, source.fd.as_fd(), &source.stat)?;
        let created = fs.stat_fd(tmpfile.as_fd())?;
        self.copy
            .apply_metadata(tmpfile.as_fd(), &created, &snapshot, parent, &self.target)?;
        self.copy.check_cancelled()?;
        let name = match self.link(tmpfile.as_fd(), target) {
            Ok(name) => name,
            Err(error) => {
                return match placement::file_creation_error(error) {
                    PlaceError::Exists => Err(Failure::Place(PlaceError::Exists)),
                    _ if matches!(error, Errno::NOSPC | Errno::DQUOT) || is_fatal(error) => {
                        Err(error.into())
                    }
                    // The written bytes are copied out on the same device, so
                    // the source is never read twice.
                    _ => self
                        .through_stage_from_tmpfile(tmpfile, &created, &snapshot, parent, target),
                };
            }
        };
        if let Err(error) = fs.close(tmpfile) {
            placement::remove_if_unchanged(fs, self.at(&name), created.identity, AtFlags::empty());
            return Err(error.into());
        }
        self.copy.add_file(0);
        Ok((name, created.identity))
    }

    fn through_stage_from_tmpfile(
        &mut self,
        tmpfile: OwnedFd,
        tmpfile_stat: &EntryStat,
        snapshot: &Snapshot,
        parent: &EntryStat,
        target: FileTarget<'_>,
    ) -> Result<(OsString, LocalFileIdentity), Failure> {
        let (fd, stage) = self.create_stage(target)?;
        let copied = self
            .copy
            .copy_data(
                &mut self.data,
                &self.target,
                (tmpfile.as_fd(), tmpfile_stat),
                fd.as_fd(),
                parent,
                false,
            )
            .and_then(|()| {
                self.copy
                    .apply_metadata(fd.as_fd(), &stage.created, snapshot, parent, &self.target)
            });
        drop(tmpfile);
        match copied {
            Ok(()) => self.finish_stage(fd, stage, snapshot, parent, target),
            Err(failure) => {
                drop(fd);
                Err(self.discard(&stage, failure))
            }
        }
    }

    fn through_named_stage(
        &mut self,
        source: &Opened,
        parent: &EntryStat,
        target: FileTarget<'_>,
    ) -> Result<(OsString, LocalFileIdentity), Failure> {
        let fs = self.copy.fs;
        let (fd, stage) = self.create_stage(target)?;
        let copied = self
            .copy
            .copy_data(
                &mut self.data,
                &self.target,
                (source.fd.as_fd(), &source.stat),
                fd.as_fd(),
                parent,
                true,
            )
            .and_then(|()| Ok(Snapshot::read(fs, source.fd.as_fd(), &source.stat)?))
            .and_then(|snapshot| {
                self.copy.apply_metadata(
                    fd.as_fd(),
                    &stage.created,
                    &snapshot,
                    parent,
                    &self.target,
                )?;
                Ok(snapshot)
            });
        match copied {
            Ok(snapshot) => self.finish_stage(fd, stage, &snapshot, parent, target),
            Err(failure) => {
                drop(fd);
                Err(self.discard(&stage, failure))
            }
        }
    }

    fn create_stage(&self, target: FileTarget<'_>) -> Result<(OwnedFd, Stage), Failure> {
        let (name, (fd, created)) =
            stage::create_file(self.copy.fs, self.dir, target.stage_prefix())?;
        Ok((fd, Stage { name, created }))
    }

    /// Closes the stage and settles its times, then publishes it at a final
    /// name; a failure discards it.
    fn finish_stage(
        &mut self,
        fd: OwnedFd,
        stage: Stage,
        snapshot: &Snapshot,
        parent: &EntryStat,
        target: FileTarget<'_>,
    ) -> Result<(OsString, LocalFileIdentity), Failure> {
        let fs = self.copy.fs;
        let settled = fs
            .close(fd)
            .map_err(Failure::Os)
            .and_then(|()| {
                self.copy.settle_times(
                    self.at(&stage.name),
                    stage.created.identity,
                    snapshot,
                    self.target.family,
                )
            })
            .and_then(|()| self.copy.check_cancelled());
        if let Err(failure) = settled {
            return Err(self.discard(&stage, failure));
        }
        let FileTarget::Final(name) = target else {
            self.copy.add_file(0);
            return Ok((stage.name, stage.created.identity));
        };
        let placed = stage::publish(fs, self.dir, &stage.name, stage.created.identity, name);
        let identity = match placed {
            Ok(_) => stage.created.identity,
            // Either name may hold the copy, so neither is removed.
            Err(PlaceError::Unconfirmed) => return Err(Failure::Place(PlaceError::Unconfirmed)),
            Err(PlaceError::Unsupported) => {
                let dir = self.dir;
                let staged = Entry {
                    dir,
                    name: &stage.name,
                };
                let identity = match self.create_from(
                    staged,
                    stage.created.identity,
                    Some(snapshot),
                    name,
                    parent,
                ) {
                    Ok(identity) => identity,
                    Err(failure) => return Err(self.discard(&stage, failure)),
                };
                if let Err(error) = self.remove_stage(&stage) {
                    return Err(Failure::Error(io_error(rust_i18n::t!(
                        "The item was copied, but its staging copy could not be removed: %{error}",
                        error = crate::services::gio_error_detail(&error)
                    ))));
                }
                identity
            }
            Err(error) => return Err(self.discard(&stage, Failure::Place(error))),
        };
        self.copy.add_file(0);
        Ok((name.to_os_string(), identity))
    }

    /// Creates `name` exclusively and fills it from the file at `from`, which
    /// must still be `expected`: a stage the mount cannot place.
    fn create_from(
        &mut self,
        from: Entry<'_>,
        expected: LocalFileIdentity,
        snapshot: Option<&Snapshot>,
        name: &OsStr,
        parent: &EntryStat,
    ) -> Result<LocalFileIdentity, Failure> {
        let fs = self.copy.fs;
        let source = match fs.open_source(from, false)? {
            Some((fd, stat)) if stat.identity == expected => Opened { fd, stat },
            _ => return Err(Failure::Changed),
        };
        let read;
        let snapshot = match snapshot {
            Some(snapshot) => snapshot,
            None => {
                read = Snapshot::read(fs, source.fd.as_fd(), &source.stat)?;
                &read
            }
        };
        let target = self.at(name);
        let fd = fs
            .create_exclusive(target, Mode::from_raw_mode(0o600))
            .map_err(|error| Failure::Place(placement::file_creation_error(error)))?;
        let created = fs.stat_fd(fd.as_fd())?;
        // Takes `fd`, so a failed file is closed before it is removed: NFS
        // renames an open file aside instead.
        let filled = (|fd: OwnedFd| -> Result<(), Failure> {
            self.copy.copy_data(
                &mut self.data,
                &self.target,
                (source.fd.as_fd(), &source.stat),
                fd.as_fd(),
                parent,
                false,
            )?;
            self.copy
                .apply_metadata(fd.as_fd(), &created, snapshot, parent, &self.target)?;
            fs.close(fd)?;
            self.copy
                .settle_times(target, created.identity, snapshot, self.target.family)
        })(fd);
        if filled.is_err() {
            placement::remove_if_unchanged(fs, target, created.identity, AtFlags::empty());
        }
        filled.map(|()| created.identity)
    }

    /// Removes a stage this copy created, unless something else took its name.
    fn remove_stage(&self, stage: &Stage) -> Result<(), glib::Error> {
        let fs = self.copy.fs;
        let entry = self.at(&stage.name);
        match fs.stat(entry, true) {
            Ok(current) if current.identity == stage.created.identity => fs
                .unlink(entry, AtFlags::empty())
                .map_err(|error| io_error(os_error_detail(error))),
            Ok(_) => Err(placement_error(PlaceError::Changed)),
            Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(io_error(os_error_detail(error))),
        }
    }

    fn discard(&self, stage: &Stage, failure: Failure) -> Failure {
        let removed = self.remove_stage(stage);
        self.copy.after_cleanup(failure, removed)
    }
}
