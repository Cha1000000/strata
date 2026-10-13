// SPDX-License-Identifier: MIT

//! What a move copied of each source entry, as it was once the copy had
//! read it, so that afterwards only the entries the copy holds unchanged
//! are deleted. It keeps names, never descriptors.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use gtk::glib;
use rustix::{fs::AtFlags, io::Errno};

use super::{Failure, Mount, lock};
use crate::{
    adapters::local_operations::{
        LocalFileIdentity, bounded_local_worker_count, cancelled_local_operation,
        fs_ops::{Entry, EntryStat, FsOps},
        io_error, local_device_is_rotational,
        work_queue::{self, Group, Job, WorkQueue},
    },
    services::KeptCause,
};

/// The link count each name deletions removed of a file with several names
/// had just before, by device and inode. A transfer's deletions share one,
/// as its items may hold names of one file. Those counts tell which
/// removals followed a snapshot: the ones at or below its count.
#[derive(Default)]
pub(in crate::adapters::local_operations) struct Unlinks(Mutex<HashMap<(u64, u64), Vec<u64>>>);

/// A source entry as the copy read it.
#[derive(Clone, Copy)]
struct SourceSnapshot {
    identity: LocalFileIdentity,
    nlink: u64,
    size: u64,
    mtime: (i64, u32),
    ctime: (i64, u32),
}

impl SourceSnapshot {
    fn of(stat: &EntryStat) -> Self {
        Self {
            identity: stat.identity,
            nlink: stat.nlink,
            size: stat.size,
            mtime: stat.mtime,
            ctime: stat.ctime,
        }
    }

    /// Whether `current` is still this entry, where deletions have removed
    /// `unlinked` of its other names since it was taken. Removing a name
    /// changes the ctime and link count of the others.
    fn holds(&self, current: &EntryStat, stable_inodes: bool, unlinked: u64) -> bool {
        let same = current.kind == self.identity.kind
            && current.size == self.size
            && current.mtime == self.mtime;
        if !stable_inodes {
            // Nothing tells which names of a file this deletion removed.
            return same && (current.ctime == self.ctime || self.nlink > 1);
        }
        // Every change to an entry moves its change time; the identity and
        // link count are defence in depth.
        same && current.identity == self.identity
            && current.nlink + unlinked == self.nlink
            && (current.ctime == self.ctime || unlinked > 0)
    }
}

/// Whether a source changed between two stats of its descriptor.
pub(super) fn changed(before: &EntryStat, after: &EntryStat) -> bool {
    before.size != after.size || before.mtime != after.mtime || before.ctime != after.ctime
}

/// A source folder, and what the copy took from it.
pub(super) struct Folder {
    /// `None` for the folder holding the moved item, which stays.
    identity: Option<LocalFileIdentity>,
    children: Mutex<Vec<(OsString, Node)>>,
}

enum Node {
    Entry(SourceSnapshot),
    Folder(Arc<Folder>),
    Kept(KeptCause),
}

impl Folder {
    /// The folder holding the moved item.
    pub(super) fn root() -> Arc<Self> {
        Arc::new(Self {
            identity: None,
            children: Mutex::default(),
        })
    }

    /// The child folder `name`, recorded with its `identity`, to record its
    /// own children in.
    pub(super) fn folder(&self, name: &OsStr, identity: LocalFileIdentity) -> Arc<Self> {
        let folder = Arc::new(Self {
            identity: Some(identity),
            children: Mutex::default(),
        });
        self.add(name, Node::Folder(folder.clone()));
        folder
    }

    /// The child `name` was copied as `stat` describes it.
    pub(super) fn entry(&self, name: &OsStr, stat: &EntryStat) {
        self.add(name, Node::Entry(SourceSnapshot::of(stat)));
    }

    /// The child `name` stays at the source.
    pub(super) fn keep(&self, name: &OsStr, cause: KeptCause) {
        self.add(name, Node::Kept(cause));
    }

    fn add(&self, name: &OsStr, node: Node) {
        lock(&self.children).push((name.to_os_string(), node));
    }

    fn take(&self) -> Vec<(OsString, Node)> {
        std::mem::take(&mut *lock(&self.children))
    }
}

/// A later name of a file with several names, recorded once the links pass
/// has made it.
pub(super) struct Pending {
    folder: Arc<Folder>,
    name: OsString,
    snapshot: SourceSnapshot,
}

impl Pending {
    pub(super) fn new(folder: Arc<Folder>, name: &OsStr, stat: &EntryStat) -> Self {
        Self {
            folder,
            name: name.to_os_string(),
            snapshot: SourceSnapshot::of(stat),
        }
    }

    pub(super) fn made(self) {
        self.folder.add(&self.name, Node::Entry(self.snapshot));
    }

    pub(super) fn keep(self, cause: KeptCause) {
        self.folder.keep(&self.name, cause);
    }
}

/// The native item a move copied, by name in its pinned parent.
pub(in crate::adapters::local_operations) struct Manifest {
    pub(super) parent: Arc<OwnedFd>,
    pub(super) root: Arc<Folder>,
}

/// Records the item `name` in `parent` as it is now, for a copy that this
/// engine does not make.
pub(super) fn snapshot(
    fs: &dyn FsOps,
    parent: Arc<OwnedFd>,
    name: &OsStr,
    cancelled: &AtomicBool,
) -> Result<Manifest, Failure> {
    let root = Folder::root();
    record(fs, parent.as_fd(), name, &root, cancelled)?;
    Ok(Manifest { parent, root })
}

fn record(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
    name: &OsStr,
    folder: &Folder,
    cancelled: &AtomicBool,
) -> Result<(), Failure> {
    if cancelled.load(Ordering::Acquire) {
        return Err(Failure::Cancelled);
    }
    let at = Entry { dir, name };
    let stat = fs.stat(at, false)?;
    if !stat.is_directory() {
        folder.entry(name, &stat);
        return Ok(());
    }
    let opened = fs.open_dir(at)?;
    if fs.stat_fd(opened.as_fd())?.identity != stat.identity {
        return Err(Failure::Changed);
    }
    let recorded = folder.folder(name, stat.identity);
    for (child, _) in fs.list(opened.as_fd())? {
        record(fs, opened.as_fd(), &child, &recorded, cancelled)
            .map_err(|failure| failure.named(&child))?;
    }
    Ok(())
}

/// Deletes each entry `manifest` recorded that the source still holds as
/// recorded, then each folder left empty, and returns what it found must
/// stay, by path beneath the item's parent, even when it stopped. Entries
/// added to the source meanwhile are not recorded, so they and their
/// folders stay.
pub(super) fn delete(
    fs: &'static dyn FsOps,
    manifest: Manifest,
    unlinks: Arc<Unlinks>,
    cancelled: Arc<AtomicBool>,
) -> (Vec<(PathBuf, KeptCause)>, Result<(), glib::Error>) {
    let Manifest { parent, root } = manifest;
    let deletion = Arc::new(Deletion {
        fs,
        stable_inodes: Mount::keeps_inode_numbers(fs, parent.as_fd()),
        unlinks,
        kept: Mutex::default(),
    });
    let queue = Arc::new(WorkQueue::new(cancelled));
    for (name, path, step) in deletion.steps(&root, Path::new("")) {
        let at = At {
            deletion: deletion.clone(),
            parent: parent.clone(),
            name,
            path,
        };
        queue.enqueue(Removal {
            at,
            step,
            completion: None,
        });
    }
    let available = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let rotational = local_device_is_rotational(parent.as_fd()) == Some(true);
    let workers = bounded_local_worker_count(available, rotational);
    work_queue::run_workers(&queue, workers, Some(10), |work| {
        thread::Builder::new().spawn(work)
    });
    let result = queue.result();
    (std::mem::take(&mut *lock(&deletion.kept)), result)
}

/// What every job of one deletion shares.
struct Deletion {
    fs: &'static dyn FsOps,
    /// FUSE daemons without `use_ino` and cifs without server inode
    /// numbers number an entry afresh once the kernel evicts it.
    stable_inodes: bool,
    unlinks: Arc<Unlinks>,
    kept: Mutex<Vec<(PathBuf, KeptCause)>>,
}

impl Deletion {
    fn keep(&self, path: PathBuf, cause: KeptCause) {
        lock(&self.kept).push((path, cause));
    }

    /// Making `path` go failed with `error`.
    fn failed(&self, path: PathBuf, error: Errno) {
        match error {
            // The copy holds it, and its source is gone, as a move leaves it.
            Errno::NOENT => {
                tracing::debug!(?path, "a moved source was gone before it was deleted");
            }
            error => self.keep(path, KeptCause::Undeletable(error.raw_os_error())),
        }
    }

    /// What removes each entry `folder`, at `path`, recorded; entries
    /// recorded as staying are kept at once.
    fn steps(&self, folder: &Folder, path: &Path) -> Vec<(OsString, PathBuf, Step)> {
        let mut steps = Vec::new();
        for (name, node) in folder.take() {
            let path = path.join(&name);
            let step = match node {
                Node::Kept(cause) => {
                    self.keep(path, cause);
                    continue;
                }
                Node::Entry(snapshot) => Step::Entry(snapshot),
                Node::Folder(folder) => Step::Folder(folder),
            };
            steps.push((name, path, step));
        }
        steps
    }

    /// Whether the folder `current` is the one recorded as `identity`.
    fn same(&self, current: &EntryStat, identity: Option<LocalFileIdentity>) -> bool {
        identity.is_some_and(|identity| {
            if self.stable_inodes {
                current.identity == identity
            } else {
                current.kind == identity.kind
            }
        })
    }
}

/// A recorded entry, by name in its opened parent.
struct At {
    deletion: Arc<Deletion>,
    parent: Arc<OwnedFd>,
    name: OsString,
    /// Beneath the item's parent.
    path: PathBuf,
}

struct Removal {
    at: At,
    step: Step,
    completion: Option<Arc<Group<Removal>>>,
}

enum Step {
    Folder(Arc<Folder>),
    Entry(SourceSnapshot),
    /// Removes the folder opened as this, once its recorded entries are done.
    Rmdir(Arc<OwnedFd>),
}

impl Job for Removal {
    type Error = glib::Error;

    fn run(self, queue: &WorkQueue<Self>) {
        if queue.is_stopped() {
            return;
        }
        let Self {
            at,
            step,
            completion,
        } = self;
        match step {
            Step::Folder(folder) => at.folder(&folder, completion, queue),
            Step::Entry(snapshot) => {
                at.entry(snapshot);
                queue.complete(completion);
            }
            Step::Rmdir(opened) => {
                at.rmdir(opened);
                queue.complete(completion);
            }
        }
    }

    fn cancelled() -> glib::Error {
        cancelled_local_operation()
    }

    fn panicked() -> glib::Error {
        io_error("Delete worker panicked")
    }

    fn not_started(error: io::Error) -> glib::Error {
        io_error(format!("Could not start delete worker: {error}"))
    }
}

impl At {
    fn entry_at(&self) -> Entry<'_> {
        Entry::new(&*self.parent, &self.name)
    }

    fn folder(
        self,
        folder: &Folder,
        completion: Option<Arc<Group<Removal>>>,
        queue: &WorkQueue<Removal>,
    ) {
        let deletion = self.deletion.clone();
        let fs = deletion.fs;
        let opened = fs
            .open_dir(self.entry_at())
            .and_then(|fd| fs.stat_fd(fd.as_fd()).map(|stat| (fd, stat)));
        let fd = match opened {
            Ok((fd, current)) if deletion.same(&current, folder.identity) => Arc::new(fd),
            Ok(_) | Err(Errno::NOTDIR | Errno::LOOP) => {
                deletion.keep(self.path, KeptCause::Changed);
                queue.complete(completion);
                return;
            }
            Err(error) => {
                deletion.failed(self.path, error);
                queue.complete(completion);
                return;
            }
        };
        let steps = deletion.steps(folder, &self.path);
        let rmdir = Removal {
            at: self,
            step: Step::Rmdir(fd.clone()),
            completion,
        };
        if steps.is_empty() {
            rmdir.run(queue);
            return;
        }
        let group = Group::new(steps.len(), rmdir);
        for (name, path, step) in steps {
            let at = Self {
                deletion: deletion.clone(),
                parent: fd.clone(),
                name,
                path,
            };
            queue.enqueue(Removal {
                at,
                step,
                completion: Some(group.clone()),
            });
        }
    }

    fn entry(&self, snapshot: SourceSnapshot) {
        let deletion = &self.deletion;
        let fs = deletion.fs;
        let key = (snapshot.identity.device, snapshot.identity.inode);
        // Checking, removing and counting a name of a file with several
        // names must not interleave with another name's check.
        let mut unlinks = (snapshot.nlink > 1).then(|| lock(&deletion.unlinks.0));
        let since = unlinks
            .as_ref()
            .and_then(|unlinks| unlinks.get(&key))
            .map_or(0, |counts| {
                counts
                    .iter()
                    .filter(|&&count| count <= snapshot.nlink)
                    .count() as u64
            });
        match fs.stat(self.entry_at(), true) {
            Ok(current) if snapshot.holds(&current, deletion.stable_inodes, since) => {
                match fs.unlink(self.entry_at(), AtFlags::empty()) {
                    Ok(()) => {
                        if let Some(unlinks) = &mut unlinks {
                            unlinks.entry(key).or_default().push(current.nlink);
                        }
                    }
                    Err(error) => deletion.failed(self.path.clone(), error),
                }
            }
            Ok(_) => deletion.keep(self.path.clone(), KeptCause::Changed),
            Err(error) => deletion.failed(self.path.clone(), error),
        }
    }

    fn rmdir(&self, opened: Arc<OwnedFd>) {
        let deletion = &self.deletion;
        let fs = deletion.fs;
        // An open folder keeps its inode number even where numbers change
        // between the copy and the deletion, so the name must report it.
        let still = fs
            .stat(self.entry_at(), false)
            .and_then(|named| Ok((named, fs.stat_fd(opened.as_fd())?)))
            .is_ok_and(|(named, opened)| named.identity == opened.identity);
        drop(opened);
        if !still {
            return;
        }
        match fs.unlink(self.entry_at(), AtFlags::REMOVEDIR) {
            // A folder that holds entries kept or added meanwhile stays.
            Ok(()) | Err(Errno::NOENT | Errno::NOTEMPTY | Errno::EXIST) => {}
            Err(error) => deletion.failed(self.path.clone(), error),
        }
    }
}
