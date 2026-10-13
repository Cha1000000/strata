// SPDX-License-Identifier: MIT

//! Puts what a copy wrote on its device before anything only the copy holds
//! is deleted, the way the destination's filesystem family reaches it.

use std::{
    ffi::OsStr,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use gtk::glib;
use rustix::io::Errno;

use super::{Mount, data::kernel};
use crate::adapters::{
    local_operations::{
        fs_ops::{Entry, FsOps, OffMain},
        io_error, open_local_parent_directory, os_error_detail,
    },
    volume::{Family, MountKey},
};

#[derive(Clone, Copy, Default)]
struct Plan {
    /// `fdatasync` on each written file before it is closed.
    each_file: bool,
    sync_fs: bool,
    /// `fsync` on the barrier's folder, after any `syncfs`.
    sync_dir: bool,
}

impl Plan {
    fn of(family: Family) -> Self {
        let mut plan = match family {
            // Their `sync_fs` writes back without flushing the device's cache,
            // which their `fsync`, of folders too, does.
            Family::Fat => Self {
                sync_fs: true,
                sync_dir: true,
                ..Self::default()
            },
            // Without FUSE_SYNCFS, `syncfs` stops in the kernel. A file's
            // `fsync` reaches the daemon, and ntfs-3g and exfat-fuse sync and
            // flush their whole device for a folder's, which covers the names
            // published after the files were synced.
            Family::Fuse => Self {
                each_file: true,
                sync_dir: true,
                ..Self::default()
            },
            // Only a file's `fsync` reaches the server, as an SMB FLUSH.
            Family::Smb => Self {
                each_file: true,
                ..Self::default()
            },
            Family::Other => Self {
                each_file: true,
                sync_fs: true,
                ..Self::default()
            },
            _ => Self {
                sync_fs: true,
                ..Self::default()
            },
        };
        // Before Linux 5.8, `syncfs` reports no writeback errors.
        if kernel() < (5, 8, 0) {
            plan.each_file = true;
        }
        plan
    }
}

/// Whether a durable copy onto `family` syncs each file before closing it.
pub(super) fn each_file(family: Family) -> bool {
    Plan::of(family).each_file
}

static OPENED: AtomicU64 = AtomicU64::new(0);

/// A destination folder opened before a copy writes beneath it.
pub(in crate::adapters::local_operations) struct Barrier {
    dir: OffMain<OwnedFd>,
    pub path: PathBuf,
    pub key: MountKey,
    plan: Plan,
    opened: u64,
}

impl Barrier {
    /// `syncfs` reports only the writeback errors that follow the open of
    /// its descriptor, so this must come before the copy writes. A folder
    /// others may only write into cannot be opened for reading, but any
    /// folder above it on the same mount serves.
    pub(super) fn open(fs: &dyn FsOps, path: &Path) -> Result<Self, glib::Error> {
        let could_not_open = |error: Errno| {
            io_error(rust_i18n::t!(
                "Could not safely open %{path}: %{error}",
                path = path.display(),
                error = os_error_detail(error)
            ))
        };
        let mut key = None;
        for folder in path.ancestors() {
            let pinned = open_local_parent_directory(folder).map_err(io_error)?;
            let here = fs.mount_key(pinned.as_fd());
            match key {
                None => {
                    key = Some(here);
                    // Only flushing a folder or its filesystem reads it.
                    let plan = Plan::of(Mount::new(fs, pinned.as_fd()).family);
                    if !plan.sync_fs && !plan.sync_dir {
                        return Ok(Self::opened(fs, pinned, path.to_path_buf()));
                    }
                }
                Some(key) if key != here => break,
                Some(_) => {}
            }
            let opened = fs.open_dir(Entry {
                dir: pinned.as_fd(),
                name: OsStr::new("."),
            });
            match opened {
                Ok(dir) => return Ok(Self::opened(fs, dir, path.to_path_buf())),
                Err(Errno::ACCESS) => {}
                Err(error) => return Err(could_not_open(error)),
            }
        }
        Err(could_not_open(Errno::ACCESS))
    }

    /// A barrier at the folder `dir`, at `path`, which a copy opened.
    pub(super) fn at(fs: &dyn FsOps, dir: BorrowedFd<'_>, path: PathBuf) -> Result<Self, Errno> {
        let dir = fs.open_dir(Entry {
            dir,
            name: OsStr::new("."),
        })?;
        Ok(Self::opened(fs, dir, path))
    }

    fn opened(fs: &dyn FsOps, dir: OwnedFd, path: PathBuf) -> Self {
        let mount = Mount::new(fs, dir.as_fd());
        Self {
            dir: OffMain::new(dir),
            path,
            key: mount.key,
            plan: Plan::of(mount.family),
            opened: OPENED.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Whether files written beneath it are synced one by one, since
    /// reaching it does not reach them.
    pub(in crate::adapters::local_operations) fn syncs_each_file(&self) -> bool {
        self.plan.each_file
    }

    /// Whether this was opened first, so it reports every error `other` would.
    pub(in crate::adapters::local_operations) fn precedes(&self, other: &Self) -> bool {
        self.opened < other.opened
    }

    /// Flushes the filesystem to its device; a `removable` one gets a
    /// folder `fsync` besides.
    pub(super) fn reach(&self, fs: &dyn FsOps, removable: bool) -> Result<(), Errno> {
        if self.plan.sync_fs {
            fs.sync_fs(self.dir.as_fd())?;
        }
        if self.plan.sync_dir || removable {
            fs.sync_dir(self.dir.as_fd())?;
        }
        Ok(())
    }
}
