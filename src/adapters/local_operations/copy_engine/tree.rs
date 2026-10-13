// SPDX-License-Identifier: MIT

//! Copies a folder's tree on the shared work queue. Entries are made
//! exclusively, so in a folder this copy made a taken name is a case alias,
//! someone else's entry, or the copy's own entry whose reply was lost. Each
//! folder gets its metadata once its last child is done.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use gtk::glib;
use rustix::{
    fs::{AtFlags, FileType, Mode},
    io::Errno,
};

use super::{
    Copy, Created, Failure, Mount, best_effort, changed,
    data::Data,
    is_fatal, is_unsupported,
    links::{self, Links, Role},
    lock,
    metadata::Snapshot,
    open_file,
    restrictive::Restrictive,
    same_entry, stable_inodes, stage,
};
use crate::{
    adapters::{
        local_operations::{
            LocalFileIdentity, cancelled_local_operation, fat_family_child_name,
            fs_ops::{Entry, EntryStat, FsOps},
            io_error, numbered_names, placement,
            placement::PlaceError,
            work_queue::{self, Group, Job, WorkQueue},
        },
        volume::Family,
    },
    services::SpecialKind,
};

/// `FS_CASEFOLD_FL`, which folders made inside a casefolded one inherit.
const CASEFOLD: u32 = 0x4000_0000;

type Queue = WorkQueue<TreeJob>;
type Completion = Option<Arc<Group<TreeJob>>>;

/// What every job of one walk shares.
pub(super) struct Tree {
    copy: Arc<Copy>,
    restrictive: Mutex<Vec<Restrictive>>,
    links: Links,
    /// The next link scope a merged folder takes.
    link_scopes: AtomicUsize,
    created: Option<Arc<Created>>,
}

impl Tree {
    /// Where a mount is probed for links, outside the copy.
    pub(super) fn probe_links_in(&self, mount: &Mount, dir: Arc<OwnedFd>) {
        self.links.probe_in(mount.key, dir);
    }

    fn made(&self, path: &Path, identity: LocalFileIdentity) {
        if let Some(created) = &self.created {
            created.add(path.to_path_buf(), identity);
        }
    }
}

pub(super) enum TreeJob {
    Folder(Arc<Tree>, Folder),
    File(Arc<Tree>, File),
    Finish(Arc<Tree>, Finish),
    Merge(Arc<Tree>, Merged),
}

impl Job for TreeJob {
    type Error = glib::Error;

    fn run(self, queue: &Queue) {
        if queue.is_stopped() {
            return;
        }
        match self {
            Self::Folder(tree, folder) => folder.run(&tree, queue),
            Self::File(tree, file) => file.run(&tree, queue),
            Self::Finish(tree, finish) => finish.run(&tree, queue),
            Self::Merge(tree, merged) => merged.run(&tree, queue),
        }
    }

    fn cancelled() -> glib::Error {
        cancelled_local_operation()
    }

    fn panicked() -> glib::Error {
        io_error("Copy worker panicked")
    }

    fn not_started(error: io::Error) -> glib::Error {
        io_error(format!("Could not start copy worker: {error}"))
    }
}

/// What a walk leaves for the passes after it.
pub(super) struct Walked {
    /// Folders left open to their owner until their exact modes are set.
    pub(super) restrictive: Vec<Restrictive>,
    pub(super) links: Links,
}

/// Runs the walk that `root` starts on `workers` threads, recording what it
/// makes in `created`.
pub(super) fn walk(
    copy: &Arc<Copy>,
    workers: usize,
    created: Option<Arc<Created>>,
    root: impl FnOnce(Arc<Tree>) -> TreeJob,
) -> Result<Walked, glib::Error> {
    let tree = Arc::new(Tree {
        copy: copy.clone(),
        restrictive: Mutex::default(),
        links: Links::default(),
        link_scopes: AtomicUsize::new(0),
        created,
    });
    let queue = Arc::new(Queue::new(copy.cancelled.clone()));
    queue.enqueue(root(tree.clone()));
    work_queue::run_workers(&queue, workers.max(1), None, |work| {
        thread::Builder::new().spawn(work)
    });
    queue.result()?;
    Ok(Walked {
        restrictive: std::mem::take(&mut *lock(&tree.restrictive)),
        links: tree.links.take(),
    })
}

/// How the files of a made folder get their names.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Naming {
    /// Decided from the made folder itself.
    Decide,
    /// Each file job creates its file.
    Create,
    /// The filesystem may take two names for one, so names that may collide
    /// are claimed in byte order before their files are written. A file job
    /// adopts a file its own lost reply left, as NFS creates are not
    /// idempotent.
    Claim,
    /// A folder a merge made in the live tree, where a claim that a stopped
    /// merge left would be an empty file at a real name.
    Live,
}

impl Naming {
    fn decide(self, fs: &dyn FsOps, family: Family, folder: BorrowedFd<'_>) -> Self {
        if self != Self::Decide {
            return self;
        }
        let never_collide = stage::names_never_collide(family)
            && fs
                .file_flags(folder)
                .is_ok_and(|flags| flags & CASEFOLD == 0);
        if never_collide {
            Self::Create
        } else {
            Self::Claim
        }
    }
}

/// The names of a folder this copy made, against which its aliases are
/// numbered. A numbered name never takes a name an entry of the folder asks
/// for, and each is tried by one job only, so a name found taken is never
/// another job's entry mistaken for one whose reply was lost.
pub(super) struct Names {
    requested: Vec<OsString>,
    keys: OnceLock<HashSet<OsString>>,
    numbered: Mutex<HashSet<OsString>>,
}

impl Names {
    fn new(requested: Vec<OsString>) -> Arc<Self> {
        Arc::new(Self {
            requested,
            keys: OnceLock::new(),
            numbered: Mutex::default(),
        })
    }

    /// Makes with `make` the first numbered name free for `name`, and
    /// returns that name.
    fn number<T>(
        &self,
        name: &OsStr,
        is_directory: bool,
        mut make: impl FnMut(&OsStr) -> Result<T, Failure>,
    ) -> Result<(OsString, T), Failure> {
        let requested = self.keys.get_or_init(|| {
            self.requested
                .iter()
                .map(|name| stage::collision_key(name))
                .collect()
        });
        for candidate in numbered_names(name, is_directory) {
            let key = stage::collision_key(&candidate);
            if requested.contains(&key) || !lock(&self.numbered).insert(key) {
                continue;
            }
            match make(&candidate) {
                Err(Failure::Place(PlaceError::Exists)) => {}
                made => return made.map(|value| (candidate, value)),
            }
        }
        Err(Failure::Place(PlaceError::Exists))
    }
}

/// Makes the entry at `path`, beneath the walk's root, with `make`; where
/// its name is taken in a folder this copy made, makes it under the next
/// free numbered name and reports it saved there. `names` is `None` in a
/// folder of the live tree. Returns the path the entry took.
pub(super) fn make_numbered<T>(
    copy: &Copy,
    names: Option<&Names>,
    path: &Path,
    is_directory: bool,
    mut make: impl FnMut(&OsStr) -> Result<T, Failure>,
) -> Result<(PathBuf, T), Failure> {
    let name = path.file_name().unwrap_or_default();
    match (make(name), names) {
        (Err(Failure::Place(PlaceError::Exists)), Some(names)) => {
            let (saved, made) = names.number(name, is_directory, make)?;
            let saved = path.with_file_name(saved);
            copy.renamed(path.to_path_buf(), saved.clone());
            Ok((saved, made))
        }
        (made, _) => made.map(|value| (path.to_path_buf(), value)),
    }
}

/// A source folder and the folder this copy made for it, both opened when
/// the job runs.
pub(super) struct Folder {
    /// The source's parent and name, and its identity where known.
    pub(super) source: (Arc<OwnedFd>, OsString, Option<LocalFileIdentity>),
    /// The made folder's parent and name, and its identity.
    pub(super) target: (Arc<OwnedFd>, OsString, LocalFileIdentity),
    /// The made folder's parent, whose setgid bit it follows.
    pub(super) parent: EntryStat,
    /// Beneath the walk's root.
    pub(super) path: PathBuf,
    pub(super) naming: Naming,
    pub(super) mount: Mount,
    /// Names are linked only to names of the same scope: the whole made
    /// tree, or one merged folder.
    pub(super) link_scope: usize,
    pub(super) completion: Completion,
}

impl Folder {
    fn run(self, tree: &Arc<Tree>, queue: &Queue) {
        let name = self.source.1.clone();
        if let Err(failure) = self.fill(tree, queue) {
            queue.fail(failure.into_error(&name));
        }
    }

    fn fill(self, tree: &Arc<Tree>, queue: &Queue) -> Result<(), Failure> {
        let copy = &tree.copy;
        let fs = copy.fs;
        let (source_parent, source_name, expected) = &self.source;
        let source = fs
            .open_dir(Entry::new(&**source_parent, source_name))
            .map_err(|error| changed(source_name, error))?;
        let source_stat = fs.stat_fd(source.as_fd())?;
        if expected.is_some_and(|expected| expected != source_stat.identity) {
            return Err(Failure::Changed);
        }
        let (target_parent, target_name, created) = &self.target;
        let target = fs.open_dir(Entry::new(&**target_parent, target_name))?;
        let target_stat = fs.stat_fd(target.as_fd())?;
        if !same_entry(&target_stat, *created, self.mount.family) {
            return Err(Failure::Place(PlaceError::Changed));
        }
        let snapshot = Snapshot::read(fs, source.as_fd(), &source_stat)?;
        let naming = self.naming.decide(fs, self.mount.family, target.as_fd());
        let mut used = HashSet::new();
        let children = children(fs, source.as_fd())?
            .into_iter()
            .map(|(name, kind)| {
                let target_name = fat_family_child_name(&name, copy.fat_family, &mut used);
                (name, kind, target_name)
            })
            .collect::<Vec<_>>();
        let colliding = if naming == Naming::Claim {
            let names = children
                .iter()
                .map(|(_, _, name)| name.as_os_str())
                .collect::<Vec<_>>();
            stage::colliding(&names)
        } else {
            vec![false; children.len()]
        };
        let names = (naming != Naming::Live)
            .then(|| Names::new(children.iter().map(|(_, _, name)| name.clone()).collect()));
        let made = Made {
            dir: Arc::new(target),
            stat: target_stat,
            path: self.path,
            naming,
            names,
            mount: self.mount,
            link_scope: self.link_scope,
        };
        let finish = Finish {
            name: source_name.clone(),
            target: made.dir.clone(),
            created: target_stat,
            parent: self.parent,
            snapshot,
            path: made.path.clone(),
            mount: made.mount,
            completion: self.completion,
        };
        if children.is_empty() {
            finish.run(tree, queue);
            return Ok(());
        }
        let group = Group::new(children.len(), TreeJob::Finish(tree.clone(), finish));
        let source = Arc::new(source);
        for ((name, kind, target_name), colliding) in children.into_iter().zip(colliding) {
            copy.check_cancelled()?;
            let child = Child {
                name: &name,
                kind,
                target_name: &target_name,
                claim: colliding,
            };
            made.make(tree, queue, &source, &child, &group)
                .map_err(|failure| failure.named(&name))?;
        }
        Ok(())
    }
}

/// A target folder this copy made, while its children are made.
struct Made {
    dir: Arc<OwnedFd>,
    stat: EntryStat,
    path: PathBuf,
    naming: Naming,
    /// `None` where the folder is in the live tree.
    names: Option<Arc<Names>>,
    mount: Mount,
    link_scope: usize,
}

/// A source folder's child and the name it gets in the made folder.
struct Child<'a> {
    name: &'a OsStr,
    kind: FileType,
    target_name: &'a OsStr,
    /// The name may collide with a sibling's, so a file's name is claimed.
    claim: bool,
}

impl Made {
    /// Makes the target of `child`: a folder, a link, or a file's claimed
    /// name, and queues what fills it.
    fn make(
        &self,
        tree: &Arc<Tree>,
        queue: &Queue,
        source: &Arc<OwnedFd>,
        child: &Child<'_>,
        group: &Arc<Group<TreeJob>>,
    ) -> Result<(), Failure> {
        let copy = &tree.copy;
        let fs = copy.fs;
        let name = child.name;
        let path = self.path.join(child.target_name);
        let names = self.names.as_deref();
        let dir = &*self.dir;
        match child.kind {
            FileType::Directory => {
                let (path, created) = make_numbered(copy, names, &path, true, |name| {
                    claim_folder(fs, Entry::new(dir, name), true)
                })?;
                tree.made(&path, created);
                queue.enqueue(TreeJob::Folder(
                    tree.clone(),
                    Folder {
                        source: (source.clone(), name.to_os_string(), None),
                        target: (self.dir.clone(), file_name(&path), created),
                        parent: self.stat,
                        path,
                        naming: self.naming,
                        mount: self.mount,
                        link_scope: self.link_scope,
                        completion: Some(group.clone()),
                    },
                ));
            }
            FileType::RegularFile => {
                let (path, claimed) = if child.claim {
                    let (path, identity) = make_numbered(copy, names, &path, false, |name| {
                        claim_file(copy, Entry::new(dir, name))
                    })?;
                    tree.made(&path, identity);
                    (path, Some(identity))
                } else {
                    (path, None)
                };
                queue.enqueue(TreeJob::File(
                    tree.clone(),
                    File {
                        source: source.clone(),
                        name: name.to_os_string(),
                        target: self.dir.clone(),
                        parent: self.stat,
                        claimed,
                        adopts: self.naming == Naming::Claim,
                        names: self.names.clone(),
                        mount: self.mount,
                        path,
                        link_scope: self.link_scope,
                        completion: Some(group.clone()),
                    },
                ));
            }
            FileType::Symlink => {
                let source = Entry::new(&**source, name);
                let (path, ()) = make_numbered(copy, names, &path, false, |name| {
                    copy_link(copy, source, Entry::new(dir, name), &self.mount, true)
                })?;
                if tree.created.is_some() {
                    let made = fs.stat(Entry::new(dir, &file_name(&path)), false)?;
                    tree.made(&path, made.identity);
                }
                queue.complete(Some(group.clone()));
            }
            FileType::Fifo => {
                let source = Entry::new(&**source, name);
                let (path, made) = make_numbered(copy, names, &path, false, |name| {
                    copy_fifo(
                        copy,
                        source,
                        Entry::new(dir, name),
                        &self.stat,
                        &self.mount,
                        true,
                    )
                })?;
                match made {
                    Some(identity) => tree.made(&path, identity),
                    None => copy.skip(path, SpecialKind::Pipe)?,
                }
                queue.complete(Some(group.clone()));
            }
            kind => {
                copy.skip(path, special_kind(kind))?;
                queue.complete(Some(group.clone()));
            }
        }
        Ok(())
    }
}

/// How a special file other than a named pipe is reported.
fn special_kind(kind: FileType) -> SpecialKind {
    if kind == FileType::Socket {
        SpecialKind::Socket
    } else {
        SpecialKind::Device
    }
}

/// The last component of a path beneath the walk's root.
fn file_name(path: &Path) -> OsString {
    path.file_name().unwrap_or_default().to_os_string()
}

/// The folder's entries in byte order with their types.
fn children(fs: &dyn FsOps, folder: BorrowedFd<'_>) -> Result<Vec<(OsString, FileType)>, Failure> {
    let mut children = fs.list(folder)?;
    children.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, kind) in &mut children {
        if *kind == FileType::Unknown {
            *kind = fs
                .stat(Entry { dir: folder, name }, false)
                .map_err(|error| Failure::Os(error).named(name))?
                .kind;
        }
    }
    Ok(children)
}

/// Whether making `entry` failed because its name is taken: sshfs reports a
/// collision as `EPERM`, and SMB a folder in a file's way as `EISDIR`.
pub(super) fn taken(fs: &dyn FsOps, entry: Entry<'_>, error: Errno) -> bool {
    match error {
        Errno::EXIST | Errno::ISDIR => true,
        Errno::PERM => fs.stat(entry, false).is_ok(),
        _ => false,
    }
}

/// The taken `entry` in a folder this copy `made` when it is this copy's own
/// whose reply was lost, as NFS creations are not idempotent: listed under
/// exactly that name, which no other job of the copy tries, and still as
/// fresh as making it leaves it.
pub(super) fn adopt(
    fs: &dyn FsOps,
    entry: Entry<'_>,
    made: bool,
    fresh: impl FnOnce(&EntryStat) -> bool,
) -> Result<LocalFileIdentity, Failure> {
    let ours = || {
        if !made {
            return None;
        }
        let listed = fs
            .names(Entry {
                dir: entry.dir,
                name: OsStr::new("."),
            })
            .ok()?
            .iter()
            .any(|name| name == entry.name);
        let stat = fs.stat(entry, true).ok()?;
        (listed && fresh(&stat)).then_some(stat.identity)
    };
    ours().ok_or(Failure::Place(PlaceError::Exists))
}

fn claim_folder(
    fs: &dyn FsOps,
    entry: Entry<'_>,
    made: bool,
) -> Result<LocalFileIdentity, Failure> {
    match fs.mkdir(entry, Mode::from_raw_mode(0o700)) {
        Ok(()) => Ok(fs.stat(entry, false)?.identity),
        Err(error) if taken(fs, entry, error) => adopt(fs, entry, made, |stat| {
            stat.is_directory() && fs.names(entry).is_ok_and(|names| names.is_empty())
        }),
        Err(error) => Err(error.into()),
    }
}

/// Claims a file's name without opening it for writing: vfat mounted with
/// `flush` waits at every such close, and the file job opens it once more.
fn claim_file(copy: &Copy, entry: Entry<'_>) -> Result<LocalFileIdentity, Failure> {
    let fs = copy.fs;
    if !copy.nodes_refused.load(Ordering::Relaxed) {
        match fs.create_node(entry, FileType::RegularFile, Mode::from_raw_mode(0o600)) {
            Ok(()) => return Ok(fs.stat(entry, false)?.identity),
            Err(error) if taken(fs, entry, error) => {
                return adopt(fs, entry, true, fresh_file);
            }
            Err(error)
                if error == Errno::PERM || error == Errno::NOSYS || is_unsupported(error) =>
            {
                copy.nodes_refused.store(true, Ordering::Relaxed);
            }
            Err(error) => return Err(error.into()),
        }
    }
    match fs.create_readonly(entry, Mode::from_raw_mode(0o600)) {
        Ok(fd) => {
            let identity = fs.stat_fd(fd.as_fd())?.identity;
            fs.close(fd)?;
            Ok(identity)
        }
        Err(error) if taken(fs, entry, error) => adopt(fs, entry, true, fresh_file),
        Err(error) => Err(error.into()),
    }
}

/// As fresh as creating a file leaves it.
fn fresh_file(stat: &EntryStat) -> bool {
    stat.kind == FileType::RegularFile && stat.size == 0 && stat.nlink == 1
}

pub(super) fn read_link(fs: &dyn FsOps, at: Entry<'_>) -> Result<OsString, Failure> {
    fs.readlink(at).map_err(|error| match error {
        Errno::INVAL => Failure::Changed,
        error => error.into(),
    })
}

/// Recreates the symlink at `source` as `target`, in a folder this copy
/// `made` or not, with its times, and counts it with its target's length
/// as its bytes.
pub(super) fn copy_link(
    copy: &Copy,
    source: Entry<'_>,
    target: Entry<'_>,
    mount: &Mount,
    made: bool,
) -> Result<(), Failure> {
    let fs = copy.fs;
    let stat = fs.stat(source, false)?;
    if stat.kind != FileType::Symlink {
        return Err(Failure::Changed);
    }
    let link = read_link(fs, source)?;
    if let Err(error) = fs.symlink(&link, target) {
        if !taken(fs, target, error) {
            return Err(error.into());
        }
        adopt(fs, target, made, |stat| {
            stat.kind == FileType::Symlink
                && fs.readlink(target).is_ok_and(|current| current == link)
        })?;
    }
    // sshfs sets a link's times on what it points to.
    if mount.family != Family::Fuse
        && let Err(error) = best_effort(fs.set_link_times(target, stat.atime, stat.mtime))
    {
        // The copy failed, so its link must not stay at a real name.
        if fs.readlink(target).is_ok_and(|current| current == link) {
            let _ = fs.unlink(target, AtFlags::empty());
        }
        return Err(error.into());
    }
    copy.add_file(stat.size);
    Ok(())
}

/// Recreates the named pipe at `source` as `target`, in a folder this copy
/// `made` or not, with its owner, mode and times, and counts it as a file;
/// `None` where the destination refuses named pipes. Reading the source's
/// xattrs would open it, which lets a writer waiting on it go on.
pub(super) fn copy_fifo(
    copy: &Copy,
    source: Entry<'_>,
    target: Entry<'_>,
    parent: &EntryStat,
    mount: &Mount,
    made: bool,
) -> Result<Option<LocalFileIdentity>, Failure> {
    let fs = copy.fs;
    let stat = fs.stat(source, false)?;
    if stat.kind != FileType::Fifo {
        return Err(Failure::Changed);
    }
    match fs.create_node(target, FileType::Fifo, Mode::from_raw_mode(0o600)) {
        Ok(()) => {}
        Err(error) if taken(fs, target, error) => {
            adopt(fs, target, made, |stat| stat.kind == FileType::Fifo)?;
        }
        Err(error) if refuses_pipes(error) => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let created = match fs.open_fifo(target) {
        Ok(fd) => {
            let created = fs.stat_fd(fd.as_fd())?;
            if created.kind != FileType::Fifo {
                return Err(Failure::Place(PlaceError::Changed));
            }
            Snapshot::of(&stat).apply_to_pipe(
                fs,
                fd.as_fd(),
                &created,
                parent,
                mount,
                &copy.refused_xattrs,
            )?;
            best_effort(fs.close(fd))?;
            created
        }
        Err(error) if is_fatal(error) => return Err(error.into()),
        Err(_) => fs.stat(target, false)?,
    };
    copy.add_file(0);
    Ok(Some(created.identity))
}

/// How filesystems without named pipes refuse one: vfat with `EPERM`,
/// exfat-fuse with `EIO` and SMB with `EOPNOTSUPP`.
fn refuses_pipes(error: Errno) -> bool {
    matches!(error, Errno::PERM | Errno::IO | Errno::NOSYS | Errno::INVAL) || is_unsupported(error)
}

/// A regular file copied into a folder through its own descriptor.
pub(super) struct File {
    source: Arc<OwnedFd>,
    name: OsString,
    target: Arc<OwnedFd>,
    /// The folder the file is made in, whose setgid bit it follows.
    parent: EntryStat,
    /// What the folder job claimed; `None` makes the file exclusively.
    claimed: Option<LocalFileIdentity>,
    /// A taken name may be the file's own, from a lost reply.
    adopts: bool,
    /// The names of the folder, where this copy made it.
    names: Option<Arc<Names>>,
    mount: Mount,
    /// Beneath the walk's root: where it was claimed, or the name it asks for.
    path: PathBuf,
    link_scope: usize,
    completion: Completion,
}

impl File {
    fn run(self, tree: &Arc<Tree>, queue: &Queue) {
        #[cfg(test)]
        let _activity = crate::adapters::local_operations::tests::CopyActivity::start();
        match tree.copy.with_data(|data| self.copy(tree, data)) {
            Ok(role) => {
                if role != Role::Later {
                    tree.copy.add_file(0);
                }
                queue.complete(self.completion);
            }
            Err(failure) => queue.fail(failure.into_error(&self.name)),
        }
    }

    fn copy(&self, tree: &Tree, data: &mut Data) -> Result<Role, Failure> {
        let copy = &*tree.copy;
        let fs = copy.fs;
        copy.check_cancelled()?;
        if copy.counted {
            copy.progress.start_file(&self.name);
        }
        let source = open_file(fs, Entry::new(&*self.source, &self.name))?;
        let key = (
            self.link_scope,
            source.stat.identity.device,
            source.stat.identity.inode,
        );
        let role = if source.stat.nlink > 1 {
            let name = links::Name {
                key,
                path: self.path.clone(),
                source: self.name.clone(),
                claimed: self.claimed,
                names: self.names.clone(),
                size: source.stat.size,
            };
            tree.links.role(fs, &self.mount, name)
        } else {
            Role::Copied
        };
        if role == Role::Later {
            return Ok(role);
        }
        let dir = &*self.target;
        let (path, (fd, expected)) = match self.claimed {
            Some(claimed) => {
                let fd = open_made(fs, Entry::new(dir, &file_name(&self.path)))?;
                (self.path.clone(), (fd, Some(claimed)))
            }
            None => make_numbered(copy, self.names.as_deref(), &self.path, false, |name| {
                let target = Entry::new(dir, name);
                match fs.create_exclusive(target, Mode::from_raw_mode(0o600)) {
                    Ok(fd) => Ok((fd, None)),
                    Err(error) if !taken(fs, target, error) => Err(error.into()),
                    Err(_) if self.adopts => {
                        let identity = adopt(fs, target, true, fresh_file)?;
                        tree.made(&self.path.with_file_name(name), identity);
                        Ok((open_made(fs, target)?, Some(identity)))
                    }
                    Err(_) => Err(Failure::Place(PlaceError::Exists)),
                }
            })?,
        };
        let name = file_name(&path);
        let target = Entry::new(dir, &name);
        let created = fs.stat_fd(fd.as_fd())?;
        match expected {
            Some(expected) => {
                let ours = if stable_inodes(self.mount.family) {
                    created.identity == expected
                } else {
                    created.kind == FileType::RegularFile && created.size == 0
                };
                if !ours {
                    return Err(Failure::Place(PlaceError::Changed));
                }
            }
            None => tree.made(&path, created.identity),
        }
        // Takes `fd`, so a failed file is closed before it is removed: NFS
        // renames an open file aside instead.
        let written = (|fd: OwnedFd| -> Result<(), Failure> {
            copy.copy_data(
                data,
                &self.mount,
                (source.fd.as_fd(), &source.stat),
                fd.as_fd(),
                &self.parent,
                true,
            )?;
            let snapshot = Snapshot::read(fs, source.fd.as_fd(), &source.stat)?;
            copy.apply_metadata(fd.as_fd(), &created, &snapshot, &self.parent, &self.mount)?;
            fs.close(fd)?;
            copy.settle_times(target, created.identity, &snapshot, self.mount.family)
        })(fd);
        if written.is_err() {
            placement::remove_if_unchanged(fs, target, created.identity, AtFlags::empty());
        }
        written?;
        if role == Role::First {
            tree.links.written(key, path, created.identity);
        }
        Ok(role)
    }
}

/// Opens a file this copy made earlier, for writing.
fn open_made(fs: &dyn FsOps, target: Entry<'_>) -> Result<OwnedFd, Failure> {
    fs.open_write(target).map_err(|error| match error {
        // A pipe without a reader took the file's place.
        Errno::NXIO => Failure::Place(PlaceError::Changed),
        error => error.into(),
    })
}

/// A made folder's own metadata, once its last child is done.
pub(super) struct Finish {
    /// The source folder's name, for errors.
    name: OsString,
    target: Arc<OwnedFd>,
    created: EntryStat,
    parent: EntryStat,
    snapshot: Snapshot,
    path: PathBuf,
    mount: Mount,
    completion: Completion,
}

impl Finish {
    fn run(self, tree: &Arc<Tree>, queue: &Queue) {
        let copy = &tree.copy;
        let applied = self.snapshot.apply_to_folder(
            copy.fs,
            self.target.as_fd(),
            &self.created,
            &self.parent,
            &self.mount,
            &copy.refused_xattrs,
        );
        match applied {
            Ok(mode) => {
                tree.links
                    .retime(&self.path, self.created.identity, self.snapshot);
                if let Some(mode) = mode {
                    lock(&tree.restrictive).push(Restrictive {
                        path: self.path,
                        identity: self.created.identity,
                        mode,
                    });
                }
                drop(self.target);
                queue.complete(self.completion);
            }
            Err(error) => queue.fail(Failure::Os(error).into_error(&self.name)),
        }
    }
}

/// A source folder merged into the live folder of the same name, whose own
/// metadata is left alone.
pub(super) struct Merged {
    pub(super) source: (Arc<OwnedFd>, OsString),
    /// The live folder's parent and name, and its identity once seen.
    pub(super) target: (Arc<OwnedFd>, OsString, Option<LocalFileIdentity>),
    pub(super) path: PathBuf,
}

impl Merged {
    fn run(self, tree: &Arc<Tree>, queue: &Queue) {
        if let Err(failure) = self.merge(tree, queue) {
            queue.fail(failure.into_error(&self.source.1));
        }
    }

    fn merge(&self, tree: &Arc<Tree>, queue: &Queue) -> Result<(), Failure> {
        let copy = &tree.copy;
        let fs = copy.fs;
        let (source_parent, source_name) = &self.source;
        let source = fs
            .open_dir(Entry::new(&**source_parent, source_name))
            .map_err(|error| changed(source_name, error))?;
        let (target_parent, target_name, expected) = &self.target;
        let target = fs
            .open_dir(Entry::new(&**target_parent, target_name))
            .map_err(|error| changed(target_name, error))?;
        let target_stat = fs.stat_fd(target.as_fd())?;
        let mount = Mount::new(fs, target.as_fd());
        if expected.is_some_and(|expected| !same_entry(&target_stat, expected, mount.family)) {
            return Err(Failure::Changed);
        }
        let children = children(fs, source.as_fd())?;
        let (source, target) = (Arc::new(source), Arc::new(target));
        tree.probe_links_in(&mount, target.clone());
        let live = Live {
            source,
            dir: target,
            stat: target_stat,
            mount,
            link_scope: tree.link_scopes.fetch_add(1, Ordering::Relaxed),
        };
        let mut used = HashSet::new();
        for (name, kind) in children {
            copy.check_cancelled()?;
            let target_name = fat_family_child_name(&name, copy.fat_family, &mut used);
            live.merge(tree, queue, &name, kind, &target_name, &self.path)
                .map_err(|failure| failure.named(&name))?;
        }
        Ok(())
    }
}

/// A live folder a merge copies into, and its source folder.
struct Live {
    source: Arc<OwnedFd>,
    dir: Arc<OwnedFd>,
    stat: EntryStat,
    mount: Mount,
    link_scope: usize,
}

impl Live {
    /// Merges a folder into a folder of the same name, and makes anything
    /// else at a name that is free; nothing there is ever replaced.
    fn merge(
        &self,
        tree: &Arc<Tree>,
        queue: &Queue,
        name: &OsStr,
        kind: FileType,
        target_name: &OsStr,
        path: &Path,
    ) -> Result<(), Failure> {
        let copy = &tree.copy;
        let fs = copy.fs;
        let target = Entry::new(&*self.dir, target_name);
        let path = path.join(target_name);
        let existing = match fs.stat(target, false) {
            Ok(existing) => Some(existing),
            Err(Errno::NOENT) => None,
            Err(error) => return Err(error.into()),
        };
        match (existing, kind) {
            (Some(existing), FileType::Directory) if existing.is_directory() => {
                queue.enqueue(TreeJob::Merge(
                    tree.clone(),
                    Merged {
                        source: (self.source.clone(), name.to_os_string()),
                        target: (
                            self.dir.clone(),
                            target_name.to_os_string(),
                            Some(existing.identity),
                        ),
                        path,
                    },
                ));
            }
            (Some(_), _) => return Err(Failure::Place(PlaceError::Exists)),
            (None, FileType::Directory) => {
                let created = claim_folder(fs, target, false)?;
                queue.enqueue(TreeJob::Folder(
                    tree.clone(),
                    Folder {
                        source: (self.source.clone(), name.to_os_string(), None),
                        target: (self.dir.clone(), target_name.to_os_string(), created),
                        parent: self.stat,
                        path,
                        naming: Naming::Live,
                        mount: self.mount,
                        link_scope: self.link_scope,
                        completion: None,
                    },
                ));
            }
            (None, FileType::RegularFile) => queue.enqueue(TreeJob::File(
                tree.clone(),
                File {
                    source: self.source.clone(),
                    name: name.to_os_string(),
                    target: self.dir.clone(),
                    parent: self.stat,
                    claimed: None,
                    adopts: false,
                    names: None,
                    mount: self.mount,
                    path,
                    link_scope: self.link_scope,
                    completion: None,
                },
            )),
            (None, FileType::Symlink) => {
                copy_link(
                    copy,
                    Entry::new(&*self.source, name),
                    target,
                    &self.mount,
                    false,
                )?;
            }
            (None, FileType::Fifo) => {
                let source = Entry::new(&*self.source, name);
                if copy_fifo(copy, source, target, &self.stat, &self.mount, false)?.is_none() {
                    copy.skip(path, SpecialKind::Pipe)?;
                }
            }
            (None, kind) => copy.skip(path, special_kind(kind))?,
        }
        Ok(())
    }
}
