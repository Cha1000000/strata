// SPDX-License-Identifier: MIT

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    os::fd::{AsFd, BorrowedFd, OwnedFd},
};

use rustix::{
    fs::{FileType, Mode, RenameFlags},
    io::Errno,
};
use unicode_normalization::UnicodeNormalization;

use crate::adapters::{
    local_operations::{
        LocalFileIdentity,
        fs_ops::{Entry, EntryStat, FsOps},
        placement::{self, PlaceError, Placed},
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

/// Where two different names never collide, unless a folder is casefolded.
pub(super) fn names_never_collide(family: Family) -> bool {
    matches!(
        family,
        Family::Ext | Family::Xfs | Family::Btrfs | Family::F2fs | Family::Tmpfs
    )
}

/// Which of a folder's names a filesystem that compares names loosely may
/// take for another of them. Every name is, where one cannot be folded or
/// looks like a FAT short name (`PROGRA~1`), which may alias any long name.
pub(super) fn colliding(names: &[&OsStr]) -> Vec<bool> {
    let short_name = |name: &&OsStr| {
        name.as_encoded_bytes()
            .windows(2)
            .any(|pair| pair[0] == b'~' && pair[1].is_ascii_digit())
    };
    let keys = names
        .iter()
        .map(|name| fold(name))
        .collect::<Option<Vec<_>>>();
    let Some(keys) = keys.filter(|_| !names.iter().any(short_name)) else {
        return vec![true; names.len()];
    };
    let mut counts = HashMap::<&str, usize>::new();
    for key in &keys {
        *counts.entry(key).or_default() += 1;
    }
    keys.iter().map(|key| counts[key.as_str()] > 1).collect()
}

pub(in crate::adapters::local_operations) fn collision_key(name: &OsStr) -> OsString {
    fold(name).map_or_else(|| name.to_os_string(), OsString::from)
}

/// The same for any two names a filesystem may take for one: compatibility
/// forms decomposed, case folded, and without the trailing dots and spaces
/// Windows drops. It may join names that differ, never split ones that alias.
fn fold(name: &OsStr) -> Option<String> {
    let name = name.to_str()?.trim_end_matches(['.', ' ']);
    if name.is_ascii() {
        return Some(name.to_ascii_lowercase());
    }
    Some(
        name.nfkd()
            .flat_map(char::to_uppercase)
            .flat_map(char::to_lowercase)
            .nfkd()
            .collect(),
    )
}

pub(in crate::adapters::local_operations) fn create_file(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
    prefix: &str,
) -> Result<(OsString, (OwnedFd, EntryStat)), PlaceError> {
    placement::reserve_hidden_name(prefix, |name| {
        let entry = Entry { dir, name };
        let fd = match fs.create_exclusive(entry, Mode::from_raw_mode(0o600)) {
            Ok(fd) => fd,
            Err(Errno::EXIST) => lost_file(fs, entry).ok_or(PlaceError::Exists)?,
            Err(error) => return Err(placement::file_creation_error(error)),
        };
        let stat = fs.stat_fd(fd.as_fd()).map_err(PlaceError::Io)?;
        Ok((fd, stat))
    })
}

fn lost_file(fs: &dyn FsOps, entry: Entry<'_>) -> Option<OwnedFd> {
    let fresh = |stat: &EntryStat| stat.kind == FileType::RegularFile && stat.size == 0;
    let identity = placement::left_by_lost_reply(fs, entry, |stat| fresh(stat) && stat.nlink == 1)?;
    let fd = fs.open_write(entry).ok()?;
    fs.stat_fd(fd.as_fd())
        .is_ok_and(|opened| opened.identity == identity && fresh(&opened))
        .then_some(fd)
}

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
