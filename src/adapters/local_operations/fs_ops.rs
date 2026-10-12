// SPDX-License-Identifier: MIT

//! The directory-relative syscalls that placement uses, behind one seam so
//! tests can emulate filesystems that lack a primitive.

use std::{
    ffi::{OsStr, OsString},
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::ffi::OsStringExt,
    },
    path::Path,
};

use gtk::{gio, glib, prelude::*};

use rustix::{
    fs::{AtFlags, FileType, FsWord, Mode, OFlags, RenameFlags, StatxFlags},
    io::Errno,
};

use super::LocalFileIdentity;
use crate::adapters::volume::MountKey;

/// `STATX_MNT_ID_UNIQUE` (Linux 6.8), which rustix does not name yet.
const STATX_MNT_ID_UNIQUE: u32 = 0x4000;

#[derive(Clone, Copy)]
pub(super) struct Entry<'a> {
    pub dir: BorrowedFd<'a>,
    pub name: &'a OsStr,
}

impl<'a> Entry<'a> {
    pub(super) fn new(dir: &'a impl AsFd, name: &'a OsStr) -> Self {
        Self {
            dir: dir.as_fd(),
            name,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct EntryStat {
    pub identity: LocalFileIdentity,
    pub kind: FileType,
    pub nlink: u64,
    pub size: u64,
}

impl EntryStat {
    pub(super) fn is_directory(&self) -> bool {
        self.kind == FileType::Directory
    }
}

pub(super) trait FsOps: Send + Sync {
    fn rename(&self, from: Entry<'_>, to: Entry<'_>, flags: RenameFlags) -> Result<(), Errno>;
    fn link(&self, from: Entry<'_>, to: Entry<'_>) -> Result<(), Errno>;
    fn unlink(&self, at: Entry<'_>, flags: AtFlags) -> Result<(), Errno>;
    fn mkdir(&self, at: Entry<'_>, mode: Mode) -> Result<(), Errno>;
    fn symlink(&self, target: &OsStr, at: Entry<'_>) -> Result<(), Errno>;
    fn readlink(&self, at: Entry<'_>) -> Result<OsString, Errno>;
    /// Never follows `at`; `force_sync` asks a network filesystem to revalidate.
    fn stat(&self, at: Entry<'_>, force_sync: bool) -> Result<EntryStat, Errno>;
    fn stat_dir(&self, dir: BorrowedFd<'_>) -> Result<EntryStat, Errno>;
    fn open_parent(&self, dir: BorrowedFd<'_>) -> Result<OwnedFd, Errno>;
    fn create_exclusive(&self, at: Entry<'_>, mode: Mode) -> Result<OwnedFd, Errno>;
    fn names(&self, dir: Entry<'_>) -> Result<Vec<OsString>, Errno>;
    fn mount_key(&self, dir: BorrowedFd<'_>) -> MountKey;
    fn fs_type(&self, dir: BorrowedFd<'_>) -> Option<FsWord>;
    /// GIO's `access::can-trash` for a native item, without following it.
    fn can_trash(&self, path: &Path) -> bool;
    fn trash(&self, path: &Path, cancellable: &gio::Cancellable) -> Result<(), glib::Error>;
}

pub(super) struct System;

pub(super) fn fs() -> &'static dyn FsOps {
    #[cfg(test)]
    {
        &super::tests::fs_quirks::SCOPED
    }
    #[cfg(not(test))]
    {
        &System
    }
}

pub(super) fn assert_off_main_thread() {
    debug_assert!(
        !glib::MainContext::default().is_owner(),
        "filesystem placement must not run on the main thread"
    );
}

fn entry_stat(stat: &rustix::fs::Statx) -> EntryStat {
    EntryStat {
        identity: LocalFileIdentity {
            device: rustix::fs::makedev(stat.stx_dev_major, stat.stx_dev_minor),
            inode: stat.stx_ino,
        },
        kind: FileType::from_raw_mode(u32::from(stat.stx_mode)),
        nlink: u64::from(stat.stx_nlink),
        size: stat.stx_size,
    }
}

fn legacy_stat(stat: &rustix::fs::Stat) -> EntryStat {
    EntryStat {
        identity: LocalFileIdentity::from_stat(stat),
        kind: FileType::from_raw_mode(stat.st_mode),
        nlink: stat.st_nlink,
        size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
    }
}

fn statx_at(dir: BorrowedFd<'_>, name: &OsStr, flags: AtFlags) -> Result<EntryStat, Errno> {
    let mask = StatxFlags::TYPE | StatxFlags::INO | StatxFlags::NLINK | StatxFlags::SIZE;
    match rustix::fs::statx(dir, name, flags | AtFlags::SYMLINK_NOFOLLOW, mask) {
        Ok(stat) => Ok(entry_stat(&stat)),
        Err(Errno::NOSYS) => {
            let at_flags = (flags & AtFlags::EMPTY_PATH) | AtFlags::SYMLINK_NOFOLLOW;
            rustix::fs::statat(dir, name, at_flags).map(|stat| legacy_stat(&stat))
        }
        Err(error) => Err(error),
    }
}

impl FsOps for System {
    fn rename(&self, from: Entry<'_>, to: Entry<'_>, flags: RenameFlags) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::renameat_with(from.dir, from.name, to.dir, to.name, flags)
    }

    fn link(&self, from: Entry<'_>, to: Entry<'_>) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::linkat(from.dir, from.name, to.dir, to.name, AtFlags::empty())
    }

    fn unlink(&self, at: Entry<'_>, flags: AtFlags) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::unlinkat(at.dir, at.name, flags)
    }

    fn mkdir(&self, at: Entry<'_>, mode: Mode) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::mkdirat(at.dir, at.name, mode)
    }

    fn symlink(&self, target: &OsStr, at: Entry<'_>) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::symlinkat(target, at.dir, at.name)
    }

    fn readlink(&self, at: Entry<'_>) -> Result<OsString, Errno> {
        assert_off_main_thread();
        rustix::fs::readlinkat(at.dir, at.name, Vec::new())
            .map(|link| OsString::from_vec(link.into_bytes()))
    }

    fn stat(&self, at: Entry<'_>, force_sync: bool) -> Result<EntryStat, Errno> {
        assert_off_main_thread();
        let flags = if force_sync {
            AtFlags::STATX_FORCE_SYNC
        } else {
            AtFlags::empty()
        };
        statx_at(at.dir, at.name, flags)
    }

    fn stat_dir(&self, dir: BorrowedFd<'_>) -> Result<EntryStat, Errno> {
        assert_off_main_thread();
        statx_at(dir, OsStr::new(""), AtFlags::EMPTY_PATH)
    }

    fn open_parent(&self, dir: BorrowedFd<'_>) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        rustix::fs::openat(
            dir,
            "..",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
    }

    fn create_exclusive(&self, at: Entry<'_>, mode: Mode) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        rustix::fs::openat(
            at.dir,
            at.name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            mode,
        )
    }

    fn names(&self, dir: Entry<'_>) -> Result<Vec<OsString>, Errno> {
        assert_off_main_thread();
        let handle = rustix::fs::openat(
            dir.dir,
            dir.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&handle)? {
            let entry = entry?;
            let name = entry.file_name();
            if name != c"." && name != c".." {
                names.push(OsString::from_vec(name.to_bytes().to_vec()));
            }
        }
        Ok(names)
    }

    fn mount_key(&self, dir: BorrowedFd<'_>) -> MountKey {
        assert_off_main_thread();
        let mask = StatxFlags::from_bits_retain(STATX_MNT_ID_UNIQUE) | StatxFlags::MNT_ID;
        match rustix::fs::statx(
            dir,
            "",
            AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC,
            mask,
        ) {
            Ok(stat) => {
                let device = rustix::fs::makedev(stat.stx_dev_major, stat.stx_dev_minor);
                if stat.stx_mask & STATX_MNT_ID_UNIQUE != 0 {
                    MountKey::Unique(stat.stx_mnt_id)
                } else if StatxFlags::from_bits_retain(stat.stx_mask).contains(StatxFlags::MNT_ID) {
                    MountKey::Mount {
                        id: stat.stx_mnt_id,
                        device,
                    }
                } else {
                    MountKey::Device(device)
                }
            }
            Err(_) => rustix::fs::fstat(dir)
                .map_or(MountKey::Device(u64::MAX), |stat| MountKey::Device(stat.st_dev)),
        }
    }

    fn fs_type(&self, dir: BorrowedFd<'_>) -> Option<FsWord> {
        assert_off_main_thread();
        rustix::fs::fstatfs(dir).ok().map(|stat| stat.f_type)
    }

    fn can_trash(&self, path: &Path) -> bool {
        assert_off_main_thread();
        gio::File::for_path(path)
            .query_info(
                gio::FILE_ATTRIBUTE_ACCESS_CAN_TRASH,
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                gio::Cancellable::NONE,
            )
            .is_ok_and(|info| info.boolean(gio::FILE_ATTRIBUTE_ACCESS_CAN_TRASH))
    }

    fn trash(&self, path: &Path, cancellable: &gio::Cancellable) -> Result<(), glib::Error> {
        assert_off_main_thread();
        gio::File::for_path(path).trash(Some(cancellable))
    }
}
