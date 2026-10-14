// SPDX-License-Identifier: MIT

//! What a move copied of each source entry, as it was once the copy had
//! read it, so that afterwards only the entries the copy holds unchanged
//! are deleted. It keeps names, never descriptors. What an operation placed
//! is recorded the same way, for undo to delete only while unchanged.

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
use rustix::{
    fs::{AtFlags, FileType},
    io::Errno,
};

use super::{Failure, Mount, lock};
use crate::{
    adapters::local_operations::{
        LocalFileIdentity, bounded_local_worker_count, cancelled_local_operation,
        fs_ops::{Entry, EntryStat, FsOps, OffMain},
        io_error, local_device_is_rotational,
        work_queue::{self, Group, Job, WorkQueue},
    },
    adapters::volume::CoarseTimes,
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

/// Sleeps, or moves the time tests pretend it is on.
pub(super) fn sleep(duration: Duration) {
    #[cfg(test)]
    if let Some(pretended) = lock(&TEST_NOW).as_mut() {
        *pretended += duration;
        return;
    }
    thread::sleep(duration);
}

enum Waited {
    Settled,
    Stopped,
    /// The clock went back to before the copy read the source: a rewrite
    /// may then be stamped within the tick the copy read, and waiting would
    /// last as long as it went back.
    SetBack,
}

fn wait_until(unsettled: Unsettled, stopped: &dyn Fn() -> bool) -> Waited {
    loop {
        let now = now();
        if now < unsettled.read {
            return Waited::SetBack;
        }
        let left = unsettled.settled.duration_since(now).unwrap_or_default();
        if left.is_zero() {
            return Waited::Settled;
        }
        if stopped() {
            return Waited::Stopped;
        }
        #[cfg(test)]
        if let Some(pretended) = lock(&TEST_NOW).as_mut() {
            *pretended = unsettled.settled;
            continue;
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// How far the coarse clock filesystems stamp times with may run behind
/// ours: a kernel tick, at most 10 ms, with room to spare.
const COARSE_CLOCK: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
pub(super) struct Unsettled {
    read: SystemTime,
    settled: SystemTime,
    /// A modification time ahead of our clock, which stamps it: until that
    /// clock reaches it, no rewrite can have left it as it is.
    ahead: Option<SystemTime>,
}

pub(super) fn unsettled(
    stat: &EntryStat,
    coarse: Option<CoarseTimes>,
    started: SystemTime,
) -> Option<Unsettled> {
    let (seconds, nanoseconds) = stat.mtime;
    // A time with no fraction of a second may come from a filesystem as
    // coarse as FAT's that the mount does not show, such as one a server
    // shares.
    let coarse = coarse.or_else(|| {
        (nanoseconds == 0).then_some(CoarseTimes {
            window: Duration::from_secs(2),
            stamped_here: false,
        })
    })?;
    let modified =
        UNIX_EPOCH.checked_add(Duration::new(u64::try_from(seconds).ok()?, nanoseconds))?;
    // Our clock stamps a time ahead of it only once it reaches that time; a
    // server's, which may run ahead of ours, may stamp it now.
    let stamped = if coarse.stamped_here {
        modified
    } else {
        modified.min(started)
    };
    let settled = stamped.checked_add(coarse.window + COARSE_CLOCK)?;
    (started < settled).then_some(Unsettled {
        read: started,
        settled,
        ahead: (coarse.stamped_here && started < modified).then_some(modified),
    })
}

#[derive(Clone)]
pub(super) struct Contents {
    digest: digest::Digest,
    unsettled: Unsettled,
    /// How many leading bytes the digest covers, if not all of them.
    prefix: Option<u64>,
}

impl Contents {
    pub(super) fn new(digest: digest::Digest, unsettled: Unsettled) -> Self {
        Self {
            digest,
            unsettled,
            prefix: None,
        }
    }
}

pub(super) struct Read {
    pub stat: EntryStat,
    pub contents: Option<Contents>,
}

const MIB: usize = 1 << 20;

#[derive(Clone)]
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

    /// Removing a name changes the ctime and link count of the others.
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

pub(super) fn changed(before: &EntryStat, after: &EntryStat) -> bool {
    before.size != after.size || before.mtime != after.mtime || before.ctime != after.ctime
}

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

impl Node {
    fn copied(&self) -> Self {
        match self {
            Self::Entry(snapshot) => Self::Entry(snapshot.clone()),
            Self::Folder(folder) => Self::Folder(Arc::new(Folder {
                identity: folder.identity,
                children: Mutex::new(
                    lock(&folder.children)
                        .iter()
                        .map(|(name, node)| (name.clone(), node.copied()))
                        .collect(),
                ),
            })),
            Self::Kept(cause) => Self::Kept(cause.clone()),
        }
    }
}

impl Folder {
    pub(super) fn root() -> Arc<Self> {
        Arc::new(Self {
            identity: None,
            children: Mutex::default(),
        })
    }

    pub(super) fn folder(&self, name: &OsStr, identity: LocalFileIdentity) -> Arc<Self> {
        let folder = Arc::new(Self {
            identity: Some(identity),
            children: Mutex::default(),
        });
        self.add(name, Node::Folder(folder.clone()));
        folder
    }

    pub(super) fn entry(&self, name: &OsStr, stat: &EntryStat) {
        self.add(name, Node::Entry(SourceSnapshot::of(stat, None)));
    }

    pub(super) fn file(&self, name: &OsStr, read: Read) {
        self.add(
            name,
            Node::Entry(SourceSnapshot::of(&read.stat, read.contents)),
        );
    }

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

pub(in crate::adapters::local_operations) struct Manifest {
    pub(super) parent: OffMain<Arc<OwnedFd>>,
    pub(super) root: Arc<Folder>,
    copy: Option<Published>,
}

impl Manifest {
    pub(in crate::adapters::local_operations) fn copied_to(&mut self, copy: Published) {
        self.copy = Some(copy);
    }
}

pub(in crate::adapters::local_operations) struct Published {
    parent: OffMain<Arc<OwnedFd>>,
    name: OsString,
    identity: LocalFileIdentity,
}

impl Published {
    pub(super) fn at(fs: &dyn FsOps, parent: Arc<OwnedFd>, name: OsString) -> Result<Self, Errno> {
        let identity = fs.stat(Entry::new(&*parent, &name), false)?.identity;
        Ok(Self {
            parent: OffMain::new(parent),
            name,
            identity,
        })
    }

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

pub(super) fn snapshot(
    fs: &dyn FsOps,
    parent: Arc<OwnedFd>,
    name: &OsStr,
    cancelled: &AtomicBool,
) -> Result<Manifest, Failure> {
    let root = Folder::root();
    record(fs, parent.as_fd(), name, &root, cancelled, false)?;
    Ok(Manifest {
        parent: OffMain::new(parent),
        root,
        copy: None,
    })
}

/// What an operation put at a name, as it was once there.
pub(in crate::adapters::local_operations) struct Placed {
    name: OsString,
    node: Node,
}

impl Placed {
    /// A fresh record to delete by, as deleting takes what it deletes.
    pub(super) fn root(&self) -> Arc<Folder> {
        let root = Folder::root();
        root.add(&self.name, self.node.copied());
        root
    }
}

/// Run right after placing, so that it records what was placed.
pub(super) fn placed(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
    name: &OsStr,
    cancelled: &AtomicBool,
) -> Result<Placed, Failure> {
    let root = Folder::root();
    record(fs, dir, name, &root, cancelled, true)?;
    let (name, node) = root.take().pop().ok_or(Failure::Changed)?;
    Ok(Placed { name, node })
}

pub(super) fn of(parent: Arc<OwnedFd>, root: Arc<Folder>) -> Manifest {
    Manifest {
        parent: OffMain::new(parent),
        root,
        copy: None,
    }
}

/// With `contents`, a file within a tick of its mount's coarse times is
/// digested, for a deletion to compare.
fn record(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
    name: &OsStr,
    folder: &Folder,
    cancelled: &AtomicBool,
    contents: bool,
) -> Result<(), Failure> {
    if cancelled.load(Ordering::Acquire) {
        return Err(Failure::Cancelled);
    }
    let at = Entry { dir, name };
    let stat = fs.stat(at, false)?;
    if contents && stat.kind == FileType::RegularFile {
        folder.add(name, digested(fs, at, &stat, cancelled)?);
        return Ok(());
    }
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
        record(fs, opened.as_fd(), &child, &recorded, cancelled, contents)
            .map_err(|failure| failure.named(&child))?;
    }
    Ok(())
}

fn digested(
    fs: &dyn FsOps,
    at: Entry<'_>,
    stat: &EntryStat,
    cancelled: &AtomicBool,
) -> Result<Node, Failure> {
    let coarse = Mount::new(fs, at.dir).coarse_times;
    let Some(unsettled) = unsettled(stat, coarse, now()) else {
        return Ok(Node::Entry(SourceSnapshot::of(stat, None)));
    };
    // Once the tick passes, any rewrite changes the times, so the bytes not
    // read by then need no digest.
    let stopped = || cancelled.load(Ordering::Acquire) || now() >= unsettled.settled;
    let read = || -> Result<Option<Option<Contents>>, Errno> {
        let Some((file, _)) = fs.open_source(at, false)? else {
            return Ok(None);
        };
        let end = stat.size.saturating_add(1);
        let (digest, prefix) = digest_of(fs, file.as_fd(), end, &stopped)?;
        let after = fs.stat_fd(file.as_fd())?;
        let contents = (prefix != Some(0)).then_some(Contents {
            digest,
            unsettled,
            prefix,
        });
        Ok((after.identity == stat.identity && !changed(stat, &after)).then_some(contents))
    };
    let read = read();
    if cancelled.load(Ordering::Acquire) {
        return Err(Failure::Cancelled);
    }
    Ok(match read {
        Ok(Some(contents)) => Node::Entry(SourceSnapshot::of(stat, contents)),
        // Nothing tells what was placed, so a deletion keeps it.
        _ => Node::Kept(KeptCause::Changed),
    })
}

/// The digest of the file's first `end` bytes, or of all of it if shorter,
/// with how many bytes it read if `stopped` cut it short.
fn digest_of(
    fs: &dyn FsOps,
    file: BorrowedFd<'_>,
    end: u64,
    stopped: &dyn Fn() -> bool,
) -> Result<(digest::Digest, Option<u64>), Errno> {
    let mut buffer = vec![0; usize::try_from(end).map_or(MIB, |end| end.clamp(64 << 10, MIB))];
    let mut digest = digest::Context::new(&digest::SHA256);
    let mut offset = 0;
    while offset < end {
        if stopped() {
            return Ok((digest.finish(), Some(offset)));
        }
        let wanted =
            usize::try_from(end - offset).map_or(buffer.len(), |left| left.min(buffer.len()));
        match fs.read_at(file, &mut buffer[..wanted], offset) {
            Ok(0) => break,
            Ok(read) => {
                digest.update(&buffer[..read]);
                offset += read as u64;
            }
            Err(Errno::INTR) => {}
            Err(error) => return Err(error),
        }
    }
    Ok((digest.finish(), None))
}

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
    let removed = remove(fs, parent.into_inner(), &root, unlinks, cancelled);
    (removed.kept, removed.result)
}

pub(in crate::adapters::local_operations) struct Removed {
    pub kept: Vec<(PathBuf, KeptCause)>,
    /// Whether any entry was deleted.
    pub removed: bool,
    pub result: Result<(), glib::Error>,
}

/// Deletes the recorded entries that are still as recorded.
pub(super) fn remove(
    fs: &'static dyn FsOps,
    parent: Arc<OwnedFd>,
    root: &Folder,
    unlinks: Arc<Unlinks>,
    cancelled: Arc<AtomicBool>,
) -> Removed {
    walk(fs, parent, root, unlinks, cancelled, false)
}

/// Keeps what [`remove`] would keep, and folders holding entries added
/// since, without deleting anything.
pub(super) fn compare(
    fs: &'static dyn FsOps,
    parent: Arc<OwnedFd>,
    root: &Folder,
    unlinks: Arc<Unlinks>,
    cancelled: Arc<AtomicBool>,
) -> Removed {
    walk(fs, parent, root, unlinks, cancelled, true)
}

fn walk(
    fs: &'static dyn FsOps,
    parent: Arc<OwnedFd>,
    root: &Folder,
    unlinks: Arc<Unlinks>,
    cancelled: Arc<AtomicBool>,
    compare_only: bool,
) -> Removed {
    let deletion = Arc::new(Deletion {
        fs,
        stable_inodes: Mount::keeps_inode_numbers(fs, parent.as_fd()),
        unlinks,
        kept: Mutex::default(),
        removed: AtomicBool::new(false),
        compare_only,
    });
    let steps = deletion.steps(root, Path::new(""));
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
    Removed {
        kept: std::mem::take(&mut *lock(&deletion.kept)),
        removed: deletion.removed.load(Ordering::Acquire),
        result: queue.result(),
    }
}

struct Deletion {
    fs: &'static dyn FsOps,
    /// FUSE daemons without `use_ino` and cifs without server inode
    /// numbers number an entry afresh once the kernel evicts it.
    stable_inodes: bool,
    unlinks: Arc<Unlinks>,
    kept: Mutex<Vec<(PathBuf, KeptCause)>>,
    removed: AtomicBool,
    compare_only: bool,
}

impl Deletion {
    fn keep(&self, path: PathBuf, cause: KeptCause) {
        lock(&self.kept).push((path, cause));
    }

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

    /// Whether the folder lists a name its record lacks.
    fn added_to(&self, opened: BorrowedFd<'_>, folder: &Folder) -> Result<bool, Errno> {
        let recorded: HashSet<OsString> = lock(&folder.children)
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        Ok(self
            .fs
            .list(opened)?
            .iter()
            .any(|(name, _)| !recorded.contains(name)))
    }

    fn failed(&self, path: PathBuf, error: Errno) {
        match error {
            // The copy holds it, and its source is gone, as a move leaves it.
            Errno::NOENT => {
                tracing::debug!(?path, "a moved source was gone before it was deleted");
            }
            error => self.keep(path, KeptCause::Undeletable(error.raw_os_error())),
        }
    }

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

struct At {
    deletion: Arc<Deletion>,
    parent: Arc<OwnedFd>,
    name: OsString,
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
            .and_then(|fd| fs.stat_fd(fd.as_fd()).map(|stat| (fd, stat)))
            .and_then(|(fd, stat)| {
                let added = deletion.compare_only && deletion.added_to(fd.as_fd(), folder)?;
                Ok((fd, stat, added))
            });
        let fd = match opened {
            Ok((fd, current, false)) if deletion.same(&current, folder.identity) => Arc::new(fd),
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
        let ahead = snapshot
            .contents
            .as_ref()
            .and_then(|contents| contents.unsettled.ahead)
            .filter(|&modified| now() < modified);
        // Compared before the stat below, which sees any later rewrite.
        let compared = match &snapshot.contents {
            Some(contents) if ahead.is_none() => {
                match self.same_contents(contents, snapshot.size, stopped) {
                    None => return,
                    Some(Ok(Some(file))) => Some(file),
                    Some(Ok(None)) => return deletion.keep(self.path.clone(), KeptCause::Changed),
                    Some(Err(error)) => return deletion.failed(self.path.clone(), error),
                }
            }
            _ => None,
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
        // Left uncompared only while the clock has not reached its time.
        let still_ahead = ahead.is_none_or(|modified| now() < modified);
        let unchanged = compared_here
            && still_ahead
            && current
                .as_ref()
                .is_ok_and(|current| snapshot.holds(current, deletion.stable_inodes, since));
        match current {
            Ok(_) if unchanged && deletion.compare_only => {}
            Ok(current) if unchanged => match fs.unlink(self.entry_at(), AtFlags::empty()) {
                Ok(()) => {
                    deletion.removed.store(true, Ordering::Release);
                    if let Some(unlinks) = &mut unlinks {
                        unlinks.entry(key).or_default().push(current.nlink);
                    }
                }
                Err(error) => deletion.failed(self.path.clone(), error),
            },
            Ok(_) => deletion.keep(self.path.clone(), KeptCause::Changed),
            Err(error) => deletion.failed(self.path.clone(), error),
        }
    }

    /// The file, still open, if it holds what the copy holds; `None` if the
    /// deletion stopped meanwhile.
    fn same_contents(
        &self,
        contents: &Contents,
        size: u64,
        stopped: &dyn Fn() -> bool,
    ) -> Option<Result<Option<OwnedFd>, Errno>> {
        match wait_until(contents.unsettled, stopped) {
            Waited::Settled => {}
            Waited::Stopped => return None,
            Waited::SetBack => return Some(Ok(None)),
        }
        let fs = self.deletion.fs;
        let file = match fs.open_source(self.entry_at(), false) {
            Ok(Some((file, _))) => file,
            Ok(None) => return Some(Ok(None)),
            Err(error) => return Some(Err(error)),
        };
        // A byte past what the copy holds tells a file that grew.
        let end = contents.prefix.unwrap_or(size.saturating_add(1));
        match digest_of(fs, file.as_fd(), end, stopped) {
            Ok((_, Some(_))) => None,
            Ok((digest, None)) => Some(Ok(
                (digest.as_ref() == contents.digest.as_ref()).then_some(file)
            )),
            Err(error) => Some(Err(error)),
        }
    }

    fn rmdir(&self, opened: Arc<OwnedFd>) {
        let deletion = &self.deletion;
        if deletion.compare_only {
            return;
        }
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
            Ok(()) => deletion.removed.store(true, Ordering::Release),
            Err(Errno::NOENT) => {}
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
