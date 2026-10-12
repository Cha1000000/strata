// SPDX-License-Identifier: MIT

//! Puts an entry at a new name without ever replacing whatever is there:
//! `renameat2(NOREPLACE)`, then a hard link for files or a fresh empty
//! placeholder for folders, then a recreated link for symlinks. Errors that
//! may mean "unsupported" are only learned from definitive errnos or from a
//! probe on Strata's own names, and nothing is unlinked without proof.

use std::{
    cell::Cell,
    ffi::{OsStr, OsString},
    os::fd::{AsFd, BorrowedFd},
};

use gtk::glib;
use rustix::{
    fs::{AtFlags, FileType, Mode, RenameFlags},
    io::Errno,
};

use super::{
    LocalFileIdentity,
    fs_ops::{Entry, EntryStat, FsOps},
};
use crate::adapters::volume::{
    MountCapabilities, MountKey, learn_mount_capability, mount_capabilities,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Placed {
    Renamed,
    Linked,
    ReservedDirectory,
    RecreatedSymlink,
    /// Only from a rename: a plain rename over an empty file reserved first.
    OverReservedFile,
    /// Only from a rename: a plain rename of a folder to a name found free.
    AfterCheck,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlaceError {
    Exists,
    CrossDevice,
    Unsupported,
    InvalidName,
    IntoItself,
    Changed,
    /// The outcome could not be proven, so both names were kept.
    Unconfirmed,
    Io(Errno),
}

const ENOTSUPP: Errno = Errno::from_raw_os_error(524);
const PROBE_PREFIX: &str = ".strata-probe-";
const RENAME_PREFIX: &str = ".strata-rename-";
const NAME_ATTEMPTS: usize = 8;

fn is_unsupported(error: Errno) -> bool {
    matches!(error, Errno::NOSYS | Errno::OPNOTSUPP | ENOTSUPP)
}

fn may_have_lost_reply(error: Errno) -> bool {
    matches!(
        error,
        Errno::NOENT | Errno::IO | Errno::INTR | Errno::TIMEDOUT | Errno::STALE
    )
}

fn at<'a>(dir: BorrowedFd<'a>, name: &'a OsStr) -> Entry<'a> {
    Entry { dir, name }
}

/// Looks the mount key up only when a fallback needs it, so the first rung
/// costs a single syscall.
struct Mount<'a> {
    fs: &'a dyn FsOps,
    dir: BorrowedFd<'a>,
    key: Cell<Option<MountKey>>,
}

impl<'a> Mount<'a> {
    fn new(fs: &'a dyn FsOps, dir: BorrowedFd<'a>) -> Self {
        Self {
            fs,
            dir,
            key: Cell::new(None),
        }
    }

    fn key(&self) -> MountKey {
        if let Some(key) = self.key.get() {
            return key;
        }
        let key = self.fs.mount_key(self.dir);
        self.key.set(Some(key));
        key
    }

    fn capabilities(&self) -> MountCapabilities {
        mount_capabilities(self.key())
    }

    fn learn(&self, learn: impl FnOnce(&mut MountCapabilities)) {
        learn_mount_capability(self.key(), learn);
    }

    fn probe(&self, links: bool, folders: bool) -> Result<Probed, Errno> {
        let probed = probe(self.fs, self.dir, links, folders)?;
        self.learn(|capabilities| {
            for (field, value) in [
                (&mut capabilities.no_replace, probed.no_replace),
                (&mut capabilities.hard_links, probed.hard_links),
                (
                    &mut capabilities.rename_over_empty_directory,
                    probed.rename_over_empty_directory,
                ),
            ] {
                if value.is_some() {
                    *field = value;
                }
            }
        });
        Ok(probed)
    }
}

pub(super) fn place(
    fs: &dyn FsOps,
    from: Entry<'_>,
    to: Entry<'_>,
    expected: Option<LocalFileIdentity>,
) -> Result<Placed, PlaceError> {
    let mount = Mount::new(fs, to.dir);
    let mut source = None;
    match fs.rename(from, to, RenameFlags::NOREPLACE) {
        Ok(()) => return Ok(Placed::Renamed),
        Err(Errno::EXIST | Errno::NOTEMPTY) => return Err(PlaceError::Exists),
        Err(Errno::XDEV) => return Err(PlaceError::CrossDevice),
        Err(Errno::INVAL) => {
            let stat = stat_source(fs, from)?;
            if stat.is_directory() && contains(fs, stat.identity, to.dir) != Some(false) {
                return Err(PlaceError::IntoItself);
            }
            let no_replace = match mount.capabilities().no_replace {
                None => mount
                    .probe(false, false)
                    .ok()
                    .and_then(|probed| probed.no_replace),
                known => known,
            };
            if no_replace == Some(true) {
                return Err(PlaceError::InvalidName);
            }
            source = Some(stat);
        }
        Err(error) if is_unsupported(error) => {
            mount.learn(|capabilities| capabilities.no_replace = Some(false));
        }
        Err(error) if may_have_lost_reply(error) => {
            return resolve(fs, from, to, expected, error).map(|()| Placed::Renamed);
        }
        Err(error) => return Err(PlaceError::Io(error)),
    }

    let source = match source {
        Some(source) => source,
        None => stat_source(fs, from)?,
    };
    if expected.is_some_and(|expected| expected != source.identity) {
        return Err(PlaceError::Changed);
    }
    if source.is_directory() {
        place_directory(fs, &mount, from, to, &source)
    } else {
        place_non_directory(fs, &mount, from, to, &source)
    }
}

fn stat_source(fs: &dyn FsOps, from: Entry<'_>) -> Result<EntryStat, PlaceError> {
    fs.stat(from, true).map_err(PlaceError::Io)
}

fn exists(fs: &dyn FsOps, entry: Entry<'_>) -> Result<bool, PlaceError> {
    match fs.stat(entry, false) {
        Ok(_) => Ok(true),
        Err(Errno::NOENT) => Ok(false),
        Err(error) => Err(PlaceError::Io(error)),
    }
}

/// Decides a reply that may have been lost from where the item now is.
fn resolve(
    fs: &dyn FsOps,
    from: Entry<'_>,
    to: Entry<'_>,
    identity: Option<LocalFileIdentity>,
    error: Errno,
) -> Result<(), PlaceError> {
    let Some(identity) = identity else {
        return Err(PlaceError::Io(error));
    };
    match (fs.stat(from, true), fs.stat(to, true)) {
        (Err(Errno::NOENT), Ok(target)) if target.identity == identity => Ok(()),
        (Ok(source), Err(Errno::NOENT)) if source.identity == identity => {
            Err(PlaceError::Io(error))
        }
        (Ok(source), Ok(target)) if source.identity == identity && target.identity != identity => {
            Err(PlaceError::Io(error))
        }
        _ => Err(PlaceError::Unconfirmed),
    }
}

fn place_non_directory(
    fs: &dyn FsOps,
    mount: &Mount<'_>,
    from: Entry<'_>,
    to: Entry<'_>,
    source: &EntryStat,
) -> Result<Placed, PlaceError> {
    if mount.capabilities().hard_links != Some(false) {
        match fs.link(from, to) {
            Ok(()) => return finish_link(fs, from, to, source),
            Err(Errno::EXIST) => {
                return if link_reply_was_lost(fs, to, source) {
                    finish_link(fs, from, to, source)
                } else {
                    Err(PlaceError::Exists)
                };
            }
            Err(Errno::PERM) => {
                if exists(fs, to)? {
                    return Err(PlaceError::Exists);
                }
                let hard_links = match mount.capabilities().hard_links {
                    None => mount
                        .probe(true, false)
                        .ok()
                        .and_then(|probed| probed.hard_links),
                    known => known,
                };
                if hard_links == Some(true) {
                    return Err(PlaceError::Io(Errno::PERM));
                }
            }
            Err(Errno::MLINK) => {}
            Err(Errno::XDEV) => return Err(PlaceError::CrossDevice),
            Err(Errno::INVAL) => return Err(PlaceError::InvalidName),
            Err(error) if is_unsupported(error) => {
                mount.learn(|capabilities| capabilities.hard_links = Some(false));
            }
            Err(error) if may_have_lost_reply(error) => {
                return match fs.stat(to, true) {
                    Ok(target) if target.identity == source.identity => {
                        finish_link(fs, from, to, source)
                    }
                    Ok(_) | Err(Errno::NOENT) => Err(PlaceError::Io(error)),
                    Err(_) => Err(PlaceError::Unconfirmed),
                };
            }
            Err(error) => return Err(PlaceError::Io(error)),
        }
    }
    recreate_symlink(fs, from, to, source)
}

/// An `EEXIST` can be the lost reply of a link that did happen; a case alias
/// or an older hard link fails the link-count or exact-name test.
fn link_reply_was_lost(fs: &dyn FsOps, to: Entry<'_>, source: &EntryStat) -> bool {
    fs.stat(to, true).is_ok_and(|target| {
        target.identity == source.identity && target.nlink == source.nlink + 1
    }) && fs
        .names(at(to.dir, OsStr::new(".")))
        .is_ok_and(|names| names.iter().any(|name| name == to.name))
}

fn finish_link(
    fs: &dyn FsOps,
    from: Entry<'_>,
    to: Entry<'_>,
    source: &EntryStat,
) -> Result<Placed, PlaceError> {
    let current = match fs.stat(from, true) {
        Ok(current) => current,
        Err(Errno::NOENT) => return Ok(Placed::Linked),
        Err(_) => return Err(PlaceError::Unconfirmed),
    };
    if current.identity != source.identity {
        return Err(PlaceError::Changed);
    }
    let proven = current.nlink >= 2
        || fs
            .stat(to, false)
            .is_ok_and(|target| target.identity == source.identity);
    if !proven {
        return Err(PlaceError::Unconfirmed);
    }
    match fs.unlink(from, AtFlags::empty()) {
        Ok(()) => Ok(Placed::Linked),
        Err(Errno::NOENT) => {
            resolve(fs, from, to, Some(source.identity), Errno::NOENT).map(|()| Placed::Linked)
        }
        Err(error) => {
            if fs.stat(to, true).is_ok_and(|target| {
                target.identity == source.identity && target.nlink >= 2
            }) {
                let _ = fs.unlink(to, AtFlags::empty());
            }
            Err(PlaceError::Io(error))
        }
    }
}

fn recreate_symlink(
    fs: &dyn FsOps,
    from: Entry<'_>,
    to: Entry<'_>,
    source: &EntryStat,
) -> Result<Placed, PlaceError> {
    if source.kind != FileType::Symlink {
        return Err(PlaceError::Unsupported);
    }
    let link = fs.readlink(from).map_err(PlaceError::Io)?;
    match fs.symlink(&link, to) {
        Ok(()) => {}
        Err(Errno::EXIST) => return Err(PlaceError::Exists),
        Err(Errno::PERM) if exists(fs, to)? => return Err(PlaceError::Exists),
        Err(Errno::PERM) => return Err(PlaceError::Unsupported),
        Err(Errno::INVAL) => return Err(PlaceError::InvalidName),
        Err(error) => return Err(PlaceError::Io(error)),
    }
    let created = fs
        .stat(to, false)
        .map_err(|_| PlaceError::Unconfirmed)?
        .identity;
    match fs.stat(from, true) {
        Ok(current) if current.identity == source.identity => {
            match fs.unlink(from, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => Ok(Placed::RecreatedSymlink),
                Err(error) => {
                    remove_if_unchanged(fs, to, created, AtFlags::empty());
                    Err(PlaceError::Io(error))
                }
            }
        }
        Err(Errno::NOENT) => Ok(Placed::RecreatedSymlink),
        _ => {
            remove_if_unchanged(fs, to, created, AtFlags::empty());
            Err(PlaceError::Changed)
        }
    }
}

pub(super) fn remove_if_unchanged(
    fs: &dyn FsOps,
    entry: Entry<'_>,
    identity: LocalFileIdentity,
    flags: AtFlags,
) -> bool {
    fs.stat(entry, true)
        .is_ok_and(|current| current.identity == identity)
        && fs.unlink(entry, flags).is_ok()
}

fn placeholder_is_ours(fs: &dyn FsOps, to: Entry<'_>, placeholder: LocalFileIdentity) -> bool {
    fs.stat(to, true)
        .is_ok_and(|current| current.identity == placeholder)
        && fs.names(to).is_ok_and(|names| names.is_empty())
}

fn remove_placeholder(fs: &dyn FsOps, to: Entry<'_>, placeholder: LocalFileIdentity) -> bool {
    placeholder_is_ours(fs, to, placeholder) && fs.unlink(to, AtFlags::REMOVEDIR).is_ok()
}

fn place_directory(
    fs: &dyn FsOps,
    mount: &Mount<'_>,
    from: Entry<'_>,
    to: Entry<'_>,
    source: &EntryStat,
) -> Result<Placed, PlaceError> {
    if mount.capabilities().rename_over_empty_directory == Some(false) {
        return Err(PlaceError::Unsupported);
    }
    match fs.mkdir(to, Mode::from_raw_mode(0o700)) {
        Ok(()) => {}
        Err(Errno::EXIST) => return Err(PlaceError::Exists),
        Err(Errno::PERM) if exists(fs, to)? => return Err(PlaceError::Exists),
        Err(Errno::INVAL) => return Err(PlaceError::InvalidName),
        // A lost reply leaves an empty folder that cannot be told apart from a racer's.
        Err(error) if may_have_lost_reply(error) => return Err(PlaceError::Exists),
        Err(error) => return Err(PlaceError::Io(error)),
    }
    let placeholder = fs.stat(to, false).map_err(PlaceError::Io)?.identity;
    match fs.rename(from, to, RenameFlags::empty()) {
        Ok(()) => Ok(Placed::ReservedDirectory),
        Err(error @ (Errno::NOTEMPTY | Errno::EXIST | Errno::PERM)) => {
            if !placeholder_is_ours(fs, to, placeholder) {
                return Err(PlaceError::Exists);
            }
            let _ = fs.unlink(to, AtFlags::REMOVEDIR);
            if error != Errno::PERM {
                mount.learn(|capabilities| capabilities.rename_over_empty_directory = Some(false));
            }
            Err(PlaceError::Unsupported)
        }
        Err(Errno::NOTDIR | Errno::ISDIR) => Err(PlaceError::Exists),
        Err(Errno::INVAL) => {
            remove_placeholder(fs, to, placeholder);
            if contains(fs, source.identity, to.dir) == Some(false) {
                Err(PlaceError::InvalidName)
            } else {
                Err(PlaceError::IntoItself)
            }
        }
        Err(Errno::XDEV) => {
            remove_placeholder(fs, to, placeholder);
            Err(PlaceError::CrossDevice)
        }
        Err(error) if may_have_lost_reply(error) => {
            match (fs.stat(from, true), fs.stat(to, true)) {
                (Err(Errno::NOENT), Ok(target)) if target.identity == source.identity => {
                    Ok(Placed::ReservedDirectory)
                }
                (Ok(current), Ok(target))
                    if current.identity == source.identity && target.identity == placeholder =>
                {
                    remove_placeholder(fs, to, placeholder);
                    Err(PlaceError::Io(error))
                }
                (Ok(current), _) if current.identity == source.identity => {
                    Err(PlaceError::Io(error))
                }
                _ => Err(PlaceError::Unconfirmed),
            }
        }
        Err(error) => {
            remove_placeholder(fs, to, placeholder);
            Err(PlaceError::Io(error))
        }
    }
}

/// Whether `dir` is `ancestor` or inside it, walking `..` (which crosses
/// mount boundaries) up to the root. `None` when the walk fails.
pub(super) fn contains(
    fs: &dyn FsOps,
    ancestor: LocalFileIdentity,
    dir: BorrowedFd<'_>,
) -> Option<bool> {
    let mut current = fs.stat_dir(dir).ok()?;
    if current.identity == ancestor {
        return Some(true);
    }
    let mut handle = fs.open_parent(dir).ok()?;
    for _ in 0..4096 {
        let parent = fs.stat_dir(handle.as_fd()).ok()?;
        if parent.identity == ancestor {
            return Some(true);
        }
        if parent.identity == current.identity {
            return Some(false);
        }
        current = parent;
        handle = fs.open_parent(handle.as_fd()).ok()?;
    }
    None
}

/// Tries fixed-length `<prefix><uuid>` names until `attempt` stops reporting
/// that the name is taken.
pub(super) fn reserve_hidden_name<T>(
    prefix: &str,
    mut attempt: impl FnMut(&OsStr) -> Result<T, PlaceError>,
) -> Result<(OsString, T), PlaceError> {
    for _ in 0..NAME_ATTEMPTS {
        let name = OsString::from(format!("{prefix}{}", glib::uuid_string_random()));
        match attempt(&name) {
            Err(PlaceError::Exists) => continue,
            result => return result.map(|value| (name, value)),
        }
    }
    Err(PlaceError::Exists)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Probed {
    pub no_replace: Option<bool>,
    pub hard_links: Option<bool>,
    pub rename_over_empty_directory: Option<bool>,
}

pub(super) fn creation_error(error: Errno) -> PlaceError {
    match error {
        Errno::EXIST => PlaceError::Exists,
        Errno::INVAL => PlaceError::InvalidName,
        error => PlaceError::Io(error),
    }
}

/// Tries each primitive on new names of Strata's own in `dir`, where no
/// errno can be blamed on the user's item. Fields stay `None` when the probe
/// cannot tell.
pub(super) fn probe(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
    links: bool,
    folders: bool,
) -> Result<Probed, Errno> {
    let mut probed = Probed::default();
    let created = reserve_hidden_name(PROBE_PREFIX, |name| {
        drop(
            fs.create_exclusive(at(dir, name), Mode::from_raw_mode(0o600))
                .map_err(creation_error)?,
        );
        fs.stat(at(dir, name), false)
            .map(|stat| stat.identity)
            .map_err(PlaceError::Io)
    });
    let (mut current, identity) = match created {
        Ok(created) => created,
        Err(PlaceError::Io(error)) => return Err(error),
        Err(_) => return Ok(probed),
    };

    if let Ok((renamed, supported)) = reserve_hidden_name(PROBE_PREFIX, |name| {
        match fs.rename(at(dir, &current), at(dir, name), RenameFlags::NOREPLACE) {
            Ok(()) => Ok(Some(true)),
            Err(Errno::EXIST) => Err(PlaceError::Exists),
            Err(error) if error == Errno::INVAL || is_unsupported(error) => Ok(Some(false)),
            Err(_) => Ok(None),
        }
    }) {
        probed.no_replace = supported;
        if supported == Some(true) {
            current = renamed;
        }
    }

    if links
        && let Ok((linked, supported)) = reserve_hidden_name(PROBE_PREFIX, |name| {
            match fs.link(at(dir, &current), at(dir, name)) {
                Ok(()) => Ok(Some(true)),
                Err(Errno::EXIST) => Err(PlaceError::Exists),
                Err(error) if error == Errno::PERM || is_unsupported(error) => Ok(Some(false)),
                Err(_) => Ok(None),
            }
        })
    {
        probed.hard_links = supported;
        if supported == Some(true) {
            remove_if_unchanged(fs, at(dir, &linked), identity, AtFlags::empty());
        }
    }
    remove_if_unchanged(fs, at(dir, &current), identity, AtFlags::empty());

    if folders {
        probed.rename_over_empty_directory = probe_folders(fs, dir);
    }
    Ok(probed)
}

fn probe_folders(fs: &dyn FsOps, dir: BorrowedFd<'_>) -> Option<bool> {
    let mkdir = |name: &OsStr| {
        fs.mkdir(at(dir, name), Mode::from_raw_mode(0o700))
            .map_err(creation_error)?;
        fs.stat(at(dir, name), false)
            .map(|stat| stat.identity)
            .map_err(PlaceError::Io)
    };
    let (moved, moved_identity) = reserve_hidden_name(PROBE_PREFIX, mkdir).ok()?;
    let Ok((target, target_identity)) = reserve_hidden_name(PROBE_PREFIX, mkdir) else {
        remove_placeholder(fs, at(dir, &moved), moved_identity);
        return None;
    };
    let result = match fs.rename(at(dir, &moved), at(dir, &target), RenameFlags::empty()) {
        Ok(()) => {
            remove_placeholder(fs, at(dir, &target), moved_identity);
            return Some(true);
        }
        Err(Errno::NOTEMPTY | Errno::EXIST)
            if placeholder_is_ours(fs, at(dir, &target), target_identity) =>
        {
            Some(false)
        }
        Err(_) => None,
    };
    remove_placeholder(fs, at(dir, &moved), moved_identity);
    remove_placeholder(fs, at(dir, &target), target_identity);
    result
}

#[derive(Debug)]
pub(super) struct SetAside {
    pub name: OsString,
    /// As observed after the move; a recreated symlink is a new inode.
    pub identity: LocalFileIdentity,
}

/// Moves `from` to the free name `to`, refusing an entry that is no longer
/// `expected`, and returns the identity now at `to`.
pub(super) fn move_aside(
    fs: &dyn FsOps,
    from: Entry<'_>,
    to: Entry<'_>,
    expected: LocalFileIdentity,
) -> Result<LocalFileIdentity, PlaceError> {
    let placed = place(fs, from, to, Some(expected))?;
    let identity = fs
        .stat(to, true)
        .map_err(|_| PlaceError::Unconfirmed)?
        .identity;
    if placed != Placed::RecreatedSymlink && identity != expected {
        return Err(match place(fs, to, from, Some(identity)) {
            Ok(_) => PlaceError::Changed,
            Err(_) => PlaceError::Unconfirmed,
        });
    }
    Ok(identity)
}

/// Moves the entry at `entry` to a hidden sibling, refusing an entry that is
/// no longer `expected`.
pub(super) fn set_aside(
    fs: &dyn FsOps,
    entry: Entry<'_>,
    prefix: &str,
    expected: LocalFileIdentity,
) -> Result<SetAside, PlaceError> {
    reserve_hidden_name(prefix, |name| {
        move_aside(fs, entry, at(entry.dir, name), expected)
    })
    .map(|(name, identity)| SetAside { name, identity })
    .map_err(|error| match error {
        PlaceError::InvalidName => PlaceError::Unsupported,
        error => error,
    })
}

pub(super) fn put_back(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
    aside: &SetAside,
    name: &OsStr,
) -> Result<(), PlaceError> {
    place(fs, at(dir, &aside.name), at(dir, name), Some(aside.identity)).map(drop)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Feasibility {
    Yes,
    No,
}

/// Whether an existing entry in `dir` can be moved aside without risking a
/// clobber, probing the mount when nothing is known yet.
pub(super) fn can_set_aside(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
    directory: bool,
) -> Result<Feasibility, PlaceError> {
    let feasible = |capabilities: MountCapabilities| {
        let fallback = if directory {
            capabilities.rename_over_empty_directory
        } else {
            capabilities.hard_links
        };
        match (capabilities.no_replace, fallback) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        }
    };
    let mount = Mount::new(fs, dir);
    let known = match feasible(mount.capabilities()) {
        Some(known) => known,
        None => {
            mount
                .probe(!directory, directory)
                .map_err(PlaceError::Io)?;
            feasible(mount.capabilities()) == Some(true)
        }
    };
    Ok(if known {
        Feasibility::Yes
    } else {
        Feasibility::No
    })
}

/// Atomically swaps two entries. `Ok(false)` means the filesystem rejected
/// the flag, which is remembered for the mount.
pub(super) fn exchange(fs: &dyn FsOps, from: Entry<'_>, to: Entry<'_>) -> Result<bool, PlaceError> {
    match fs.rename(from, to, RenameFlags::EXCHANGE) {
        Ok(()) => Ok(true),
        Err(error) if error == Errno::INVAL || is_unsupported(error) => {
            Mount::new(fs, to.dir).learn(|capabilities| capabilities.exchange = Some(false));
            Ok(false)
        }
        Err(Errno::XDEV) => Err(PlaceError::CrossDevice),
        Err(error) => Err(PlaceError::Io(error)),
    }
}

/// Filesystems known to implement `RENAME_EXCHANGE` faithfully. FUSE,
/// network, overlay and the other FAT-family drivers are left out because
/// some silently overwrite instead.
const EXCHANGE_FILESYSTEMS: [u32; 8] = [
    0xEF53,      // ext2/3/4
    0x5846_5342, // XFS
    0x9123_683E, // btrfs
    0xF2F5_2010, // f2fs
    0x0102_1994, // tmpfs
    0xCA45_1A4E, // bcachefs
    0x2FC1_2FC1, // ZFS
    0x4D44,      // vfat
];

/// Reads the filesystem type afresh on every call; it is never cached.
pub(super) fn exchange_allowed(fs: &dyn FsOps, dir: BorrowedFd<'_>) -> bool {
    fs.fs_type(dir)
        .is_some_and(|fs_type| EXCHANGE_FILESYSTEMS.contains(&(fs_type as u32)))
        && Mount::new(fs, dir).capabilities().exchange != Some(false)
}

/// How an existing entry is moved out of the way where Trash is unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Displacement {
    Exchange,
    SetAside,
}

/// The safe way to displace an entry in `dir` without Trash, or `None`
/// when the mount offers none.
pub(super) fn displacement_without_trash(
    fs: &dyn FsOps,
    dir: BorrowedFd<'_>,
    directory: bool,
) -> Result<Option<Displacement>, PlaceError> {
    if exchange_allowed(fs, dir) {
        return Ok(Some(Displacement::Exchange));
    }
    Ok((can_set_aside(fs, dir, directory)? == Feasibility::Yes).then_some(Displacement::SetAside))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Renamed {
    Direct(Placed),
    /// Only the case changed, through a hidden temporary name.
    CaseOnly,
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RenameFailure {
    Place(PlaceError),
    /// A case-only rename failed and the item could not be moved back from
    /// its temporary name.
    Stranded {
        temporary: OsString,
        error: PlaceError,
    },
}

/// Renames `from` to `to` without replacing another item. A name that only
/// differs in case on a case-insensitive mount resolves to the item itself,
/// so it is renamed in two steps through a hidden temporary name.
pub(super) fn rename_in_place(
    fs: &dyn FsOps,
    from: Entry<'_>,
    to: Entry<'_>,
) -> Result<Renamed, RenameFailure> {
    if from.name == to.name && same_directory(fs, from.dir, to.dir) {
        return Ok(Renamed::Unchanged);
    }
    let rename = Renaming::new(fs, to.dir);
    match rename.place(from, to, None) {
        Err(PlaceError::Exists) => {}
        result => return result.map(Renamed::Direct).map_err(RenameFailure::Place),
    }
    let Some(identity) = alias_of(fs, from, to) else {
        return Err(RenameFailure::Place(PlaceError::Exists));
    };
    let (temporary, _) = reserve_hidden_name(RENAME_PREFIX, |name| {
        rename.place(from, at(from.dir, name), Some(identity))
    })
    .map_err(|error| {
        RenameFailure::Place(match error {
            PlaceError::InvalidName => PlaceError::Unsupported,
            error => error,
        })
    })?;
    let parked = at(from.dir, &temporary);
    match rename.place(parked, to, Some(identity)) {
        Ok(_) => Ok(Renamed::CaseOnly),
        Err(error) => match rename.place(parked, from, Some(identity)) {
            Ok(_) => Err(RenameFailure::Place(error)),
            Err(_) => Err(RenameFailure::Stranded { temporary, error }),
        },
    }
}

fn same_directory(fs: &dyn FsOps, left: BorrowedFd<'_>, right: BorrowedFd<'_>) -> bool {
    match (fs.stat_dir(left), fs.stat_dir(right)) {
        (Ok(left), Ok(right)) => left.identity == right.identity,
        _ => false,
    }
}

/// The item's identity when `to` names `from` itself rather than another
/// item: a case alias, never a separate hard link.
fn alias_of(fs: &dyn FsOps, from: Entry<'_>, to: Entry<'_>) -> Option<LocalFileIdentity> {
    let source = fs.stat(from, true).ok()?;
    let target = fs.stat(to, true).ok()?;
    if source.identity != target.identity {
        return None;
    }
    let single_name = !source.is_directory() && source.nlink == 1;
    let alias = single_name
        || fs
            .names(at(to.dir, OsStr::new(".")))
            .is_ok_and(|names| !names.iter().any(|name| name == to.name));
    alias.then_some(source.identity)
}

/// The ladder, plus a last rung so renames still work on mounts that have
/// neither `RENAME_NOREPLACE` nor hard links. That rung runs only after a
/// probe on Strata's own names confirms both are missing.
struct Renaming<'a> {
    fs: &'a dyn FsOps,
    mount: Mount<'a>,
    probed: Cell<Option<Option<Probed>>>,
}

impl<'a> Renaming<'a> {
    fn new(fs: &'a dyn FsOps, dir: BorrowedFd<'a>) -> Self {
        Self {
            fs,
            mount: Mount::new(fs, dir),
            probed: Cell::new(None),
        }
    }

    fn place(
        &self,
        from: Entry<'_>,
        to: Entry<'_>,
        expected: Option<LocalFileIdentity>,
    ) -> Result<Placed, PlaceError> {
        match place(self.fs, from, to, expected) {
            Err(PlaceError::Unsupported) => {}
            result => return result,
        }
        let source = stat_source(self.fs, from)?;
        if expected.is_some_and(|expected| expected != source.identity) {
            return Err(PlaceError::Changed);
        }
        if !self.lacks_placement(source.is_directory()) {
            return Err(PlaceError::Unsupported);
        }
        if source.is_directory() {
            rename_folder_after_check(self.fs, from, to, &source)
        } else {
            rename_over_reserved_file(self.fs, from, to, &source)
        }
    }

    fn lacks_placement(&self, directory: bool) -> bool {
        let probed = match self.probed.get() {
            Some(probed) => probed,
            None => {
                let probed = self.mount.probe(true, false).ok();
                self.probed.set(Some(probed));
                probed
            }
        };
        probed.is_some_and(|probed| {
            probed.no_replace == Some(false) && (directory || probed.hard_links == Some(false))
        })
    }
}

fn rename_over_reserved_file(
    fs: &dyn FsOps,
    from: Entry<'_>,
    to: Entry<'_>,
    source: &EntryStat,
) -> Result<Placed, PlaceError> {
    let reserved = match fs.create_exclusive(to, Mode::from_raw_mode(0o600)) {
        Ok(file) => rustix::fs::fstat(&file)
            .map(|stat| LocalFileIdentity::from_stat(&stat))
            .map_err(PlaceError::Io)?,
        Err(Errno::PERM) if exists(fs, to)? => return Err(PlaceError::Exists),
        Err(error) => return Err(creation_error(error)),
    };
    let still_reserved = || {
        fs.stat(to, true).is_ok_and(|current| {
            current.identity == reserved
                && current.kind == FileType::RegularFile
                && current.size == 0
        })
    };
    if !still_reserved() {
        return Err(PlaceError::Exists);
    }
    let error = match fs.rename(from, to, RenameFlags::empty()) {
        Ok(()) => return Ok(Placed::OverReservedFile),
        Err(error) => error,
    };
    if may_have_lost_reply(error)
        && matches!(fs.stat(from, true), Err(Errno::NOENT))
        && fs
            .stat(to, true)
            .is_ok_and(|target| target.identity == source.identity)
    {
        return Ok(Placed::OverReservedFile);
    }
    if still_reserved() {
        let _ = fs.unlink(to, AtFlags::empty());
    }
    Err(match error {
        Errno::INVAL => PlaceError::InvalidName,
        Errno::XDEV => PlaceError::CrossDevice,
        error => PlaceError::Io(error),
    })
}

fn rename_folder_after_check(
    fs: &dyn FsOps,
    from: Entry<'_>,
    to: Entry<'_>,
    source: &EntryStat,
) -> Result<Placed, PlaceError> {
    if exists(fs, to)? {
        return Err(PlaceError::Exists);
    }
    match fs.rename(from, to, RenameFlags::empty()) {
        Ok(()) => Ok(Placed::AfterCheck),
        Err(Errno::EXIST | Errno::NOTEMPTY | Errno::NOTDIR | Errno::ISDIR) => {
            Err(PlaceError::Exists)
        }
        Err(Errno::PERM) if exists(fs, to)? => Err(PlaceError::Exists),
        Err(Errno::INVAL) => Err(if contains(fs, source.identity, to.dir) == Some(false) {
            PlaceError::InvalidName
        } else {
            PlaceError::IntoItself
        }),
        Err(Errno::XDEV) => Err(PlaceError::CrossDevice),
        Err(error) if may_have_lost_reply(error) => {
            resolve(fs, from, to, Some(source.identity), error).map(|()| Placed::AfterCheck)
        }
        Err(error) => Err(PlaceError::Io(error)),
    }
}
