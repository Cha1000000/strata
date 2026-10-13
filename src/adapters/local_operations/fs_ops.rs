// SPDX-License-Identifier: MIT

//! The directory-relative syscalls that placement uses, behind one seam so
//! tests can emulate filesystems that lack a primitive.

use std::{
    ffi::{OsStr, OsString},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd},
        unix::ffi::OsStringExt,
    },
    path::Path,
};

use gtk::{gio, glib, prelude::*};

use rustix::{
    fs::{AtFlags, FileType, FsWord, Mode, OFlags, RenameFlags, ResolveFlags, StatxFlags},
    io::Errno,
};

use super::{LocalFileIdentity, retry_local_open};
use crate::adapters::volume::{MountKey, MountTable};

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
    /// Permission bits only.
    pub mode: u32,
    pub nlink: u64,
    pub size: u64,
    pub atime: (i64, u32),
    pub mtime: (i64, u32),
    pub uid: u32,
    pub gid: u32,
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
    /// The open file or folder itself, from cached attributes.
    fn stat_fd(&self, fd: BorrowedFd<'_>) -> Result<EntryStat, Errno>;
    fn open_parent(&self, dir: BorrowedFd<'_>) -> Result<OwnedFd, Errno>;
    fn create_exclusive(&self, at: Entry<'_>, mode: Mode) -> Result<OwnedFd, Errno>;
    fn names(&self, dir: Entry<'_>) -> Result<Vec<OsString>, Errno>;
    fn mount_key(&self, dir: BorrowedFd<'_>) -> MountKey;
    fn fs_type(&self, dir: BorrowedFd<'_>) -> Option<FsWord>;
    /// GIO's `access::can-trash` for a native item, without following it.
    fn can_trash(&self, path: &Path) -> bool;
    /// GLib's own result: `Ok(false)` is a failure without an error (GLib #4060).
    fn trash(&self, path: &Path, cancellable: &gio::Cancellable) -> Result<bool, glib::Error>;
    /// The regular file at `at`, opened for reading without following it,
    /// or `None` when something else is there. `listed` skips looking at it
    /// by name first, where the caller just saw a regular file there.
    fn open_source(
        &self,
        at: Entry<'_>,
        listed: bool,
    ) -> Result<Option<(OwnedFd, EntryStat)>, Errno>;
    /// An unnamed file in `dir`, readable so a failed link can copy it out.
    fn open_tmpfile(&self, dir: BorrowedFd<'_>, mode: Mode) -> Result<OwnedFd, Errno>;
    /// Gives an unnamed file a name, never replacing one; `through_proc`
    /// links `/proc/self/fd/N` where `AT_EMPTY_PATH` needs privileges.
    fn link_fd(&self, fd: BorrowedFd<'_>, to: Entry<'_>, through_proc: bool) -> Result<(), Errno>;
    fn reflink(&self, target: BorrowedFd<'_>, source: BorrowedFd<'_>) -> Result<(), Errno>;
    fn copy_range(
        &self,
        source: BorrowedFd<'_>,
        source_offset: u64,
        target: BorrowedFd<'_>,
        target_offset: u64,
        len: usize,
    ) -> Result<usize, Errno>;
    fn read_at(&self, fd: BorrowedFd<'_>, buffer: &mut [u8], offset: u64) -> Result<usize, Errno>;
    fn write_at(&self, fd: BorrowedFd<'_>, buffer: &[u8], offset: u64) -> Result<usize, Errno>;
    /// Reports what `close` returns, where NFS, SMB and FUSE fail late writes.
    fn close(&self, fd: OwnedFd) -> Result<(), Errno>;
    fn chown_fd(&self, fd: BorrowedFd<'_>, uid: Option<u32>, gid: Option<u32>)
    -> Result<(), Errno>;
    fn chmod_fd(&self, fd: BorrowedFd<'_>, mode: u32) -> Result<(), Errno>;
    fn set_times_fd(
        &self,
        fd: BorrowedFd<'_>,
        atime: (i64, u32),
        mtime: (i64, u32),
    ) -> Result<(), Errno>;
    fn xattr_names(&self, fd: BorrowedFd<'_>) -> Result<Vec<OsString>, Errno>;
    fn get_xattr(&self, fd: BorrowedFd<'_>, name: &OsStr) -> Result<Vec<u8>, Errno>;
    fn set_xattr(&self, fd: BorrowedFd<'_>, name: &OsStr, value: &[u8]) -> Result<(), Errno>;
    /// The mountinfo filesystem type of the mount holding `fd`.
    fn mount_type(&self, fd: BorrowedFd<'_>) -> Option<String>;
    /// The folder at `at`, which may name several components, opened
    /// without following a symlink anywhere on the way.
    fn open_dir(&self, at: Entry<'_>) -> Result<OwnedFd, Errno>;
    /// The folder's entries with their types; `Unknown` where the
    /// filesystem does not say.
    fn list(&self, dir: BorrowedFd<'_>) -> Result<Vec<(OsString, FileType)>, Errno>;
    /// Makes a regular file without opening it, or a named pipe.
    fn create_node(&self, at: Entry<'_>, kind: FileType, mode: Mode) -> Result<(), Errno>;
    /// Opens the named pipe at `at` for its metadata, which never waits for
    /// a writer.
    fn open_fifo(&self, at: Entry<'_>) -> Result<OwnedFd, Errno>;
    /// Creates a file exclusively without opening it for writing, so its
    /// close flushes nothing (vfat mounted with `flush` waits there).
    fn create_readonly(&self, at: Entry<'_>, mode: Mode) -> Result<OwnedFd, Errno>;
    /// Opens a file this copy claimed, for writing.
    fn open_write(&self, at: Entry<'_>) -> Result<OwnedFd, Errno>;
    /// The `FS_IOC_GETFLAGS` inode flags.
    fn file_flags(&self, fd: BorrowedFd<'_>) -> Result<u32, Errno>;
    fn set_link_times(
        &self,
        at: Entry<'_>,
        atime: (i64, u32),
        mtime: (i64, u32),
    ) -> Result<(), Errno>;
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
        "filesystem calls must not run on the main thread"
    );
}

fn entry_stat(stat: &rustix::fs::Statx) -> EntryStat {
    let kind = FileType::from_raw_mode(u32::from(stat.stx_mode));
    let birth = StatxFlags::from_bits_retain(stat.stx_mask)
        .contains(StatxFlags::BTIME)
        .then_some((stat.stx_btime.tv_sec, stat.stx_btime.tv_nsec));
    EntryStat {
        identity: LocalFileIdentity {
            device: rustix::fs::makedev(stat.stx_dev_major, stat.stx_dev_minor),
            inode: stat.stx_ino,
            kind,
            birth,
        },
        kind,
        mode: u32::from(stat.stx_mode) & 0o7777,
        nlink: u64::from(stat.stx_nlink),
        size: stat.stx_size,
        atime: (stat.stx_atime.tv_sec, stat.stx_atime.tv_nsec),
        mtime: (stat.stx_mtime.tv_sec, stat.stx_mtime.tv_nsec),
        uid: stat.stx_uid,
        gid: stat.stx_gid,
    }
}

fn legacy_stat(stat: &rustix::fs::Stat) -> EntryStat {
    EntryStat {
        identity: LocalFileIdentity::from_stat(stat),
        kind: FileType::from_raw_mode(stat.st_mode),
        mode: stat.st_mode & 0o7777,
        nlink: stat.st_nlink,
        size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
        atime: (
            stat.st_atime,
            u32::try_from(stat.st_atime_nsec).unwrap_or_default(),
        ),
        mtime: (
            stat.st_mtime,
            u32::try_from(stat.st_mtime_nsec).unwrap_or_default(),
        ),
        uid: stat.st_uid,
        gid: stat.st_gid,
    }
}

fn statx_at(dir: BorrowedFd<'_>, name: &OsStr, flags: AtFlags) -> Result<EntryStat, Errno> {
    let mask = StatxFlags::TYPE
        | StatxFlags::MODE
        | StatxFlags::INO
        | StatxFlags::NLINK
        | StatxFlags::SIZE
        | StatxFlags::ATIME
        | StatxFlags::MTIME
        | StatxFlags::BTIME
        | StatxFlags::UID
        | StatxFlags::GID;
    match rustix::fs::statx(dir, name, flags | AtFlags::SYMLINK_NOFOLLOW, mask) {
        Ok(stat) => Ok(entry_stat(&stat)),
        Err(Errno::NOSYS) => {
            let at_flags = (flags & AtFlags::EMPTY_PATH) | AtFlags::SYMLINK_NOFOLLOW;
            rustix::fs::statat(dir, name, at_flags).map(|stat| legacy_stat(&stat))
        }
        Err(error) => Err(error),
    }
}

fn timespec((seconds, nanoseconds): (i64, u32)) -> rustix::fs::Timespec {
    rustix::fs::Timespec {
        tv_sec: seconds,
        tv_nsec: nanoseconds.into(),
    }
}

/// Reads a variable-length list or value, retrying when it grew in between.
fn read_sized(mut read: impl FnMut(&mut [u8]) -> Result<usize, Errno>) -> Result<Vec<u8>, Errno> {
    let mut attempts = 0;
    loop {
        let size = read(&mut [])?;
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buffer = vec![0; size];
        match read(&mut buffer) {
            Ok(length) => {
                buffer.truncate(length);
                return Ok(buffer);
            }
            Err(Errno::RANGE) if attempts < 3 => attempts += 1,
            Err(error) => return Err(error),
        }
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

    fn stat_fd(&self, fd: BorrowedFd<'_>) -> Result<EntryStat, Errno> {
        assert_off_main_thread();
        statx_at(
            fd,
            OsStr::new(""),
            AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC,
        )
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
            Err(_) => rustix::fs::fstat(dir).map_or(MountKey::Device(u64::MAX), |stat| {
                MountKey::Device(stat.st_dev)
            }),
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

    #[expect(
        unsafe_code,
        reason = "gio-rs asserts that FALSE always carries an error, which GLib #4060 breaks"
    )]
    fn trash(&self, path: &Path, cancellable: &gio::Cancellable) -> Result<bool, glib::Error> {
        use glib::translate::{ToGlibPtr, from_glib_full};

        assert_off_main_thread();
        let file = gio::File::for_path(path);
        let mut error = std::ptr::null_mut();
        // SAFETY: file and cancellable outlive the call, and error is a valid out-pointer.
        let trashed = unsafe {
            gio::ffi::g_file_trash(
                file.to_glib_none().0,
                cancellable.to_glib_none().0,
                &mut error,
            )
        };
        if error.is_null() {
            Ok(trashed != glib::ffi::GFALSE)
        } else {
            // SAFETY: GLib set the error and transferred its ownership to us.
            Err(unsafe { from_glib_full(error) })
        }
    }

    fn open_source(
        &self,
        at: Entry<'_>,
        listed: bool,
    ) -> Result<Option<(OwnedFd, EntryStat)>, Errno> {
        assert_off_main_thread();
        if !listed && statx_at(at.dir, at.name, AtFlags::empty())?.kind != FileType::RegularFile {
            return Ok(None);
        }
        // NONBLOCK, so an entry swapped for a FIFO meanwhile cannot block the open.
        let fd = retry_local_open(|| {
            rustix::fs::openat2(
                at.dir,
                at.name,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
            )
        })?;
        let stat = statx_at(fd.as_fd(), OsStr::new(""), AtFlags::EMPTY_PATH)?;
        if stat.kind != FileType::RegularFile {
            return Ok(None);
        }
        // Some FUSE daemons pass the flag on to their reads.
        rustix::fs::fcntl_setfl(&fd, OFlags::empty())?;
        Ok(Some((fd, stat)))
    }

    fn open_tmpfile(&self, dir: BorrowedFd<'_>, mode: Mode) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        rustix::fs::openat(
            dir,
            ".",
            OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC,
            mode,
        )
    }

    fn link_fd(&self, fd: BorrowedFd<'_>, to: Entry<'_>, through_proc: bool) -> Result<(), Errno> {
        assert_off_main_thread();
        if through_proc {
            rustix::fs::linkat(
                rustix::fs::CWD,
                format!("/proc/self/fd/{}", fd.as_raw_fd()),
                to.dir,
                to.name,
                AtFlags::SYMLINK_FOLLOW,
            )
        } else {
            rustix::fs::linkat(fd, "", to.dir, to.name, AtFlags::EMPTY_PATH)
        }
    }

    fn reflink(&self, target: BorrowedFd<'_>, source: BorrowedFd<'_>) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::ioctl_ficlone(target, source)
    }

    fn copy_range(
        &self,
        source: BorrowedFd<'_>,
        mut source_offset: u64,
        target: BorrowedFd<'_>,
        mut target_offset: u64,
        len: usize,
    ) -> Result<usize, Errno> {
        assert_off_main_thread();
        rustix::fs::copy_file_range(
            source,
            Some(&mut source_offset),
            target,
            Some(&mut target_offset),
            len,
        )
    }

    fn read_at(&self, fd: BorrowedFd<'_>, buffer: &mut [u8], offset: u64) -> Result<usize, Errno> {
        assert_off_main_thread();
        rustix::io::pread(fd, buffer, offset)
    }

    fn write_at(&self, fd: BorrowedFd<'_>, buffer: &[u8], offset: u64) -> Result<usize, Errno> {
        assert_off_main_thread();
        rustix::io::pwrite(fd, buffer, offset)
    }

    #[expect(
        unsafe_code,
        reason = "OwnedFd and rustix discard close errors, which report failed network writes"
    )]
    fn close(&self, fd: OwnedFd) -> Result<(), Errno> {
        assert_off_main_thread();
        let raw = fd.into_raw_fd();
        // SAFETY: `raw` came from an owned descriptor and is closed exactly once.
        if unsafe { libc::close(raw) } == 0 {
            return Ok(());
        }
        match Errno::from_io_error(&std::io::Error::last_os_error()) {
            // Linux always releases the descriptor, as `g_close` assumes.
            Some(Errno::INTR) | None => Ok(()),
            Some(error) => Err(error),
        }
    }

    fn chown_fd(
        &self,
        fd: BorrowedFd<'_>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<(), Errno> {
        assert_off_main_thread();
        // -1 means "unchanged" to fchown, and is never a real id.
        let uid = uid
            .filter(|uid| *uid != u32::MAX)
            .map(rustix::process::Uid::from_raw);
        let gid = gid
            .filter(|gid| *gid != u32::MAX)
            .map(rustix::process::Gid::from_raw);
        rustix::fs::fchown(fd, uid, gid)
    }

    fn chmod_fd(&self, fd: BorrowedFd<'_>, mode: u32) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::fchmod(fd, Mode::from_raw_mode(mode))
    }

    fn set_times_fd(
        &self,
        fd: BorrowedFd<'_>,
        atime: (i64, u32),
        mtime: (i64, u32),
    ) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::futimens(
            fd,
            &rustix::fs::Timestamps {
                last_access: timespec(atime),
                last_modification: timespec(mtime),
            },
        )
    }

    fn xattr_names(&self, fd: BorrowedFd<'_>) -> Result<Vec<OsString>, Errno> {
        assert_off_main_thread();
        let list = read_sized(|buffer| rustix::fs::flistxattr(fd, buffer))?;
        Ok(list
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .map(|name| OsString::from_vec(name.to_vec()))
            .collect())
    }

    fn get_xattr(&self, fd: BorrowedFd<'_>, name: &OsStr) -> Result<Vec<u8>, Errno> {
        assert_off_main_thread();
        read_sized(|buffer| rustix::fs::fgetxattr(fd, name, buffer))
    }

    fn set_xattr(&self, fd: BorrowedFd<'_>, name: &OsStr, value: &[u8]) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::fsetxattr(fd, name, value, rustix::fs::XattrFlags::empty())
    }

    fn mount_type(&self, fd: BorrowedFd<'_>) -> Option<String> {
        assert_off_main_thread();
        let mounts = MountTable::current();
        let fs_type = match rustix::fs::statx(
            fd,
            "",
            AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC,
            StatxFlags::MNT_ID,
        ) {
            Ok(stat)
                if StatxFlags::from_bits_retain(stat.stx_mask).contains(StatxFlags::MNT_ID) =>
            {
                mounts.fs_type_of(stat.stx_mnt_id)
            }
            // Before Linux 5.8.
            _ => {
                let path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()?;
                mounts.fs_type_for(&path)
            }
        };
        fs_type.map(str::to_owned)
    }

    fn open_dir(&self, at: Entry<'_>) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        retry_local_open(|| {
            rustix::fs::openat2(
                at.dir,
                at.name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
            )
        })
    }

    fn list(&self, dir: BorrowedFd<'_>) -> Result<Vec<(OsString, FileType)>, Errno> {
        assert_off_main_thread();
        let mut entries = Vec::new();
        for entry in rustix::fs::Dir::read_from(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if name != c"." && name != c".." {
                entries.push((
                    OsString::from_vec(name.to_bytes().to_vec()),
                    entry.file_type(),
                ));
            }
        }
        Ok(entries)
    }

    fn create_node(&self, at: Entry<'_>, kind: FileType, mode: Mode) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::mknodat(at.dir, at.name, kind, mode, 0)
    }

    fn open_fifo(&self, at: Entry<'_>) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        rustix::fs::openat(
            at.dir,
            at.name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
    }

    fn create_readonly(&self, at: Entry<'_>, mode: Mode) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        rustix::fs::openat(
            at.dir,
            at.name,
            OFlags::RDONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            mode,
        )
    }

    fn open_write(&self, at: Entry<'_>) -> Result<OwnedFd, Errno> {
        assert_off_main_thread();
        // NONBLOCK, so a pipe swapped in for the claim fails at once instead
        // of waiting for a reader.
        let fd = rustix::fs::openat(
            at.dir,
            at.name,
            OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        rustix::fs::fcntl_setfl(&fd, OFlags::empty())?;
        Ok(fd)
    }

    fn file_flags(&self, fd: BorrowedFd<'_>) -> Result<u32, Errno> {
        assert_off_main_thread();
        rustix::fs::ioctl_getflags(fd).map(|flags| flags.bits())
    }

    fn set_link_times(
        &self,
        at: Entry<'_>,
        atime: (i64, u32),
        mtime: (i64, u32),
    ) -> Result<(), Errno> {
        assert_off_main_thread();
        rustix::fs::utimensat(
            at.dir,
            at.name,
            &rustix::fs::Timestamps {
                last_access: timespec(atime),
                last_modification: timespec(mtime),
            },
            AtFlags::SYMLINK_NOFOLLOW,
        )
    }
}
