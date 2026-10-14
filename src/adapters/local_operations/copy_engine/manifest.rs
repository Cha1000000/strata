// SPDX-License-Identifier: MIT

//! What a move copied of each source entry, as it was once the copy had
//! read it, so that afterwards only the entries the copy holds unchanged
//! are deleted. It keeps names, never descriptors.

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gtk::glib;
use ring::digest;
use rustix::{fs::AtFlags, io::Errno};

use super::{Failure, Mount, lock};
use crate::{
    adapters::local_operations::{
        LocalFileIdentity, bounded_local_worker_count, cancelled_local_operation,
        fs_ops::{Entry, EntryStat, FsOps, OffMain},
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

/// The time tests pretend it is, which waits move on instead of sleeping.
#[cfg(test)]
static TEST_NOW: Mutex<Option<SystemTime>> = Mutex::new(None);

/// Pretends it is `now`, or follows the real clock with `None`.
#[cfg(test)]
pub(in crate::adapters::local_operations) fn set_clock_for_test(now: Option<SystemTime>) {
    *lock(&TEST_NOW) = now;
}

pub(in crate::adapters::local_operations) fn now() -> SystemTime {
    #[cfg(test)]
    if let Some(now) = *lock(&TEST_NOW) {
        return now;
    }
    SystemTime::now()
}

/// Waits until `until`, checking `stopped` meanwhile; false if it stopped.
fn wait_until(until: SystemTime, stopped: &dyn Fn() -> bool) -> bool {
    loop {
        let left = until.duration_since(now()).unwrap_or_default();
        if left.is_zero() {
            return true;
        }
        if stopped() {
            return false;
        }
        #[cfg(test)]
        if let Some(pretended) = lock(&TEST_NOW).as_mut() {
            *pretended = until;
            continue;
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// How far the coarse clock filesystems stamp times with may run behind
/// ours: a kernel tick, at most 10 ms, with room to spare.
const COARSE_CLOCK: Duration = Duration::from_millis(100);

/// Until when a rewrite of the file `stat` describes, read from `started`,
/// may leave its modification time as it is, where the mount keeps times
/// as coarse as `window`; `None` once any rewrite would move it.
pub(super) fn unsettled(
    stat: &EntryStat,
    window: Duration,
    started: SystemTime,
) -> Option<SystemTime> {
    let (seconds, nanoseconds) = stat.mtime;
    let modified =
        UNIX_EPOCH.checked_add(Duration::new(u64::try_from(seconds).ok()?, nanoseconds))?;
    // A time ahead of our clock, from a server's, settles no later than one
    // read as it was set.
    let settled = modified.min(started).checked_add(window + COARSE_CLOCK)?;
    (started < settled).then_some(settled)
}

/// What a copy holds of a source file whose times may not show a rewrite:
/// a digest of the bytes it copied, which the source must still match once
/// any rewrite would move its modification time.
pub(super) struct Contents {
    digest: digest::Digest,
    settled: SystemTime,
}

impl Contents {
    pub(super) fn new(digest: digest::Digest, settled: SystemTime) -> Self {
        Self { digest, settled }
    }
}

/// A source file as a copy read it.
pub(super) struct Read {
    pub stat: EntryStat,
    pub contents: Option<Contents>,
}

const MIB: usize = 1 << 20;

/// A source entry as the copy read it.
struct SourceSnapshot {
    identity: LocalFileIdentity,
    nlink: u64,
    size: u64,
    mtime: (i64, u32),
    ctime: (i64, u32),
    contents: Option<Box<Contents>>,
}

impl SourceSnapshot {
    fn of(stat: &EntryStat, contents: Option<Contents>) -> Self {
        Self {
            identity: stat.identity,
            nlink: stat.nlink,
            size: stat.size,
            mtime: stat.mtime,
            ctime: stat.ctime,
            contents: contents.map(Box::new),
        }
    }

    /// Whether `current` is still this entry, where deletions have removed
    /// `unlinked` of its other names since it was taken. Removing a name
    /// changes the ctime and link count of the others.
    fn holds(&self, current: &EntryStat, stable_inodes: bool, unlinked: u64) -> bool {
        let same = current.identity.may_be(&self.identity)
            && current.size == self.size
            && current.mtime == self.mtime;
        if !stable_inodes {
            // Nothing tells which names of a file this deletion removed. Where
            // times are as coarse as FAT's, only a birth time tells a file put
            // in its place apart.
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
        self.add(name, Node::Entry(SourceSnapshot::of(stat, None)));
    }

    /// The child file `name` was copied as `read` describes it.
    pub(super) fn file(&self, name: &OsStr, read: Read) {
        self.add(
            name,
            Node::Entry(SourceSnapshot::of(&read.stat, read.contents)),
        );
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
            snapshot: SourceSnapshot::of(stat, None),
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
    pub(super) parent: OffMain<Arc<OwnedFd>>,
    pub(super) root: Arc<Folder>,
    /// Where the copy went, where the move can see it.
    copy: Option<Published>,
}

impl Manifest {
    pub(in crate::adapters::local_operations) fn copied_to(&mut self, copy: Published) {
        self.copy = Some(copy);
    }
}

/// A move's copy, by name in its pinned parent, as published.
pub(in crate::adapters::local_operations) struct Published {
    parent: OffMain<Arc<OwnedFd>>,
    name: OsString,
    identity: LocalFileIdentity,
}

impl Published {
    /// The item `name` in `parent` as it is now.
    pub(super) fn at(fs: &dyn FsOps, parent: Arc<OwnedFd>, name: OsString) -> Result<Self, Errno> {
        let identity = fs.stat(Entry::new(&*parent, &name), false)?.identity;
        Ok(Self {
            parent: OffMain::new(parent),
            name,
            identity,
        })
    }

    /// Whether the copy is still where it was published.
    fn stays(&self, fs: &dyn FsOps) -> bool {
        let parent = &**self.parent;
        let stable = Mount::keeps_inode_numbers(fs, parent.as_fd());
        fs.stat(Entry::new(parent, &self.name), false)
            .is_ok_and(|current| {
                if stable {
                    current.identity == self.identity
                } else {
                    current.identity.may_be(&self.identity)
                }
            })
    }
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
    Ok(Manifest {
        parent: OffMain::new(parent),
        root,
        copy: None,
    })
}

/// A manifest of what a copy that records took of the item in `parent`.
pub(super) fn of(parent: Arc<OwnedFd>, root: Arc<Folder>) -> Manifest {
    Manifest {
        parent: OffMain::new(parent),
        root,
        copy: None,
    }
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
    let Manifest { parent, root, copy } = manifest;
    // Deleting what only a copy that is gone held would lose it.
    if copy.is_some_and(|copy| !copy.stays(fs)) {
        let kept = root
            .take()
            .into_iter()
            .map(|(name, _)| (PathBuf::from(name), KeptCause::CopyGone))
            .collect();
        return (kept, Ok(()));
    }
    let parent = parent.into_inner();
    let deletion = Arc::new(Deletion {
        fs,
        stable_inodes: Mount::keeps_inode_numbers(fs, parent.as_fd()),
        unlinks,
        kept: Mutex::default(),
    });
    let steps = deletion.steps(&root, Path::new(""));
    let workers = match steps.as_slice() {
        // A moved file has no folder to share out.
        [(_, _, Step::Entry(_))] => 1,
        _ => {
            let available = thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1);
            let rotational = local_device_is_rotational(parent.as_fd()) == Some(true);
            bounded_local_worker_count(available, rotational)
        }
    };
    let queue = Arc::new(WorkQueue::new(cancelled));
    for (name, path, step) in steps {
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

    /// Whether the folder at `at`, at `path`, which could not be removed,
    /// holds an entry that is not, nor holds, one already kept.
    fn holds_others(&self, at: Entry<'_>, path: &Path) -> bool {
        let kept: HashSet<OsString> = lock(&self.kept)
            .iter()
            .filter_map(|(kept, _)| kept.strip_prefix(path).ok()?.iter().next())
            .map(OsStr::to_os_string)
            .collect();
        self.fs
            .open_dir(at)
            .and_then(|opened| self.fs.list(opened.as_fd()))
            .map_or(true, |entries| {
                entries.iter().any(|(name, _)| !kept.contains(name))
            })
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
                current.identity.may_be(&identity)
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
                at.entry(&snapshot, &|| queue.is_stopped());
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

    fn entry(&self, snapshot: &SourceSnapshot, stopped: &dyn Fn() -> bool) {
        let deletion = &self.deletion;
        let fs = deletion.fs;
        // Compared before the stat below, which sees any later rewrite.
        let compared = match &snapshot.contents {
            None => None,
            Some(contents) => match self.same_contents(contents, snapshot.size, stopped) {
                None => return,
                Some(Ok(Some(file))) => Some(file),
                Some(Ok(None)) => return deletion.keep(self.path.clone(), KeptCause::Changed),
                Some(Err(error)) => return deletion.failed(self.path.clone(), error),
            },
        };
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
        let current = fs.stat(self.entry_at(), true);
        // Only the file compared may go, not one renamed over its name since;
        // its open descriptor keeps its inode number. Closed before the
        // unlink, as FUSE and NFS rename a file unlinked while open aside.
        let compared_here = compared.is_none_or(|file| {
            let opened = fs.stat_fd(file.as_fd());
            matches!((&current, opened), (Ok(current), Ok(opened)) if opened.identity == current.identity)
        });
        match current {
            Ok(current)
                if compared_here && snapshot.holds(&current, deletion.stable_inodes, since) =>
            {
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

    /// The file, still open, if it holds what the copy holds, read once any
    /// rewrite would move its modification time; `None` if the deletion
    /// stopped meanwhile.
    fn same_contents(
        &self,
        contents: &Contents,
        size: u64,
        stopped: &dyn Fn() -> bool,
    ) -> Option<Result<Option<OwnedFd>, Errno>> {
        if !wait_until(contents.settled, stopped) {
            return None;
        }
        let fs = self.deletion.fs;
        let file = match fs.open_source(self.entry_at(), false) {
            Ok(Some((file, _))) => file,
            Ok(None) => return Some(Ok(None)),
            Err(error) => return Some(Err(error)),
        };
        let mut buffer =
            vec![0; usize::try_from(size).map_or(MIB, |size| size.clamp(64 << 10, MIB))];
        let mut digest = digest::Context::new(&digest::SHA256);
        let mut offset = 0;
        loop {
            if stopped() {
                return None;
            }
            match fs.read_at(file.as_fd(), &mut buffer, offset) {
                Ok(0) => break,
                Ok(read) => {
                    digest.update(&buffer[..read]);
                    offset += read as u64;
                }
                Err(Errno::INTR) => {}
                Err(error) => return Some(Err(error)),
            }
        }
        Some(Ok(
            (digest.finish().as_ref() == contents.digest.as_ref()).then_some(file)
        ))
    }

    fn rmdir(&self, opened: Arc<OwnedFd>) {
        let deletion = &self.deletion;
        let fs = deletion.fs;
        // An open folder keeps its inode number even where numbers change
        // between the copy and the deletion, so the name must report it.
        let named = fs
            .stat(self.entry_at(), false)
            .and_then(|named| Ok((named, fs.stat_fd(opened.as_fd())?)));
        drop(opened);
        match named {
            Ok((named, opened)) if named.identity == opened.identity => {}
            Ok(_) => return deletion.keep(self.path.clone(), KeptCause::Changed),
            Err(error) => return deletion.failed(self.path.clone(), error),
        }
        match fs.unlink(self.entry_at(), AtFlags::REMOVEDIR) {
            Ok(()) | Err(Errno::NOENT) => {}
            // Entries added meanwhile stay, and so does their folder.
            Err(Errno::NOTEMPTY | Errno::EXIST) => {
                if deletion.holds_others(self.entry_at(), &self.path) {
                    deletion.keep(self.path.clone(), KeptCause::Changed);
                }
            }
            Err(error) => deletion.failed(self.path.clone(), error),
        }
    }
}
