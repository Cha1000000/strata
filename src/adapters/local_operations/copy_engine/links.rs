// SPDX-License-Identifier: MIT

//! Hard links within one copied item, or one merge stage. Only the first
//! name of a source file with several names is copied. Once every file of
//! its scope is written, each later name is linked to that copy, or copied
//! from it where the destination refuses.

use std::{
    collections::{HashMap, HashSet, hash_map},
    ffi::OsString,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use rustix::{fs::AtFlags, io::Errno};

use super::{
    Copy, Created, Failure, FileCopy, Mount, is_fatal, lock,
    manifest::Pending,
    metadata::Snapshot,
    restrictive, stable_inodes,
    tree::{self, Names},
};
use crate::{
    adapters::{
        local_operations::{
            LocalFileIdentity,
            fs_ops::{Entry, FsOps},
            placement::{self, PlaceError},
        },
        volume::{Family, MountKey},
    },
    services::{KeptCause, RenameCause},
};

/// A source file by device and inode, within one scope.
pub(super) type Key = (usize, u64, u64);

/// How a name of a source file with several names is made.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Role {
    /// Copied on its own, as the destination keeps no links.
    Copied,
    /// Copied, and the later names are linked to the copy.
    First,
    /// Made by the links pass.
    Later,
}

/// A name of a source file with several names.
pub(super) struct Name {
    pub(super) key: Key,
    /// Beneath the walk's root.
    pub(super) path: PathBuf,
    /// The source name, for errors.
    pub(super) source: OsString,
    /// What the folder job claimed at `path`.
    pub(super) claimed: Option<LocalFileIdentity>,
    /// The names of its folder.
    pub(super) names: Arc<Names>,
    pub(super) size: u64,
    /// Recorded once made, by a move.
    pub(super) recorded: Option<Pending>,
}

#[derive(Default)]
struct First {
    /// Where the copy is, once it is written.
    written: Option<(PathBuf, LocalFileIdentity)>,
    /// It kept changing while it was read, so a move leaves every name.
    torn: bool,
}

/// A made folder that gains a later name after it got its times.
struct Retimed {
    scope: usize,
    path: PathBuf,
    identity: LocalFileIdentity,
    snapshot: Snapshot,
}

#[derive(Default)]
pub(super) struct Links {
    state: Mutex<State>,
    probes: Mutex<HashMap<MountKey, Probe>>,
}

/// Whether a mount keeps links, where it has not learned that.
enum Probe {
    /// To be probed in this folder, outside the copy.
    In(Arc<OwnedFd>),
    /// Probed by this copy, without learning anything.
    Found(bool),
}

#[derive(Default)]
struct State {
    firsts: HashMap<Key, First>,
    later: Vec<Name>,
    /// The folders later names land in, by scope.
    folders: HashSet<(usize, PathBuf)>,
    retimed: Vec<Retimed>,
}

impl Links {
    /// Probes `mount`, if it must be, in `dir`: a folder of the destination
    /// that the copy does not make, so nothing a probe leaves ends up in it.
    pub(super) fn probe_in(&self, mount: MountKey, dir: Arc<OwnedFd>) {
        lock(&self.probes).entry(mount).or_insert(Probe::In(dir));
    }

    /// A probe rules out links that report another inode, as on sshfs.
    fn kept(&self, fs: &dyn FsOps, mount: &Mount) -> bool {
        if let Some(kept) = mount.capabilities().hard_links {
            return kept;
        }
        let mut probes = lock(&self.probes);
        let kept = match probes.get(&mount.key) {
            Some(Probe::Found(kept)) => return *kept,
            Some(Probe::In(dir)) => placement::links_hold(fs, dir.as_fd()),
            None => false,
        };
        probes.insert(mount.key, Probe::Found(kept));
        kept
    }

    /// Decides how `name` is made, and records it.
    pub(super) fn role(&self, fs: &dyn FsOps, mount: &Mount, name: Name) -> Role {
        if !self.kept(fs, mount) {
            return Role::Copied;
        }
        let mut state = lock(&self.state);
        match state.firsts.entry(name.key) {
            hash_map::Entry::Vacant(vacant) => {
                vacant.insert(First::default());
                Role::First
            }
            hash_map::Entry::Occupied(_) => {
                if let Some(folder) = name.path.parent() {
                    state.folders.insert((name.key.0, folder.to_path_buf()));
                }
                state.later.push(name);
                Role::Later
            }
        }
    }

    /// The first name of `key` is written at `path`.
    pub(super) fn written(&self, key: Key, path: PathBuf, identity: LocalFileIdentity) {
        if let Some(first) = lock(&self.state).firsts.get_mut(&key) {
            first.written = Some((path, identity));
        }
    }

    /// The first name of `key` kept changing while it was read.
    pub(super) fn torn(&self, key: Key) {
        if let Some(first) = lock(&self.state).firsts.get_mut(&key) {
            first.torn = true;
        }
    }

    /// Keeps the times of the made folder at `path` in `scope` for after
    /// the links pass, when a later name lands in it.
    pub(super) fn retime(
        &self,
        scope: usize,
        path: &Path,
        identity: LocalFileIdentity,
        snapshot: Snapshot,
    ) {
        let mut state = lock(&self.state);
        if state.folders.contains(&(scope, path.to_path_buf())) {
            state.retimed.push(Retimed {
                scope,
                path: path.to_path_buf(),
                identity,
                snapshot,
            });
        }
    }

    pub(super) fn take(&self) -> Self {
        Self {
            state: Mutex::new(std::mem::take(&mut *lock(&self.state))),
            probes: Mutex::default(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        lock(&self.state).later.is_empty()
    }

    /// Makes every later name of `scope` beneath `root`, the folder at
    /// `base` beneath the walk's root, in path order, then gives the folders
    /// they land in their times back. Runs while every folder is still open
    /// to its owner.
    pub(super) fn make(
        &self,
        scope: usize,
        copy: &Copy,
        root: BorrowedFd<'_>,
        base: &Path,
        family: Family,
        created: Option<&Created>,
    ) -> Result<(), Failure> {
        let (mut later, firsts, retimed) = {
            let mut state = lock(&self.state);
            let (later, others) = std::mem::take(&mut state.later)
                .into_iter()
                .partition::<Vec<_>, _>(|name| name.key.0 == scope);
            state.later = others;
            let (retimed, others) = std::mem::take(&mut state.retimed)
                .into_iter()
                .partition::<Vec<_>, _>(|folder| folder.scope == scope);
            state.retimed = others;
            let firsts = later
                .iter()
                .filter_map(|name| {
                    let first = state.firsts.get(&name.key)?;
                    Some((name.key, (first.written.clone(), first.torn)))
                })
                .collect::<HashMap<_, _>>();
            (later, firsts, retimed)
        };
        later.sort_by(|left, right| left.path.cmp(&right.path));
        let mut linking = true;
        for mut name in later {
            copy.check_cancelled()?;
            let made = match firsts.get(&name.key) {
                Some((Some((path, identity)), _)) => {
                    make(copy, (root, base), (path, *identity), &name, &mut linking)
                }
                Some((None, true)) => {
                    leave(copy, (root, base), &name)
                        .map_err(|failure| failure.named(&name.source))?;
                    if let Some(recorded) = name.recorded.take() {
                        recorded.keep(KeptCause::Changed);
                    }
                    continue;
                }
                _ => Err(Failure::Changed),
            };
            let (path, made) = made.map_err(|failure| failure.named(&name.source))?;
            if let Some(created) = created {
                created.add(path, made);
            }
            if let Some(recorded) = name.recorded.take() {
                recorded.made();
            }
            copy.add_file(name.size);
        }
        let stable = stable_inodes(family);
        for folder in retimed {
            let path = beneath(base, &folder.path)?;
            if let Some(fd) = restrictive::reopen(copy.fs, root, path, folder.identity, stable)? {
                folder.snapshot.apply_times(copy.fs, fd.as_fd())?;
            }
        }
        Ok(())
    }
}

/// `path`, beneath the walk's root, as a path beneath `base`.
fn beneath<'a>(base: &Path, path: &'a Path) -> Result<&'a Path, Failure> {
    path.strip_prefix(base).map_err(|_| Failure::Changed)
}

/// Links `name` to the copy at `first`, or copies that copy on the same
/// device where the destination refuses; one refusal ends `linking`.
/// Returns where `name` was made and what it holds.
fn make(
    copy: &Copy,
    (root, base): (BorrowedFd<'_>, &Path),
    (first, identity): (&Path, LocalFileIdentity),
    name: &Name,
    linking: &mut bool,
) -> Result<(PathBuf, LocalFileIdentity), Failure> {
    let fs = copy.fs;
    let (Some(first_name), Some(file_name)) = (first.file_name(), name.path.file_name()) else {
        return Err(Failure::Changed);
    };
    let first_dir = parent(fs, root, beneath(base, first)?)?;
    let dir = parent(fs, root, beneath(base, &name.path)?)?;
    let from = Entry::new(&first_dir, first_name);
    if let Some(claimed) = name.claimed
        && !placement::remove_if_unchanged(
            fs,
            Entry::new(&dir, file_name),
            claimed,
            AtFlags::empty(),
        )
    {
        return Err(Failure::Place(PlaceError::Changed));
    }
    tree::make_numbered(
        copy,
        &name.names,
        &name.path,
        false,
        RenameCause::Alias,
        |file_name| {
            let to = Entry::new(&dir, file_name);
            if *linking {
                match fs.link(from, to) {
                    Ok(()) => {
                        linked(fs, from, to, identity)?;
                        name.names.made(file_name, identity);
                        return Ok(identity);
                    }
                    // Only this copy's own link, from a lost NFS reply, reports
                    // the copy's identity.
                    Err(error) if tree::taken(fs, to, error) => {
                        return tree::adopt(fs, to, Some(&name.names), |stat| {
                            stat.identity == identity
                        });
                    }
                    Err(error)
                        if is_fatal(error) || matches!(error, Errno::NOSPC | Errno::DQUOT) =>
                    {
                        return Err(error.into());
                    }
                    Err(_) => *linking = false,
                }
            }
            let parent = fs.stat_fd(dir.as_fd())?;
            let made = FileCopy::new(copy, dir.as_fd())
                .create_from(from, identity, None, file_name, &parent)?;
            name.names.made(file_name, made);
            Ok(made)
        },
    )
}

/// Removes what the folder job claimed for `name`, whose first name kept
/// changing while it was read, so no name of it is copied.
fn leave(copy: &Copy, (root, base): (BorrowedFd<'_>, &Path), name: &Name) -> Result<(), Failure> {
    let (Some(claimed), Some(file_name)) = (name.claimed, name.path.file_name()) else {
        return Ok(());
    };
    let dir = parent(copy.fs, root, beneath(base, &name.path)?)?;
    if placement::remove_if_unchanged(
        copy.fs,
        Entry::new(&dir, file_name),
        claimed,
        AtFlags::empty(),
    ) {
        Ok(())
    } else {
        Err(Failure::Place(PlaceError::Changed))
    }
}

/// The folder holding `path` beneath `root`.
fn parent(fs: &dyn FsOps, root: BorrowedFd<'_>, path: &Path) -> Result<OwnedFd, Errno> {
    let parent = path.parent().unwrap_or(Path::new(""));
    fs.open_dir(Entry {
        dir: root,
        name: restrictive::beneath(parent),
    })
}

/// A new link that reports another item linked whatever replaced the copy
/// at `from`, so that second name of it is removed again. Anything else at
/// `to` was put there after the link, and is left alone.
fn linked(
    fs: &dyn FsOps,
    from: Entry<'_>,
    to: Entry<'_>,
    identity: LocalFileIdentity,
) -> Result<(), Failure> {
    let made = fs.stat(to, false)?;
    if made.identity == identity {
        return Ok(());
    }
    if fs
        .stat(from, true)
        .is_ok_and(|current| current.identity == made.identity)
    {
        placement::remove_if_unchanged(fs, to, made.identity, AtFlags::empty());
    }
    Err(Failure::Changed)
}
