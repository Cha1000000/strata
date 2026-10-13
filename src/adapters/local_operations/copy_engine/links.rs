// SPDX-License-Identifier: MIT

//! Hard links within one copied item. Only the first name of a source file
//! with several names is copied. Once every file is written, each later name
//! is linked to that copy, or copied from it where the destination refuses.

use std::{
    collections::{HashMap, HashSet, hash_map},
    ffi::OsString,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use rustix::{fs::AtFlags, io::Errno};

use super::{
    Copy, Failure, FileCopy, Mount, is_fatal, lock, metadata::Snapshot, restrictive, stable_inodes,
    tree,
};
use crate::adapters::{
    local_operations::{
        LocalFileIdentity,
        fs_ops::{Entry, FsOps},
        placement::{self, PlaceError},
    },
    volume::{Family, MountKey},
};

/// A source file by device and inode, within one link scope.
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
    pub(super) size: u64,
}

struct First {
    path: PathBuf,
    /// Once the copy is written.
    identity: Option<LocalFileIdentity>,
}

/// A made folder that gains a later name after it got its times.
struct Retimed {
    path: PathBuf,
    identity: LocalFileIdentity,
    snapshot: Snapshot,
}

#[derive(Default)]
pub(super) struct Links(Mutex<State>);

#[derive(Default)]
struct State {
    /// Mounts probed by this copy, where nothing was learned.
    probed: HashMap<MountKey, bool>,
    firsts: HashMap<Key, First>,
    later: Vec<Name>,
    /// The folders later names land in.
    folders: HashSet<PathBuf>,
    retimed: Vec<Retimed>,
}

impl Links {
    /// Decides how `name`, to be made in `dir`, is made, and records it.
    pub(super) fn role(
        &self,
        fs: &dyn FsOps,
        mount: &Mount,
        dir: BorrowedFd<'_>,
        name: Name,
    ) -> Role {
        let mut state = lock(&self.0);
        // A probe rules out links that report another inode, as on sshfs.
        let kept = mount.capabilities().hard_links.unwrap_or_else(|| {
            *state
                .probed
                .entry(mount.key)
                .or_insert_with(|| placement::links_hold(fs, dir))
        });
        if !kept {
            return Role::Copied;
        }
        match state.firsts.entry(name.key) {
            hash_map::Entry::Vacant(vacant) => {
                vacant.insert(First {
                    path: name.path,
                    identity: None,
                });
                Role::First
            }
            hash_map::Entry::Occupied(_) => {
                if let Some(folder) = name.path.parent() {
                    state.folders.insert(folder.to_path_buf());
                }
                state.later.push(name);
                Role::Later
            }
        }
    }

    pub(super) fn written(&self, key: Key, identity: LocalFileIdentity) {
        if let Some(first) = lock(&self.0).firsts.get_mut(&key) {
            first.identity = Some(identity);
        }
    }

    /// Keeps the times of the made folder at `path` for after the links
    /// pass, when a later name lands in it.
    pub(super) fn retime(&self, path: &Path, identity: LocalFileIdentity, snapshot: Snapshot) {
        let mut state = lock(&self.0);
        if state.folders.contains(path) {
            state.retimed.push(Retimed {
                path: path.to_path_buf(),
                identity,
                snapshot,
            });
        }
    }

    pub(super) fn take(&self) -> Self {
        Self(Mutex::new(std::mem::take(&mut *lock(&self.0))))
    }

    pub(super) fn is_empty(&self) -> bool {
        lock(&self.0).later.is_empty()
    }

    /// Makes every later name beneath `root`, in path order, then gives the
    /// folders they land in their times back. Runs while every folder is
    /// still open to its owner.
    pub(super) fn make(
        self,
        copy: &Copy,
        root: BorrowedFd<'_>,
        family: Family,
    ) -> Result<(), Failure> {
        let State {
            firsts,
            mut later,
            retimed,
            ..
        } = self.0.into_inner().unwrap_or_else(PoisonError::into_inner);
        later.sort_by(|left, right| left.path.cmp(&right.path));
        let mut linking = true;
        for name in &later {
            copy.check_cancelled()?;
            let first = firsts
                .get(&name.key)
                .and_then(|first| Some((first.path.as_path(), first.identity?)));
            let made = match first {
                Some(first) => make(copy, root, first, name, &mut linking),
                None => Err(Failure::Changed),
            };
            made.map_err(|failure| failure.named(&name.source))?;
            if copy.counted {
                copy.progress.add_bytes(name.size);
                copy.progress.add_file();
            }
        }
        let stable = stable_inodes(family);
        for folder in retimed {
            if let Some(fd) =
                restrictive::reopen(copy.fs, root, &folder.path, folder.identity, stable)?
            {
                folder.snapshot.apply_times(copy.fs, fd.as_fd())?;
            }
        }
        Ok(())
    }
}

/// Links `name` to the copy at `first`, or copies that copy on the same
/// device where the destination refuses; one refusal ends `linking`.
fn make(
    copy: &Copy,
    root: BorrowedFd<'_>,
    (first, identity): (&Path, LocalFileIdentity),
    name: &Name,
    linking: &mut bool,
) -> Result<(), Failure> {
    let fs = copy.fs;
    let (Some(first_name), Some(file_name)) = (first.file_name(), name.path.file_name()) else {
        return Err(Failure::Changed);
    };
    let first_dir = parent(fs, root, first)?;
    let dir = parent(fs, root, &name.path)?;
    let from = Entry::new(&first_dir, first_name);
    let to = Entry::new(&dir, file_name);
    if let Some(claimed) = name.claimed
        && !placement::remove_if_unchanged(fs, to, claimed, AtFlags::empty())
    {
        return Err(Failure::Place(PlaceError::Changed));
    }
    if *linking {
        match fs.link(from, to) {
            Ok(()) => return linked(fs, from, to, identity),
            // Only this copy's own link, from a lost NFS reply, reports the
            // copy's identity.
            Err(error) if tree::taken(fs, to, error) => {
                return tree::adopt(fs, to, Some(&HashSet::new()), |stat| {
                    stat.identity == identity
                })
                .map(drop);
            }
            Err(error) if is_fatal(error) || matches!(error, Errno::NOSPC | Errno::DQUOT) => {
                return Err(error.into());
            }
            Err(_) => *linking = false,
        }
    }
    let parent = fs.stat_fd(dir.as_fd())?;
    FileCopy::new(copy, dir.as_fd())
        .create_from(from, identity, None, file_name, &parent)
        .map(drop)
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
