// SPDX-License-Identifier: MIT

//! Copies native items through descriptors on worker threads. A copy never
//! replaces an entry, and shows a partial item under its final name only
//! where the destination cannot place a stage.

mod barrier;
mod data;
mod links;
mod manifest;
mod metadata;
mod restrictive;
pub(super) mod stage;
mod tree;

use std::{
    borrow::Borrow,
    collections::{BTreeSet, HashSet},
    ffi::{OsStr, OsString},
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use gtk::{gio, glib};
use rustix::{
    fs::{AtFlags, FileType, Mode},
    io::Errno,
};

use super::{
    LocalFileIdentity, PinnedEntry, Reported, StageContent, StagedSibling, TransferProgressTracker,
    blocking_join_message, bounded_local_worker_count, cancelled_local_operation,
    copy_failure_after_cleanup,
    fs_ops::{Entry, EntryStat, FsOps, fs},
    io_error, local_delete_worker_count, local_device_is_rotational,
    naming::Taken,
    open_local_parent_directory, os_error_detail, os_error_text,
    parallel_delete_local_blocking_with_workers, placement,
    placement::{COPY_PREFIX, ENOTSUPP, PlaceError, STAGE_PREFIX},
    placement_error, run_blocking, run_cancellable,
};
use crate::{
    adapters::volume::{
        CoarseTimes, Family, FuseFormat, MountCapabilities, MountKey, MountType, family,
        learn_mount_capability, mount_capabilities,
    },
    model::Location,
    services::{
        KeptCause, PlacedSnapshot, RenameCause, RenamedItem, SkippedItem, SpecialKind,
        TransferReport,
    },
};
pub(super) use barrier::Barrier;
use data::Data;
#[cfg(test)]
pub(super) use data::set_kernel_for_test;
pub(super) use manifest::{Manifest, Published, Unlinks};
use manifest::{Placed, Removed};
#[cfg(test)]
pub(super) use manifest::{now as clock_for_test, set_clock_for_test, set_placed_entries_for_test};
use metadata::Snapshot;
pub(super) use restrictive::Restrictive;

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
    New(PathBuf, Taken),
    Merge(PathBuf, Option<Arc<Placing>>),
}

/// The paths a merge records what it places at, as it places it.
pub(super) struct Placing {
    root: PathBuf,
    wanted: HashSet<PathBuf>,
    unwanted: Option<HashSet<PathBuf>>,
    placed: Mutex<Vec<(PathBuf, Placed)>>,
    left: Arc<AtomicUsize>,
}

impl Placing {
    /// `root` is the merged folder. Beneath it, `wanted` are the paths undo
    /// reverts. Where the merge records all it creates, `unwanted` are the
    /// overwritten paths undo leaves. `left` is the operation's
    /// [`placed_budget`].
    pub(super) fn new(
        root: PathBuf,
        wanted: HashSet<PathBuf>,
        unwanted: Option<HashSet<PathBuf>>,
        left: Arc<AtomicUsize>,
    ) -> Arc<Self> {
        Arc::new(Self {
            root,
            wanted,
            unwanted,
            placed: Mutex::default(),
            left,
        })
    }

    /// A GIO copy, which walks into what it creates, records only these.
    pub(super) fn wants(&self, path: &Path) -> bool {
        self.wanted.contains(path)
    }

    /// `path` is where the merge placed `name` of `dir`, beneath its folder,
    /// which may be a name it numbered instead of a wanted one.
    fn placed(
        &self,
        fs: &dyn FsOps,
        (dir, name): (BorrowedFd<'_>, &OsStr),
        path: &Path,
        cancelled: &AtomicBool,
    ) {
        let path = self.root.join(path);
        let wanted = match &self.unwanted {
            Some(unwanted) => !unwanted.contains(&path),
            None => self.wants(&path),
        };
        if wanted {
            self.record(fs, (dir, name), path, cancelled);
        }
    }

    fn record(
        &self,
        fs: &dyn FsOps,
        (dir, name): (BorrowedFd<'_>, &OsStr),
        path: PathBuf,
        cancelled: &AtomicBool,
    ) {
        match manifest::placed(fs, (dir, name), cancelled, &self.left) {
            Ok(placed) => lock(&self.placed).push((path, placed)),
            Err(failure) => {
                let error = failure.into_error(name);
                tracing::warn!(?path, %error, "could not record a placed item");
            }
        }
    }

    pub(super) fn take(&self) -> Vec<(PathBuf, PlacedSnapshot)> {
        std::mem::take(&mut *lock(&self.placed))
            .into_iter()
            .map(|(path, placed)| (path, PlacedSnapshot::new(placed)))
            .collect()
    }
}

/// Records what a merge copied to `path` some other way.
pub(super) async fn record_placed(
    placing: Arc<Placing>,
    path: PathBuf,
    cancellable: &gio::Cancellable,
) {
    let recorded = run_cancellable(cancellable, move |cancelled| {
        let (parent, name) = split(&path)?;
        placing.record(fs(), (parent.as_fd(), &name), path.clone(), &cancelled);
        Ok::<_, glib::Error>(())
    })
    .await;
    if let Ok(Err(error)) = recorded {
        tracing::warn!(%error, "could not record a placed item");
    }
}

/// The entries all records of what one operation placed may hold.
pub(super) fn placed_budget() -> Arc<AtomicUsize> {
    Arc::new(manifest::placed_entries())
}

/// What is at `path`, recorded as an operation just put it there, within
/// what is `left` of its [`placed_budget`]; nothing if cancelled meanwhile.
pub(super) async fn placed(
    path: PathBuf,
    left: Arc<AtomicUsize>,
    cancellable: &gio::Cancellable,
) -> Option<PlacedSnapshot> {
    let recorded = run_cancellable(cancellable, move |cancelled| {
        let (parent, name) = split(&path).ok()?;
        placed_within(fs(), (parent.as_fd(), &name), &cancelled, &left)
    })
    .await;
    recorded.ok().flatten()
}

pub(super) fn placed_at(
    fs: &dyn FsOps,
    at: (BorrowedFd<'_>, &OsStr),
    cancelled: &AtomicBool,
) -> Option<PlacedSnapshot> {
    placed_within(fs, at, cancelled, &manifest::placed_entries())
}

fn placed_within(
    fs: &dyn FsOps,
    (dir, name): (BorrowedFd<'_>, &OsStr),
    cancelled: &AtomicBool,
    left: &AtomicUsize,
) -> Option<PlacedSnapshot> {
    match manifest::placed(fs, (dir, name), cancelled, left) {
        Ok(placed) => Some(PlacedSnapshot::new(placed)),
        Err(failure) => {
            let error = failure.into_error(name);
            tracing::warn!(?name, %error, "could not record a placed item");
            None
        }
    }
}

/// Deletes what an operation placed at `path` if all of it is unchanged.
pub(super) async fn remove_placed(
    path: PathBuf,
    placed: PlacedSnapshot,
    unlinks: Arc<Unlinks>,
    cancellable: &gio::Cancellable,
) -> Removed {
    let failed = |error| Removed {
        kept: Vec::new(),
        removed: false,
        result: Err(error),
    };
    let removed = run_cancellable(cancellable, move |cancelled| {
        let Some(placed) = placed.get::<Placed>() else {
            return failed(io_error("Unknown placed item record"));
        };
        let Some(folder) = path.parent() else {
            return failed(io_error(crate::i18n::tr("Invalid local filesystem target")));
        };
        let parent = match open_local_parent_directory(folder) {
            Ok(parent) => Arc::new(parent),
            Err(_)
                if std::fs::symlink_metadata(folder)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Removed {
                    kept: Vec::new(),
                    removed: false,
                    result: Ok(()),
                };
            }
            Err(error) => return failed(io_error(error)),
        };
        // Deleting only part of it would not free the path for the original.
        let compared = manifest::compare(
            fs(),
            parent.clone(),
            &placed.root(),
            unlinks.clone(),
            cancelled.clone(),
        );
        if compared.result.is_err() || !compared.kept.is_empty() {
            return compared;
        }
        manifest::remove(fs(), parent, &placed.root(), unlinks, cancelled)
    })
    .await;
    removed.unwrap_or_else(|_| failed(io_error("Delete worker stopped unexpectedly")))
}

pub(super) struct Copied {
    pub name: OsString,
    pub reported: Reported,
    pub manifest: Option<Manifest>,
    /// Barriers of the mounts nested in a durably merged folder.
    pub nested: Vec<Barrier>,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Options {
    pub fat_family: bool,
    pub workers: Option<usize>,
    pub manifest: bool,
    /// What only the copy will hold is deleted once its [`Barrier`] is reached,
    /// so files that barrier cannot reach are synced as written.
    pub durable: bool,
}

pub(super) async fn copy_local(
    source: PathBuf,
    destination: Destination,
    options: Options,
    progress: Option<Rc<TransferProgressTracker>>,
    cancellable: &gio::Cancellable,
) -> Result<Copied, glib::Error> {
    let (Destination::New(target, _) | Destination::Merge(target, _)) = &destination;
    let target = target.clone();
    let merging = matches!(destination, Destination::Merge(..));
    let followed = following(
        Item::Path(source),
        options,
        progress.clone(),
        cancellable,
        move |copy, source| match destination {
            Destination::New(target, taken) => {
                let (parent, name) = split(&target).map_err(Failure::Error)?;
                let new = NewItem::Published(taken);
                let name = copy_new(copy, source, (&parent, &name), new, options.workers)?;
                let published = match source.recorded_in {
                    Some(_) => Some(Published::at(copy.fs, parent, name.clone())?),
                    None => None,
                };
                Ok((name, Vec::new(), published))
            }
            Destination::Merge(folder, placing) => {
                let name = folder.file_name().unwrap_or_default().to_os_string();
                merge(copy, source, &folder, options.workers, placing)
                    .map(|nested| (name, nested, None))
            }
        },
    )
    .await?;
    match followed.value {
        Ok((name, nested, published)) => {
            report(
                progress.as_deref(),
                &target.with_file_name(&name),
                followed.left,
            );
            let mut manifest = followed.manifest;
            if let (Some(manifest), Some(published)) = (&mut manifest, published) {
                manifest.copied_to(published);
            }
            Ok(Copied {
                name,
                reported: followed.reported,
                manifest,
                nested,
            })
        }
        // What a stopped merge placed stays.
        Err(error) => {
            if merging {
                report(progress.as_deref(), &target, followed.left);
            }
            Err(error)
        }
    }
}

pub(super) async fn copy_to_stage(
    source: PathBuf,
    target: PathBuf,
    options: Options,
    progress: Option<Rc<TransferProgressTracker>>,
    cancellable: &gio::Cancellable,
) -> Result<(StagedSibling, Option<Manifest>, TransferReport), glib::Error> {
    let root = target.clone();
    let followed = following(
        Item::Path(source),
        options,
        progress.clone(),
        cancellable,
        move |copy, source| {
            let staged = copy_stage(copy, source, &target, options.workers)?;
            match copy.check_cancelled() {
                Ok(()) => Ok(staged),
                Err(failure) => Err(copy.discarding(staged, failure)),
            }
        },
    )
    .await?;
    let staged = followed.value?;
    Ok((staged, followed.manifest, left_report(&root, followed.left)))
}

/// For a mount that can place neither the item nor a stage. A failed copy
/// removes only what it made.
pub(super) async fn copy_in_place(
    (source, name, expected): (Arc<OwnedFd>, OsString, LocalFileIdentity),
    (parent, target): (Arc<OwnedFd>, PathBuf),
    options: Options,
    cancellable: &gio::Cancellable,
) -> Result<(Manifest, TransferReport), glib::Error> {
    let Some(target_name) = target.file_name().map(OsStr::to_os_string) else {
        return Err(io_error(crate::i18n::tr("Invalid local filesystem target")));
    };
    let options = Options {
        manifest: true,
        ..options
    };
    let followed = following(
        Item::Pinned(source, name),
        options,
        None,
        cancellable,
        move |copy, source| {
            let new = NewItem::InPlace(expected);
            copy_new(copy, source, (&parent, &target_name), new, options.workers)
        },
    )
    .await?;
    followed.value?;
    let manifest = followed
        .manifest
        .ok_or_else(|| io_error("Copy worker stopped unexpectedly"))?;
    Ok((manifest, left_report(&target, followed.left)))
}

pub(super) async fn snapshot(
    source: PathBuf,
    cancellable: &gio::Cancellable,
) -> Result<Manifest, glib::Error> {
    let recorded = run_cancellable(cancellable, move |cancelled| {
        let (parent, name) = split(&source)?;
        manifest::snapshot(fs(), parent, &name, &cancelled)
            .map_err(|failure| failure.into_error(&name))
    })
    .await;
    recorded.map_err(|_| io_error("Copy worker stopped unexpectedly"))?
}

pub(super) async fn delete_unchanged_sources(
    manifest: Manifest,
    unlinks: Arc<Unlinks>,
    cancellable: &gio::Cancellable,
) -> (Vec<(PathBuf, KeptCause)>, Result<(), glib::Error>) {
    let deleted = run_cancellable(cancellable, move |cancelled| {
        manifest::delete(fs(), manifest, unlinks, cancelled)
    })
    .await;
    deleted.unwrap_or_else(|_| {
        (
            Vec::new(),
            Err(io_error("Delete worker stopped unexpectedly")),
        )
    })
}

pub(super) async fn sync_files(
    paths: Vec<PathBuf>,
    cancellable: &gio::Cancellable,
) -> Result<(), glib::Error> {
    let synced = run_cancellable(cancellable, move |cancelled| {
        for path in paths {
            let (parent, name) = split(&path)?;
            sync_tree(fs(), Entry::new(&*parent, &name), &cancelled)
                .map_err(|failure| failure.into_error(&name))?;
        }
        Ok(())
    })
    .await;
    synced.map_err(|_| io_error("Copy worker stopped unexpectedly"))?
}

fn sync_tree(fs: &dyn FsOps, at: Entry<'_>, cancelled: &AtomicBool) -> Result<(), Failure> {
    if cancelled.load(Ordering::Acquire) {
        return Err(Failure::Cancelled);
    }
    match fs.stat(at, false)?.kind {
        FileType::Directory => {
            let dir = fs.open_dir(at)?;
            for (name, _) in fs.list(dir.as_fd())? {
                sync_tree(fs, Entry::new(&dir, &name), cancelled)
                    .map_err(|failure| failure.named(&name))?;
            }
        }
        FileType::RegularFile => {
            // An SMB server flushes only for a handle that may write.
            let file = match fs.open_write(at) {
                Err(Errno::ACCESS | Errno::PERM) => fs.open_source(at, true)?.map(|(file, _)| file),
                opened => Some(opened?),
            };
            if let Some(file) = file {
                fs.sync_data(file.as_fd())?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn published(
    parent: Arc<OwnedFd>,
    name: OsString,
) -> Result<Published, glib::Error> {
    run_blocking(move || {
        let shown = name.clone();
        Published::at(fs(), parent, name).map_err(|error| Failure::Os(error).into_error(&shown))
    })
    .await
}

/// Must be opened before a copy writes beneath `path`.
pub(super) async fn barrier(path: PathBuf) -> Result<Barrier, glib::Error> {
    run_blocking(move || Barrier::open(fs(), &path)).await
}

pub(super) async fn reach(barrier: Barrier, removable: bool) -> Result<(), String> {
    match gio::spawn_blocking(move || barrier.reach(fs(), removable)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(os_error_text(error)),
        Err(error) => Err(blocking_join_message(error)),
    }
}

struct Followed<T> {
    value: Result<T, glib::Error>,
    reported: Reported,
    left: Left,
    manifest: Option<Manifest>,
}

#[derive(Default)]
struct Left {
    renamed: Vec<(PathBuf, PathBuf, RenameCause)>,
    skipped: Vec<(PathBuf, SpecialKind)>,
}

fn rebase(path: &mut PathBuf, from: &Path, to: &Path) {
    if let Ok(rest) = path.strip_prefix(from) {
        *path = if rest.as_os_str().is_empty() {
            to.to_path_buf()
        } else {
            to.join(rest)
        };
    }
}

fn report(progress: Option<&TransferProgressTracker>, root: &Path, left: Left) {
    if let Some(progress) = progress {
        progress.report(left_report(root, left));
    }
}

fn left_report(root: &Path, left: Left) -> TransferReport {
    let location = |path: PathBuf| Location::local(root.join(path));
    TransferReport {
        renamed: left
            .renamed
            .into_iter()
            .map(|(requested, saved_as, cause)| RenamedItem {
                requested: location(requested),
                saved_as: location(saved_as),
                cause,
            })
            .collect(),
        skipped: left
            .skipped
            .into_iter()
            .map(|(path, kind)| SkippedItem {
                location: location(path),
                kind,
            })
            .collect(),
        ..TransferReport::default()
    }
}

enum Item {
    Path(PathBuf),
    Pinned(Arc<OwnedFd>, OsString),
}

async fn following<T: Send + 'static>(
    item: Item,
    options: Options,
    progress: Option<Rc<TransferProgressTracker>>,
    cancellable: &gio::Cancellable,
    work: impl FnOnce(&Arc<Copy>, &Source) -> Result<T, Failure> + Send + 'static,
) -> Result<Followed<T>, glib::Error> {
    let shared = Arc::new(SharedProgress::default());
    let follower = progress.map(|progress| {
        let name = match &item {
            Item::Path(path) => path.file_name(),
            Item::Pinned(_, name) => Some(name.as_os_str()),
        };
        if let Some(name) = name {
            progress.set_current_file(name.to_string_lossy().into_owned());
        }
        progress.follow(&shared)
    });
    let counters = shared.clone();
    let copied = run_cancellable(cancellable, move |cancelled| {
        let (parent, name) = match item {
            Item::Path(path) => match split(&path) {
                Ok(split) => split,
                Err(error) => return (Err(error), Left::default(), None),
            },
            Item::Pinned(parent, name) => (parent, name),
        };
        let source = Source {
            parent,
            name,
            recorded_in: options.manifest.then(manifest::Folder::root),
        };
        let copy = Copy::new(
            fs(),
            source.name.clone(),
            cancelled,
            counters,
            true,
            options,
        );
        let value = work(&copy, &source).map_err(|failure| failure.into_error(&source.name));
        let manifest = source
            .recorded_in
            .map(|root| manifest::of(source.parent, root));
        (value, std::mem::take(&mut *lock(&copy.left)), manifest)
    })
    .await;
    let reported = follower.map_or_else(|| shared.reported(), |follower| follower.stop());
    let (value, left, manifest) =
        copied.map_err(|_| io_error("Copy worker stopped unexpectedly"))?;
    Ok(Followed {
        value,
        reported,
        left,
        manifest,
    })
}

pub(super) async fn copy_from_stage(
    staged: &StagedSibling,
    name: OsString,
    durable: bool,
    cancellable: &gio::Cancellable,
) -> Result<(), glib::Error> {
    let source = Source {
        parent: staged.parent.clone(),
        name: staged.name.clone(),
        recorded_in: None,
    };
    let identity = staged.identity;
    let copied = run_cancellable(cancellable, move |cancelled| {
        let copy = Copy::new(
            fs(),
            name.clone(),
            cancelled,
            Arc::default(),
            false,
            Options {
                durable,
                ..Options::default()
            },
        );
        let parent = source.parent.clone();
        from_stage(&copy, &source, identity, &parent, &name)
            .map_err(|failure| failure.into_error(&name))
    })
    .await;
    copied.map_err(|_| io_error("Copy worker stopped unexpectedly"))?
}

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
        let family = Mount::new(fs, root.as_fd()).family;
        restrictive::loosen(fs, root.as_fd(), &staged.restrictive, family);
    }
}

struct Source {
    parent: Arc<OwnedFd>,
    name: OsString,
    recorded_in: Option<Arc<manifest::Folder>>,
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

fn first_free<T>(
    names: impl IntoIterator<Item = OsString>,
    taken: Taken,
    mut attempt: impl FnMut(&OsStr) -> Result<T, Failure>,
) -> Result<(OsString, T), Failure> {
    for name in names {
        match attempt(&name) {
            Err(Failure::Place(PlaceError::Exists)) if taken == Taken::Number => {}
            result => return result.map(|value| (name, value)),
        }
    }
    Err(Failure::Place(PlaceError::Exists))
}

#[derive(Clone, Copy)]
enum NewItem {
    Published(Taken),
    InPlace(LocalFileIdentity),
}

fn copy_new(
    copy: &Arc<Copy>,
    source: &Source,
    (parent, name): (&Arc<OwnedFd>, &OsStr),
    new: NewItem,
    workers: Option<usize>,
) -> Result<OsString, Failure> {
    let stat = copy.fs.stat(source.entry(), false)?;
    let taken = match new {
        NewItem::Published(taken) => taken,
        NewItem::InPlace(expected) if expected == stat.identity => Taken::Fail,
        NewItem::InPlace(_) => return Err(Failure::Changed),
    };
    let in_place = matches!(new, NewItem::InPlace(_));
    let recorded_in = source.recorded_in.as_deref();
    match stat.kind {
        FileType::RegularFile if in_place => FileCopy::new(copy, parent.as_fd())
            .in_place(source.entry(), &stat, name, recorded_in)
            .map(|_| name.to_os_string()),
        FileType::RegularFile => FileCopy::new(copy, parent.as_fd())
            .copy_from(
                source.entry(),
                &stat,
                FileTarget::Final(name, taken),
                recorded_in,
            )
            .map(|(name, _)| name),
        FileType::Symlink => {
            copy.check_cancelled()?;
            let mount = Mount::new(copy.fs, parent.as_fd());
            let (name, link) = first_free(taken.names(name, false), taken, |name| {
                tree::copy_link(
                    copy,
                    source.entry(),
                    Entry::new(&**parent, name),
                    &mount,
                    None,
                )
            })?;
            if let Some(folder) = recorded_in {
                folder.entry(&source.name, &link);
            }
            Ok(name)
        }
        FileType::Directory => {
            let workers = workers.unwrap_or_else(|| worker_count(source, parent));
            if in_place || placement::can_place(copy.fs, parent.as_fd(), true) == Some(false) {
                return first_free(taken.names(name, true), taken, |name| {
                    direct_folder(copy, source, stat.identity, parent, name, workers)
                })
                .map(|(name, ())| name);
            }
            let staged = stage_folder(copy, source, stat.identity, parent, COPY_PREFIX, workers)?;
            publish_folder(copy, staged, name, taken, workers)
        }
        FileType::Fifo => {
            copy.check_cancelled()?;
            let mount = Mount::new(copy.fs, parent.as_fd());
            let parent_stat = copy.fs.stat_fd(parent.as_fd())?;
            let (name, pipe) = first_free(taken.names(name, false), taken, |name| {
                let target = Entry::new(&**parent, name);
                match tree::copy_fifo(copy, source.entry(), target, &parent_stat, &mount, None)? {
                    (pipe, Some(_)) => Ok(pipe),
                    (_, None) => Err(Failure::NotCopyable),
                }
            })?;
            if let Some(folder) = recorded_in {
                folder.entry(&source.name, &pipe);
            }
            Ok(name)
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
    let recorded_in = source.recorded_in.as_deref();
    match stat.kind {
        FileType::RegularFile => {
            let (name, identity) = FileCopy::new(copy, parent.as_fd()).copy_from(
                source.entry(),
                &stat,
                FileTarget::Hidden,
                recorded_in,
            )?;
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
            if let Some(folder) = recorded_in {
                folder.entry(&source.name, &stat);
            }
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

fn publish_folder(
    copy: &Arc<Copy>,
    staged: StagedSibling,
    name: &OsStr,
    taken: Taken,
    workers: usize,
) -> Result<OsString, Failure> {
    if let Err(failure) = copy.check_cancelled() {
        return Err(copy.discarding(staged, failure));
    }
    let mut names = taken.names(name, true);
    while let Some(name) = names.next() {
        let placed = stage::publish(
            copy.fs,
            staged.parent.as_fd(),
            &staged.name,
            staged.identity,
            &name,
        );
        match placed {
            Ok(_) => return Ok(name),
            Err(PlaceError::Exists) if taken == Taken::Number => {}
            // Either name may hold the copy, so neither is removed.
            Err(PlaceError::Unconfirmed) => return Err(Failure::Place(PlaceError::Unconfirmed)),
            Err(PlaceError::Unsupported) => {
                let from = copy.uncounted();
                let source = Source {
                    parent: staged.parent.clone(),
                    name: staged.name.clone(),
                    recorded_in: None,
                };
                let copied = first_free(std::iter::once(name).chain(names), taken, |name| {
                    direct_folder(
                        &from,
                        &source,
                        staged.identity,
                        &staged.parent,
                        name,
                        workers,
                    )
                });
                let discarded = copy.discard(staged);
                return match (copied, discarded) {
                    (Ok((name, ())), Ok(())) => Ok(name),
                    (Ok(_), Err(error)) => Err(Failure::Error(stage_left(&error))),
                    (Err(failure), discarded) => Err(copy.after_cleanup(failure, discarded)),
                };
            }
            Err(error) => return Err(copy.discarding(staged, Failure::Place(error))),
        }
    }
    Err(copy.discarding(staged, Failure::Place(PlaceError::Exists)))
}

fn stage_left(error: &glib::Error) -> glib::Error {
    io_error(rust_i18n::t!(
        "The item was copied, but its staging copy could not be removed: %{error}",
        error = crate::services::gio_error_detail(error)
    ))
}

struct Root {
    parent: Arc<OwnedFd>,
    name: OsString,
    identity: LocalFileIdentity,
}

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
        scope: 0,
        recorded_in: source.recorded_in.clone(),
        completion: None,
    };
    let (
        walked,
        tree::Walked {
            mut restrictive,
            links,
            ..
        },
    ) = tree::walk(copy, workers, created.cloned(), |tree| {
        tree.probe_links_in(&mount, root.parent.clone());
        tree::TreeJob::Folder(tree, folder)
    });
    walked.map_err(Failure::Error)?;
    if restrictive.is_empty() && links.is_empty() {
        return Ok(restrictive);
    }
    let opened = fs.open_dir(Entry::new(&*root.parent, &root.name))?;
    if !same_entry(&fs.stat_fd(opened.as_fd())?, root.identity, mount.family) {
        return Err(Failure::Place(PlaceError::Changed));
    }
    links.make(
        0,
        copy,
        opened.as_fd(),
        Path::new(""),
        mount.family,
        created.map(|created| &**created),
    )?;
    let stable = stable_inodes(mount.family);
    if let Err(error) = restrictive::tighten(fs, opened.as_fd(), &mut restrictive, stable) {
        restrictive::loosen(fs, opened.as_fd(), &restrictive, mount.family);
        return Err(error.into());
    }
    Ok(restrictive)
}

#[derive(Default)]
struct Created(Mutex<Vec<(PathBuf, LocalFileIdentity)>>);

impl Created {
    fn add(&self, path: PathBuf, identity: LocalFileIdentity) {
        lock(&self.0).push((path, identity));
    }

    fn remove(&self, fs: &dyn FsOps, root: &Root, family: Family) -> Result<(), glib::Error> {
        let root_entry = Entry::new(&*root.parent, &root.name);
        let dir = match fs.open_dir(root_entry) {
            Ok(dir) => dir,
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => return Err(io_error(os_error_detail(error))),
        };
        let opened = fs
            .stat_fd(dir.as_fd())
            .map_err(|error| io_error(os_error_detail(error)))?;
        if !same_entry(&opened, root.identity, family) {
            return Err(placement_error(PlaceError::Changed));
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
        remove_own(
            fs,
            root_entry,
            |current| same_entry(current, root.identity, family),
            AtFlags::REMOVEDIR,
        )
    }
}

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
    placing: Option<Arc<Placing>>,
) -> Result<Vec<Barrier>, Failure> {
    let (parent, name) = split(folder).map_err(Failure::Error)?;
    let workers = workers.unwrap_or_else(|| worker_count(source, &parent));
    let merged = tree::Merged {
        source: (source.parent.clone(), source.name.clone()),
        target: (parent, name, None),
        path: PathBuf::new(),
        recorded_in: source.recorded_in.clone(),
        placing,
    };
    let (
        walked,
        tree::Walked {
            stages,
            placed,
            mut nested,
            ..
        },
    ) = tree::walk(copy, workers, None, |tree| {
        tree::TreeJob::Merge(tree, merged)
    });
    let Err(error) = walked else {
        for barrier in &mut nested {
            barrier.path = folder.join(&barrier.path);
        }
        return Ok(nested);
    };
    copy.keep_left_beneath(&placed);
    let mut cleanup = Ok(());
    for staged in stages {
        cleanup = cleanup.and(copy.discard(staged));
    }
    Err(copy.after_cleanup(Failure::Error(error), cleanup))
}

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

fn same_entry(current: &EntryStat, identity: LocalFileIdentity, family: Family) -> bool {
    if stable_inodes(family) {
        current.identity == identity
    } else {
        current.kind == identity.kind
    }
}

/// Removes what a copy made at `entry`; anything else there is an error.
fn remove_own(
    fs: &dyn FsOps,
    entry: Entry<'_>,
    made: impl FnOnce(&EntryStat) -> bool,
    flags: AtFlags,
) -> Result<(), glib::Error> {
    match fs.stat(entry, true) {
        Ok(current) if made(&current) => match fs.unlink(entry, flags) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(io_error(os_error_detail(error))),
        },
        Ok(_) => Err(placement_error(PlaceError::Changed)),
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(io_error(os_error_detail(error))),
    }
}

#[derive(Clone, Copy)]
struct Mount {
    key: MountKey,
    family: Family,
    fuse_format: Option<FuseFormat>,
    coarse_times: Option<CoarseTimes>,
    deferred_close: Option<Duration>,
}

impl Mount {
    fn new(fs: &dyn FsOps, dir: BorrowedFd<'_>) -> Self {
        let key = fs.mount_key(dir);
        let mut capabilities = mount_capabilities(key);
        if capabilities.family.is_none() {
            capabilities = learn_mount_type(fs, dir, key);
        }
        Self {
            key,
            family: capabilities.family.unwrap_or(Family::Other),
            fuse_format: capabilities.fuse_format,
            coarse_times: capabilities.coarse_times,
            deferred_close: capabilities.deferred_close,
        }
    }

    fn keeps_inode_numbers(fs: &dyn FsOps, dir: BorrowedFd<'_>) -> bool {
        let key = fs.mount_key(dir);
        mount_capabilities(key)
            .stable_inodes
            .or_else(|| learn_mount_type(fs, dir, key).stable_inodes)
            .unwrap_or(true)
    }

    fn capabilities(&self) -> MountCapabilities {
        mount_capabilities(self.key)
    }

    fn learn(&self, learn: impl FnOnce(&mut MountCapabilities)) {
        learn_mount_capability(self.key, learn);
    }
}

/// For `fuseblk`, udev's record of the device tells which daemon serves it.
fn learn_mount_type(fs: &dyn FsOps, dir: BorrowedFd<'_>, key: MountKey) -> MountCapabilities {
    let mount_type = fs.mount_type(dir);
    let family = mount_type
        .as_ref()
        .map_or(Family::Other, |mount| family(&mount.fs_type));
    let stable = mount_type.as_ref().is_none_or(MountType::stable_inodes);
    let fuse_format = mount_type
        .as_ref()
        .filter(|mount| mount.fs_type == "fuseblk")
        .and_then(|_| fs.device_format(dir).ok().flatten())
        .and_then(|format| FuseFormat::of(&format));
    let coarse_times = mount_type
        .as_ref()
        .and_then(|mount| mount.coarse_times(fuse_format));
    let deferred_close = mount_type.and_then(|mount| mount.deferred_close());
    let mut learned = MountCapabilities::default();
    learn_mount_capability(key, |capabilities| {
        capabilities.family = Some(family);
        capabilities.stable_inodes = Some(stable);
        capabilities.fuse_format = fuse_format;
        capabilities.coarse_times = coarse_times;
        capabilities.deferred_close = deferred_close;
        learned = *capabilities;
    });
    learned
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

struct Copy {
    fs: &'static dyn FsOps,
    item: OsString,
    cancelled: Arc<AtomicBool>,
    progress: Arc<SharedProgress>,
    /// False for a copy from a stage, whose bytes were counted already and
    /// which is removed afterwards, so the copy may leave nothing out.
    counted: bool,
    fat_family: bool,
    data: Mutex<Vec<Data>>,
    refused_xattrs: Mutex<HashSet<OsString>>,
    nodes_refused: AtomicBool,
    recording: bool,
    durable: bool,
    left: Mutex<Left>,
}

impl Copy {
    fn new(
        fs: &'static dyn FsOps,
        item: OsString,
        cancelled: Arc<AtomicBool>,
        progress: Arc<SharedProgress>,
        counted: bool,
        options: Options,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            item,
            cancelled,
            progress,
            counted,
            fat_family: options.fat_family,
            data: Mutex::default(),
            refused_xattrs: Mutex::default(),
            nodes_refused: AtomicBool::new(false),
            recording: options.manifest,
            durable: options.durable,
            left: Mutex::default(),
        })
    }

    fn uncounted(&self) -> Arc<Self> {
        Self::new(
            self.fs,
            self.item.clone(),
            self.cancelled.clone(),
            self.progress.clone(),
            false,
            Options {
                durable: self.durable,
                ..Options::default()
            },
        )
    }

    fn nested(&self) -> Arc<Self> {
        let options = Options {
            fat_family: self.fat_family,
            workers: None,
            manifest: self.recording,
            durable: self.durable,
        };
        Self::new(
            self.fs,
            self.item.clone(),
            self.cancelled.clone(),
            self.progress.clone(),
            self.counted,
            options,
        )
    }

    fn adopt(&self, nested: &Self, path: &Path) {
        let Left { renamed, skipped } = std::mem::take(&mut *lock(&nested.left));
        let mut left = lock(&self.left);
        left.renamed
            .extend(renamed.into_iter().map(|(requested, saved_as, cause)| {
                (path.join(requested), path.join(saved_as), cause)
            }));
        left.skipped
            .extend(skipped.into_iter().map(|(at, kind)| (path.join(at), kind)));
    }

    fn renamed(&self, requested: PathBuf, saved_as: PathBuf, cause: RenameCause) {
        lock(&self.left).renamed.push((requested, saved_as, cause));
    }

    fn relocate(&self, from: &Path, to: &Path) {
        let move_beneath = |path: &mut PathBuf| {
            if path != from {
                rebase(path, from, to);
            }
        };
        let mut left = lock(&self.left);
        for (requested, saved_as, _) in &mut left.renamed {
            move_beneath(requested);
            move_beneath(saved_as);
        }
        for (path, _) in &mut left.skipped {
            move_beneath(path);
        }
    }

    fn keep_left_beneath(&self, paths: &[PathBuf]) {
        let kept = |path: &Path| paths.iter().any(|placed| path.starts_with(placed));
        let mut left = lock(&self.left);
        left.renamed.retain(|(_, saved_as, _)| kept(saved_as));
        left.skipped.retain(|(path, _)| kept(path));
    }

    fn skip(
        &self,
        path: PathBuf,
        (name, recorded_in): (&OsStr, Option<&manifest::Folder>),
        kind: SpecialKind,
    ) -> Result<(), Failure> {
        if !self.counted {
            return Err(Failure::NotCopyable);
        }
        match recorded_in {
            Some(folder) => folder.keep(name, KeptCause::Skipped(kind)),
            None => lock(&self.left).skipped.push((path, kind)),
        }
        Ok(())
    }

    fn add_file(&self, bytes: u64) {
        if self.counted {
            self.progress.add_bytes(bytes);
            self.progress.add_file();
        }
    }

    fn sync_written(&self, fd: BorrowedFd<'_>, mount: &Mount) -> Result<(), Failure> {
        if self.durable && barrier::each_file(mount) {
            self.fs.sync_data(fd)?;
        }
        Ok(())
    }

    fn check_cancelled(&self) -> Result<(), Failure> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Failure::Cancelled);
        }
        Ok(())
    }

    fn with_data<T>(&self, work: impl FnOnce(&mut Data) -> T) -> T {
        let mut data = lock(&self.data).pop().unwrap_or_default();
        let result = work(&mut data);
        lock(&self.data).push(data);
        result
    }

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

    fn copy_digested(
        &self,
        data: &mut Data,
        (source, source_stat): (BorrowedFd<'_>, &EntryStat),
        target: BorrowedFd<'_>,
        counted: bool,
    ) -> Result<ring::digest::Digest, Failure> {
        let progress = (counted && self.counted).then_some(&*self.progress);
        data.copy_digested(
            self.fs,
            &self.cancelled,
            (source, target),
            source_stat.size,
            |bytes| {
                if let Some(progress) = progress {
                    progress.add_bytes(bytes);
                }
            },
        )
    }

    /// When recording, a source that changed while it was read is read once
    /// more (`None` if it changed again), and one read within a tick of its
    /// mount's coarse times is digested for the move to compare.
    fn copy_file(
        &self,
        data: &mut Data,
        target_mount: &Mount,
        source: &Opened,
        target: BorrowedFd<'_>,
        parent: &EntryStat,
    ) -> Result<Option<(Snapshot, manifest::Read)>, Failure> {
        let mut stat = source.stat;
        let source_fd = source.fd.as_fd();
        // A folder may hold another mount, so each file asks its own.
        let mount_times = self
            .recording
            .then(|| Mount::new(self.fs, source_fd).coarse_times);
        for attempt in 0..2 {
            if attempt > 0 {
                self.fs.set_len(target, 0)?;
            }
            let counted = attempt == 0;
            let unsettled =
                mount_times.and_then(|coarse| manifest::unsettled(&stat, coarse, manifest::now()));
            let digest = match unsettled {
                Some(_) => Some(self.copy_digested(data, (source_fd, &stat), target, counted)?),
                None => {
                    self.copy_data(
                        data,
                        target_mount,
                        (source_fd, &stat),
                        target,
                        parent,
                        counted,
                    )?;
                    None
                }
            };
            let snapshot = Snapshot::read(self.fs, source_fd, &stat)?;
            if !self.recording {
                let read = manifest::Read {
                    stat,
                    contents: None,
                };
                return Ok(Some((snapshot, read)));
            }
            let read = self.fs.stat_fd(source_fd)?;
            if !manifest::changed(&stat, &read) {
                let read = manifest::Read {
                    stat: read,
                    contents: digest
                        .zip(unsettled)
                        .map(|(digest, unsettled)| manifest::Contents::new(digest, unsettled)),
                };
                return Ok(Some((snapshot, read)));
            }
            stat = read;
        }
        Ok(None)
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

struct Opened {
    fd: SourceFd,
    stat: EntryStat,
}

struct SourceFd {
    fd: Option<OwnedFd>,
    held: Option<OneOpen>,
}

impl SourceFd {
    fn new(fd: OwnedFd) -> Self {
        Self {
            fd: Some(fd),
            held: None,
        }
    }
}

impl AsFd for SourceFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_ref().expect("open until dropped").as_fd()
    }
}

impl Drop for SourceFd {
    fn drop(&mut self) {
        // Closed before `held` is released, as the fields drop after this.
        drop(self.fd.take());
    }
}

/// A listed name that no longer holds the entry, or holds another kind, is a
/// change; other errors, such as an unreadable entry, are reported as they are.
fn open_failure(name: &OsStr, error: Errno) -> Failure {
    match error {
        Errno::NOENT | Errno::NOTDIR | Errno::LOOP | Errno::STALE | Errno::NXIO | Errno::NODEV => {
            Failure::Error(io_error(rust_i18n::t!(
                "%{name} changed while it was being copied: %{error}",
                name = name.to_string_lossy(),
                error = os_error_detail(error)
            )))
        }
        error => Failure::Os(error).named(name),
    }
}

fn open_file(fs: &dyn FsOps, at: Entry<'_>, cancelled: &AtomicBool) -> Result<Opened, Failure> {
    let opened = match fs.open_source(at, true) {
        Err(Errno::INVAL) => open_after_deferred_close(fs, at, cancelled),
        opened => opened.map_err(|error| open_failure(at.name, error)),
    };
    match opened? {
        Some((fd, stat)) => Ok(Opened {
            fd: SourceFd::new(fd),
            stat,
        }),
        None => Err(Failure::Changed),
    }
}

/// How long after `closetimeo` the SMB client may take to close the handle.
const DEFERRED_CLOSE_MARGIN: Duration = Duration::from_millis(500);

/// The longest an open waits for a deferred close, as `closetimeo` may be
/// set to days.
const DEFERRED_CLOSE_LIMIT: Duration = Duration::from_secs(10);

/// After this machine writes a file, the SMB client keeps its handle open
/// for `closetimeo` and refuses an open of another name of the file with
/// `EINVAL` meanwhile, so the open is tried again until then.
fn open_after_deferred_close(
    fs: &dyn FsOps,
    at: Entry<'_>,
    cancelled: &AtomicBool,
) -> Result<Option<(OwnedFd, EntryStat)>, Failure> {
    let Some(deferred) = Mount::new(fs, at.dir).deferred_close else {
        return Err(open_failure(at.name, Errno::INVAL));
    };
    let mut left = deferred
        .saturating_add(DEFERRED_CLOSE_MARGIN)
        .min(DEFERRED_CLOSE_LIMIT);
    let mut pause = Duration::from_millis(10);
    while !left.is_zero() {
        let waited = pause.min(left);
        manifest::sleep(waited);
        left -= waited;
        if cancelled.load(Ordering::Acquire) {
            return Err(Failure::Cancelled);
        }
        match fs.open_source(at, true) {
            Err(Errno::INVAL) => pause = (pause * 2).min(Duration::from_millis(50)),
            opened => return opened.map_err(|error| open_failure(at.name, error)),
        }
    }
    Err(open_failure(at.name, Errno::INVAL))
}

fn open_one_name(
    fs: &dyn FsOps,
    at: Entry<'_>,
    seen: &EntryStat,
    cancelled: &AtomicBool,
) -> Result<Opened, Failure> {
    let held = OneOpen::wait(seen, cancelled)?;
    let mut source = open_file(fs, at, cancelled)?;
    source.fd.held = Some(held);
    if source.stat.identity != seen.identity {
        return Err(Failure::Changed);
    }
    Ok(source)
}

/// Files on SMB sources that a copy has a name of open, by device and
/// inode: the SMB client takes one lease per file and refuses an open of
/// another name meanwhile, or a Samba server defers it forever. Link counts
/// the client took from a listing say 1, so every file is held.
static OPEN_ON_SMB: Mutex<BTreeSet<(u64, u64)>> = Mutex::new(BTreeSet::new());
static OPEN_ON_SMB_CLOSED: Condvar = Condvar::new();

struct OneOpen((u64, u64));

impl OneOpen {
    fn wait(seen: &EntryStat, cancelled: &AtomicBool) -> Result<Self, Failure> {
        let key = (seen.identity.device, seen.identity.inode);
        let mut open = lock(&OPEN_ON_SMB);
        while !open.insert(key) {
            if cancelled.load(Ordering::Acquire) {
                return Err(Failure::Cancelled);
            }
            open = OPEN_ON_SMB_CLOSED
                .wait_timeout(open, Duration::from_millis(50))
                .map_or_else(|error| error.into_inner().0, |(open, _)| open);
        }
        Ok(Self(key))
    }
}

impl Drop for OneOpen {
    fn drop(&mut self) {
        lock(&OPEN_ON_SMB).remove(&self.0);
        OPEN_ON_SMB_CLOSED.notify_all();
    }
}

#[derive(Clone, Copy)]
enum FileTarget<'a> {
    Final(&'a OsStr, Taken),
    Hidden,
}

impl FileTarget<'_> {
    fn stage_prefix(self) -> &'static str {
        match self {
            Self::Final(..) => COPY_PREFIX,
            Self::Hidden => STAGE_PREFIX,
        }
    }
}

struct Stage {
    name: OsString,
    created: EntryStat,
}

struct FileCopy<'a> {
    copy: &'a Copy,
    dir: BorrowedFd<'a>,
    target: Mount,
    data: Data,
    read: Option<manifest::Read>,
}

impl<'a> FileCopy<'a> {
    fn new(copy: &'a Copy, dir: BorrowedFd<'a>) -> Self {
        Self {
            copy,
            dir,
            target: Mount::new(copy.fs, dir),
            data: Data::default(),
            read: None,
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

    fn open(&self, at: Entry<'_>, seen: &EntryStat) -> Result<Opened, Failure> {
        let fs = self.copy.fs;
        if Mount::new(fs, at.dir).family != Family::Smb {
            return open_file(fs, at, &self.copy.cancelled);
        }
        open_one_name(fs, at, seen, &self.copy.cancelled)
    }

    fn copy_from(
        &mut self,
        at: Entry<'_>,
        seen: &EntryStat,
        target: FileTarget<'_>,
        recorded_in: Option<&manifest::Folder>,
    ) -> Result<(OsString, LocalFileIdentity), Failure> {
        let fs = self.copy.fs;
        self.copy.check_cancelled()?;
        let source = self.open(at, seen)?;
        let parent = fs.stat_fd(self.dir)?;
        let mut tmpfile = None;
        if stage::tmpfile_allowed(self.target.family)
            && self.target.capabilities().tmpfile != Some(false)
            && TMPFILE_LINK.load(Ordering::Relaxed) != TMPFILE_LINK_UNAVAILABLE
        {
            match fs.open_tmpfile(self.dir, Mode::from_raw_mode(0o600)) {
                Ok(fd) => {
                    self.target
                        .learn(|capabilities| capabilities.tmpfile = Some(true));
                    tmpfile = Some(fd);
                }
                // Before Linux 3.11 the flag reads as O_DIRECTORY.
                Err(error) if is_unsupported(error) || error == Errno::ISDIR => {
                    self.target
                        .learn(|capabilities| capabilities.tmpfile = Some(false));
                }
                Err(error) => return Err(error.into()),
            }
        }
        let copied = match tmpfile {
            Some(fd) => self.through_tmpfile(&source, fd, &parent, target),
            None => self.through_named_stage(&source, &parent, target),
        }?;
        if let (Some(folder), Some(read)) = (recorded_in, self.read.take()) {
            folder.file(at.name, read);
        }
        Ok(copied)
    }

    fn in_place(
        &mut self,
        at: Entry<'_>,
        seen: &EntryStat,
        name: &OsStr,
        recorded_in: Option<&manifest::Folder>,
    ) -> Result<LocalFileIdentity, Failure> {
        let fs = self.copy.fs;
        self.copy.check_cancelled()?;
        let source = self.open(at, seen)?;
        if source.stat.identity != seen.identity {
            return Err(Failure::Changed);
        }
        let parent = fs.stat_fd(self.dir)?;
        let identity = self.create(name, &parent, |file, fd| {
            file.read_into(&source, fd, &parent)
        })?;
        if let (Some(folder), Some(read)) = (recorded_in, self.read.take()) {
            folder.file(at.name, read);
        }
        self.copy.add_file(0);
        Ok(identity)
    }

    fn read_into(
        &mut self,
        source: &Opened,
        target: BorrowedFd<'_>,
        parent: &EntryStat,
    ) -> Result<Snapshot, Failure> {
        let (snapshot, read) = self
            .copy
            .copy_file(&mut self.data, &self.target, source, target, parent)?
            .ok_or(Failure::Changed)?;
        self.read = Some(read);
        Ok(snapshot)
    }

    fn link(&self, tmpfile: BorrowedFd<'_>, target: FileTarget<'_>) -> Result<OsString, Errno> {
        let fs = self.copy.fs;
        match target {
            FileTarget::Final(name, taken) => {
                for name in taken.names(name, false) {
                    match link_tmpfile(fs, tmpfile, self.at(&name)) {
                        Err(error)
                            if taken == Taken::Number
                                && placement::file_creation_error(error) == PlaceError::Exists => {}
                        linked => return linked.map(|()| name),
                    }
                }
                Err(Errno::EXIST)
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
        let snapshot = self.read_into(source, tmpfile.as_fd(), parent)?;
        let created = fs.stat_fd(tmpfile.as_fd())?;
        self.copy
            .apply_metadata(tmpfile.as_fd(), &created, &snapshot, parent, &self.target)?;
        self.copy.sync_written(tmpfile.as_fd(), &self.target)?;
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
        let (fd, stage) = self.create_stage(target)?;
        let copied = self
            .read_into(source, fd.as_fd(), parent)
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

    fn finish_stage(
        &mut self,
        fd: OwnedFd,
        stage: Stage,
        snapshot: &Snapshot,
        parent: &EntryStat,
        target: FileTarget<'_>,
    ) -> Result<(OsString, LocalFileIdentity), Failure> {
        let fs = self.copy.fs;
        let settled = self
            .copy
            .sync_written(fd.as_fd(), &self.target)
            .and_then(|()| fs.close(fd).map_err(Failure::Os))
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
        let FileTarget::Final(name, taken) = target else {
            self.copy.add_file(0);
            return Ok((stage.name, stage.created.identity));
        };
        let mut names = taken.names(name, false);
        let published = loop {
            let Some(name) = names.next() else {
                return Err(self.discard(&stage, Failure::Place(PlaceError::Exists)));
            };
            match stage::publish(fs, self.dir, &stage.name, stage.created.identity, &name) {
                Ok(_) => break (name, stage.created.identity),
                Err(PlaceError::Exists) if taken == Taken::Number => {}
                // Either name may hold the copy, so neither is removed.
                Err(PlaceError::Unconfirmed) => {
                    return Err(Failure::Place(PlaceError::Unconfirmed));
                }
                Err(PlaceError::Unsupported) => {
                    let dir = self.dir;
                    let staged = Entry {
                        dir,
                        name: &stage.name,
                    };
                    let created = first_free(std::iter::once(name).chain(names), taken, |name| {
                        self.create_from(
                            staged,
                            stage.created.identity,
                            Some(snapshot),
                            name,
                            parent,
                        )
                    });
                    let created = match created {
                        Ok(created) => created,
                        Err(failure) => return Err(self.discard(&stage, failure)),
                    };
                    if let Err(error) = self.remove_made(&stage.name, stage.created.identity) {
                        return Err(Failure::Error(stage_left(&error)));
                    }
                    break created;
                }
                Err(error) => return Err(self.discard(&stage, Failure::Place(error))),
            }
        };
        self.copy.add_file(0);
        Ok(published)
    }

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
            Some((fd, stat)) if stat.identity == expected => Opened {
                fd: SourceFd::new(fd),
                stat,
            },
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
        self.create(name, parent, |file, fd| {
            let from = (source.fd.as_fd(), &source.stat);
            file.copy
                .copy_data(&mut file.data, &file.target, from, fd, parent, false)?;
            Ok(snapshot)
        })
    }

    fn create<S: Borrow<Snapshot>>(
        &mut self,
        name: &OsStr,
        parent: &EntryStat,
        write: impl FnOnce(&mut Self, BorrowedFd<'_>) -> Result<S, Failure>,
    ) -> Result<LocalFileIdentity, Failure> {
        let fs = self.copy.fs;
        let target = self.at(name);
        let fd = fs
            .create_exclusive(target, Mode::from_raw_mode(0o600))
            .map_err(|error| Failure::Place(placement::file_creation_error(error)))?;
        let created = fs.stat_fd(fd.as_fd())?;
        // Takes `fd`, so a failed file is closed before it is removed: NFS
        // renames an open file aside instead.
        let filled = (|fd: OwnedFd| -> Result<(), Failure> {
            let written = write(self, fd.as_fd())?;
            let snapshot: &Snapshot = written.borrow();
            self.copy
                .apply_metadata(fd.as_fd(), &created, snapshot, parent, &self.target)?;
            self.copy.sync_written(fd.as_fd(), &self.target)?;
            fs.close(fd)?;
            self.copy
                .settle_times(target, created.identity, snapshot, self.target.family)
        })(fd);
        match filled {
            Ok(()) => Ok(created.identity),
            Err(failure) => {
                let removed = self.remove_made(name, created.identity);
                Err(self.copy.after_cleanup(failure, removed))
            }
        }
    }

    fn remove_made(&self, name: &OsStr, identity: LocalFileIdentity) -> Result<(), glib::Error> {
        remove_own(
            self.copy.fs,
            self.at(name),
            |current| current.identity == identity,
            AtFlags::empty(),
        )
    }

    fn discard(&self, stage: &Stage, failure: Failure) -> Failure {
        let removed = self.remove_made(&stage.name, stage.created.identity);
        self.copy.after_cleanup(failure, removed)
    }
}
