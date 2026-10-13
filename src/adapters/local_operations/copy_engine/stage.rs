// SPDX-License-Identifier: MIT

//! Hidden stages for copies, and publishing one without replacing anything.

use std::{
    ffi::{OsStr, OsString},
    os::fd::{AsFd, BorrowedFd, OwnedFd},
};

use rustix::{
    fs::{Mode, RenameFlags},
    io::Errno,
};

use crate::adapters::{
    local_operations::{
        LocalFileIdentity,
        fs_ops::{Entry, EntryStat, FsOps},
        placement::{self, COPY_PREFIX, PlaceError, Placed},
    },
    volume::Family,
};

/// Local POSIX filesystems, which link an unnamed file atomically. The SMB
/// client fakes `O_TMPFILE` with a visible file, and FUSE daemons that
/// implement it are unprobed.
pub(super) fn tmpfile_allowed(family: Family) -> bool {
    matches!(
        family,
        Family::Ext
            | Family::Xfs
            | Family::Btrfs
            | Family::F2fs
            | Family::Tmpfs
            | Family::Bcachefs
            | Family::Zfs
            | Family::Overlay
    )
}

/// A new hidden file in `dir`, with the stat of its own descriptor.
pub(super) fn create_file(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
) -> Result<(OsString, (OwnedFd, EntryStat)), PlaceError> {
    placement::reserve_hidden_name(COPY_PREFIX, |name| {
        let fd = fs
            .create_exclusive(Entry { dir, name }, Mode::from_raw_mode(0o600))
            .map_err(placement::file_creation_error)?;
        let stat = fs.stat_fd(fd.as_fd()).map_err(PlaceError::Io)?;
        Ok((fd, stat))
    })
}

/// Puts the stage `stage`, recorded as `recorded`, at `name` in the same
/// folder without replacing an entry.
pub(in crate::adapters::local_operations) fn publish(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
    stage: &OsStr,
    recorded: LocalFileIdentity,
    name: &OsStr,
) -> Result<Placed, PlaceError> {
    let stage = Entry { dir, name: stage };
    let target = Entry { dir, name };
    // The stage is our own hidden name, so the first rung needs no check.
    let error = match fs.rename(stage, target, RenameFlags::NOREPLACE) {
        Ok(()) => return Ok(Placed::Renamed),
        Err(Errno::EXIST | Errno::NOTEMPTY) => return Err(PlaceError::Exists),
        Err(Errno::XDEV) => return Err(PlaceError::CrossDevice),
        Err(error) => error,
    };
    match fs.stat(stage, false) {
        Ok(current) if current.identity == recorded => {}
        Ok(_) => return Err(PlaceError::Changed),
        // A lost reply may have published it; nothing tells where the copy is.
        Err(Errno::NOENT) => return Err(PlaceError::Unconfirmed),
        Err(error) => return Err(PlaceError::Io(error)),
    }
    placement::place_after_rename(fs, stage, target, Some(recorded), error)
}
