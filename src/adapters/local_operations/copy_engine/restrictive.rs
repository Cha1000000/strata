// SPDX-License-Identifier: MIT

//! Folders whose mode would lock their owner out of the copy. They are
//! recorded by path beneath the copy's root, so no descriptor is held for
//! each, and get their exact mode last.

use std::{
    cmp::Reverse,
    ffi::OsStr,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
};

use rustix::io::Errno;

use super::{best_effort, is_fatal, stable_inodes};
use crate::adapters::{
    local_operations::{
        LocalFileIdentity,
        fs_ops::{Entry, FsOps},
    },
    volume::Family,
};

#[derive(Clone, Debug)]
pub(in crate::adapters::local_operations) struct Restrictive {
    /// Beneath the root; empty for the root itself.
    pub(super) path: PathBuf,
    pub(super) identity: LocalFileIdentity,
    pub(super) mode: u32,
}

impl Restrictive {
    fn depth(&self) -> usize {
        self.path.components().count()
    }
}

/// The folder at `path` beneath `root`, unless something other than
/// `identity` is there now.
pub(super) fn reopen(
    fs: &dyn FsOps,
    root: BorrowedFd<'_>,
    path: &Path,
    identity: LocalFileIdentity,
    stable_inodes: bool,
) -> Result<Option<OwnedFd>, Errno> {
    let opened = fs
        .open_dir(Entry {
            dir: root,
            name: beneath(path),
        })
        .and_then(|fd| fs.stat_fd(fd.as_fd()).map(|stat| (fd, stat)));
    match opened {
        Ok((fd, current)) => {
            let same = if stable_inodes {
                current.identity == identity
            } else {
                current.is_directory()
            };
            Ok(same.then_some(fd))
        }
        Err(error) if is_fatal(error) => Err(error),
        Err(_) => Ok(None),
    }
}

/// `path` as a name `open_dir` resolves beneath a folder; empty is the folder.
pub(super) fn beneath(path: &Path) -> &OsStr {
    if path.as_os_str().is_empty() {
        OsStr::new(".")
    } else {
        path.as_os_str()
    }
}

/// Gives each folder its exact mode, deepest first, so its ancestors stay
/// open to the owner until then.
pub(super) fn tighten(
    fs: &dyn FsOps,
    root: BorrowedFd<'_>,
    folders: &mut [Restrictive],
    stable_inodes: bool,
) -> Result<(), Errno> {
    folders.sort_by_key(|folder| Reverse(folder.depth()));
    for folder in folders.iter() {
        if let Some(fd) = reopen(fs, root, &folder.path, folder.identity, stable_inodes)? {
            best_effort(fs.chmod_fd(fd.as_fd(), folder.mode))?;
        }
    }
    Ok(())
}

/// Gives the owner access back, shallowest first, so the copy can be removed.
pub(super) fn loosen(
    fs: &dyn FsOps,
    root: BorrowedFd<'_>,
    folders: &[Restrictive],
    family: Family,
) {
    let mut folders = folders.iter().collect::<Vec<_>>();
    folders.sort_by_key(|folder| folder.depth());
    for folder in folders {
        let at = Entry {
            dir: root,
            name: beneath(&folder.path),
        };
        // Without POSIX extensions, the SMB client clears the read-only
        // attribute it set only once it has seen it set.
        if family == Family::Smb {
            let _ = fs.stat(at, true);
        }
        let stable = stable_inodes(family);
        if let Ok(Some(fd)) = reopen(fs, root, &folder.path, folder.identity, stable) {
            let _ = fs.chmod_fd(fd.as_fd(), folder.mode | 0o700);
        }
    }
}
