// SPDX-License-Identifier: MIT

#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    iter,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

use rustix::{
    fs::{FileType, Mode},
    io::Errno,
};

use super::super::{
    copy_engine::{refuses_name, refuses_node, taken},
    fs_ops::{Entry, fs},
    mount_profile::{FolderRules, NamePolicy, Site},
    naming::{
        fix::{Fixed, fix_name},
        numbered_name, stricter_names,
    },
    placement::{
        EXTRACTION_STAGE_PREFIX, NUMBERED_NAME_ATTEMPTS, PlaceError, creation_error, place,
        reserve_hidden_name,
    },
    placement_reason,
};
use super::{ArchiveError, extraction::MemberMetadata};
use crate::{
    adapters::volume::{NameLimit, Strictness},
    services::{NameMode, RenameCause, error_detail},
};

/// Reading procfs avoids a process-global umask(2) race in the multithreaded GUI.
pub(super) fn process_umask() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("Umask:")
                    .and_then(|value| u32::from_str_radix(value.trim(), 8).ok())
            })
        })
        .unwrap_or(0o022)
}

pub(super) fn permission_bits(mode: u32) -> u32 {
    mode & 0o777
}

fn timestamps(modified: SystemTime) -> Option<rustix::fs::Timestamps> {
    let since_epoch = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: i64::try_from(since_epoch.as_secs()).ok()?,
            tv_nsec: since_epoch.subsec_nanos().into(),
        },
    })
}

const FUSE_SUPER_MAGIC: rustix::fs::FsWord = 0x6573_5546;

/// FAT and some network/FUSE mounts cannot store Unix permissions or timestamps.
fn best_effort(result: rustix::io::Result<()>) -> rustix::io::Result<()> {
    match result {
        Err(rustix::io::Errno::PERM | rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::INVAL) => {
            Ok(())
        }
        result => result,
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct MetadataCalls {
    pub(super) chmod: fn(BorrowedFd<'_>, rustix::fs::Mode) -> rustix::io::Result<()>,
    pub(super) set_times: fn(BorrowedFd<'_>, &rustix::fs::Timestamps) -> rustix::io::Result<()>,
    pub(super) set_link_times:
        fn(BorrowedFd<'_>, &OsStr, &rustix::fs::Timestamps) -> rustix::io::Result<()>,
}

impl MetadataCalls {
    pub(super) const SYSTEM: Self = Self {
        chmod: |directory, mode| rustix::fs::fchmod(directory, mode),
        set_times: |file, times| rustix::fs::futimens(file, times),
        set_link_times: |parent, name, times| {
            rustix::fs::utimensat(parent, name, times, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        },
    };
}

/// Clamp parent traversal at the extraction root so one such member does not abort the archive.
pub(super) fn sanitized_archive_path(name: impl AsRef<OsStr>) -> Result<PathBuf, String> {
    let native = name.as_ref();
    let name = native.to_string_lossy();
    let normalized: Vec<_> = native
        .as_bytes()
        .iter()
        .map(|byte| if *byte == b'\\' { b'/' } else { *byte })
        .collect();
    if normalized.is_empty() || normalized.starts_with(b"/") {
        return Err(unsafe_archive_path(&name));
    }

    let mut path = PathBuf::new();
    for component in normalized.split(|byte| *byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                path.pop();
            }
            bytes if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' => {
                return Err(unsafe_archive_path(&name));
            }
            _ => path.push(OsStr::from_bytes(component)),
        }
    }
    if path.as_os_str().is_empty() {
        return Err(
            rust_i18n::t!("Refusing empty archive path: %{name}", name = name).into_owned(),
        );
    }
    Ok(path)
}

fn unsafe_archive_path(name: &str) -> String {
    rust_i18n::t!("Refusing unsafe archive path: %{name}", name = name).into_owned()
}

fn staging_creation_failed(error: &str) -> String {
    rust_i18n::t!(
        "Could not create the extraction staging folder: %{error}",
        error = error
    )
    .into_owned()
}

fn errno_text(error: rustix::io::Errno) -> String {
    crate::services::io_error_message(&error.into())
}

fn errno_detail(error: rustix::io::Errno) -> String {
    crate::services::io_error_detail(&error.into())
}

fn no_file_name() -> String {
    crate::i18n::tr("Archive entry has no file name")
}

fn staging_removal_failed(error: rustix::io::Errno) -> String {
    rust_i18n::t!(
        "Could not remove the extraction staging folder: %{error}",
        error = errno_detail(error)
    )
    .into_owned()
}

fn modification_time_failed(path: &Path, error: rustix::io::Errno) -> String {
    rust_i18n::t!(
        "Could not restore the modification time of “%{path}”: %{error}",
        path = path.display(),
        error = errno_detail(error)
    )
    .into_owned()
}

fn no_available_name(name: &OsStr) -> String {
    rust_i18n::t!(
        "Could not find an available extraction name for %{name}",
        name = name.to_string_lossy()
    )
    .into_owned()
}

/// Appends ` ({index})` to a directory name, or inserts it before a file's
/// extension: `pkg-1.2.3 (2)`, `readme (2).txt`.
fn suffixed_name(name: &OsStr, index: u64, is_directory: bool, limit: NameLimit) -> OsString {
    let path = Path::new(name);
    let stem = match path.extension() {
        Some(_) if !is_directory => path.file_stem().unwrap_or(name),
        _ => name,
    };
    let extension = OsStr::from_bytes(&name.as_bytes()[stem.len()..]);
    numbered_name(stem, index, extension, limit, is_directory)
}

pub(super) fn archive_stem(archive_name: &str) -> &str {
    let lower = archive_name.to_ascii_lowercase();
    let stem = [".tar.gz", ".tgz", ".tar", ".zip", ".7z", ".rar"]
        .iter()
        .find_map(|suffix| {
            lower
                .ends_with(suffix)
                .then(|| &archive_name[..archive_name.len() - suffix.len()])
        })
        .unwrap_or(archive_name);
    if stem.trim_matches('.').is_empty() {
        archive_name
    } else {
        stem
    }
}

/// Pinned roots and NOFOLLOW traversal prevent symlink swaps from redirecting writes.
pub(super) struct ExtractionDestination {
    root: OwnedFd,
    calls: MetadataCalls,
    names: MemberNames,
}

impl ExtractionDestination {
    /// Opens `path` as a directory. Ordinary symlinks along the path are
    /// resolved, since users browse symlinked folders, but the resolved
    /// directory is pinned so later replacement of any component cannot
    /// redirect writes. This mirrors how copy and compress open their parents.
    ///
    /// # Errors
    ///
    /// Returns an error if `path` is not absolute or cannot be opened as a directory.
    pub(super) fn open(path: &Path) -> Result<Self, String> {
        let relative = path
            .strip_prefix("/")
            .map_err(|_| crate::i18n::tr("Extraction destination must use an absolute path"))?;
        let filesystem_root = rustix::fs::open(
            c"/",
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| {
            rust_i18n::t!(
                "Could not open the filesystem root: %{error}",
                error = errno_detail(error)
            )
            .into_owned()
        })?;
        let relative = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        let root = rustix::fs::openat2(
            &filesystem_root,
            relative,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::IN_ROOT | rustix::fs::ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|error| {
            rust_i18n::t!(
                "Could not open extraction destination: %{error}",
                error = errno_detail(error)
            )
            .into_owned()
        })?;
        let names = MemberNames::of(root.as_fd());
        Ok(Self {
            root,
            calls: MetadataCalls::SYSTEM,
            names,
        })
    }

    #[cfg(test)]
    pub(super) fn with_metadata_calls(self, calls: MetadataCalls) -> Self {
        Self { calls, ..self }
    }

    pub(super) fn create_staging(&self) -> Result<(OsString, Self), String> {
        let (name, ()) = reserve_hidden_name(EXTRACTION_STAGE_PREFIX, |name| {
            fs().mkdir(
                Entry::new(&self.root, name),
                rustix::fs::Mode::from_raw_mode(0o777),
            )
            .map_err(creation_error)
        })
        .map_err(|error| staging_creation_failed(&error_detail(placement_reason(error))))?;
        let root = rustix::fs::openat(
            &self.root,
            &name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| {
            let _ = rustix::fs::unlinkat(&self.root, &name, rustix::fs::AtFlags::REMOVEDIR);
            rust_i18n::t!(
                "Could not open the extraction staging folder: %{error}",
                error = errno_detail(error)
            )
            .into_owned()
        })?;
        let staging = Self {
            root,
            calls: self.calls,
            names: self.names.clone(),
        };
        Ok((name, staging))
    }

    pub(super) fn names(&self) -> &MemberNames {
        &self.names
    }

    pub(super) fn remove_empty_staging(&self, staging: &OsStr) -> Result<bool, String> {
        match rustix::fs::unlinkat(&self.root, staging, rustix::fs::AtFlags::REMOVEDIR) {
            Ok(()) => Ok(true),
            // Some filesystems report a non-empty directory as EEXIST.
            Err(rustix::io::Errno::NOTEMPTY | rustix::io::Errno::EXIST) => Ok(false),
            Err(error) => Err(staging_removal_failed(error)),
        }
    }

    pub(super) fn remove_directory_only_staging(&self, staging: &OsStr) -> Result<bool, String> {
        remove_tree(self.root.as_fd(), staging, false).map_err(staging_removal_failed)
    }

    /// Never follows symlinks. Removal errors can leave a partially deleted tree.
    pub(super) fn remove_staging(&self, staging: &OsStr) -> Result<(), String> {
        remove_tree(self.root.as_fd(), staging, true)
            .map(|_| ())
            .map_err(staging_removal_failed)
    }

    /// The name of the folder an archive's entries are published in, before
    /// it is numbered: the archive's name without its extension, as the
    /// destination stores it.
    pub(super) fn folder_name<'a>(&self, archive_name: &'a str) -> Fixed<'a> {
        self.names
            .fixed(OsStr::new(archive_stem(archive_name)), true)
    }

    pub(super) fn publish_staging_as_folder(
        &self,
        staging: &OsStr,
        archive_name: &str,
    ) -> Result<String, String> {
        let stem = archive_stem(archive_name);
        if stem.contains('/') {
            return Err(
                rust_i18n::t!("Invalid extraction folder name “%{name}”", name = stem).into_owned(),
            );
        }
        let stem = self.folder_name(archive_name).name;
        for suffix in 0..NUMBERED_NAME_ATTEMPTS {
            let name = if suffix == 0 {
                stem.to_os_string()
            } else {
                numbered_name(&stem, suffix, OsStr::new(""), self.name_limit(), true)
            };
            match place_without_replacing(&self.root, staging, &self.root, &name) {
                Ok(()) => return Ok(name.to_string_lossy().into_owned()),
                Err(PlaceError::Exists) => {}
                Err(error) => {
                    return Err(rust_i18n::t!(
                        "Could not publish the extracted entries as “%{name}”: %{error}",
                        name = name.to_string_lossy(),
                        error = error_detail(placement_reason(error))
                    )
                    .into_owned());
                }
            }
        }
        Err(crate::i18n::tr("No available extraction folder name"))
    }

    pub(super) fn publish_single_root(
        &self,
        staging: &Self,
        root: &Path,
    ) -> Result<OsString, String> {
        let leaf = root.file_name().ok_or_else(no_file_name)?;
        let is_directory = fs()
            .stat(Entry::new(&staging.root, root.as_os_str()), false)
            .map_err(errno_text)?
            .is_directory();
        for index in 1..=NUMBERED_NAME_ATTEMPTS {
            let candidate = if index == 1 {
                leaf.to_owned()
            } else {
                suffixed_name(leaf, index, is_directory, self.name_limit())
            };
            match place_without_replacing(&staging.root, root.as_os_str(), &self.root, &candidate) {
                Ok(()) => return Ok(candidate),
                Err(PlaceError::Exists) => {}
                Err(error) => {
                    return Err(rust_i18n::t!(
                        "Could not publish “%{name}”: %{error}",
                        name = candidate.to_string_lossy(),
                        error = error_detail(placement_reason(error))
                    )
                    .into_owned());
                }
            }
        }
        Err(no_available_name(leaf))
    }

    /// Uses [`fstatvfs`] on the pinned root so a swapped path cannot redirect
    /// the query. A zero `f_blocks` means the filesystem does not report
    /// capacity, so callers skip the check instead of refusing every extraction.
    ///
    /// [`fstatvfs`]: rustix::fs::fstatvfs
    pub(super) fn available_bytes(&self) -> Result<Option<u64>, String> {
        let stat = rustix::fs::fstatvfs(&self.root).map_err(|error| {
            rust_i18n::t!(
                "Could not inspect free space at the extraction destination: %{error}",
                error = errno_detail(error)
            )
            .into_owned()
        })?;
        if stat.f_blocks == 0 {
            return Ok(None);
        }
        let block = if stat.f_frsize > 0 {
            stat.f_frsize
        } else {
            stat.f_bsize.max(1)
        };
        Ok(Some(stat.f_bavail.saturating_mul(block)))
    }

    fn name_limit(&self) -> NameLimit {
        self.names.rules().limit
    }

    /// Whether nothing is at `name` in `directory`, never following a link there.
    fn is_free(&self, directory: BorrowedFd<'_>, name: &OsStr) -> Result<bool, Unmade> {
        match fs().stat(
            Entry {
                dir: directory,
                name,
            },
            false,
        ) {
            Err(Errno::NOENT) => Ok(true),
            Err(error) => Err(Unmade::of(
                directory,
                error,
                rust_i18n::t!(
                    "Could not inspect extraction path %{name}: %{error}",
                    name = name.to_string_lossy(),
                    error = errno_detail(error)
                )
                .into_owned(),
            )),
            Ok(stat) => match stat.kind {
                FileType::RegularFile | FileType::Directory | FileType::Symlink => Ok(false),
                _ => Err(Unmade::Failed(
                    rust_i18n::t!(
                        "Refusing to extract over special filesystem object: %{name}",
                        name = name.to_string_lossy()
                    )
                    .into_owned(),
                )),
            },
        }
    }

    /// Makes the folder `name` in `directory`, or finds the name taken.
    fn make_folder(&self, directory: BorrowedFd<'_>, name: &OsStr) -> Result<bool, Unmade> {
        let entry = Entry {
            dir: directory,
            name,
        };
        match fs().mkdir(entry, Mode::from_raw_mode(0o777)) {
            Ok(()) => Ok(true),
            Err(error) if taken(fs(), entry, error) => Ok(false),
            Err(error) => Err(Unmade::of(directory, error, errno_text(error))),
        }
    }

    /// The folder at `path`, opened without following a symlink anywhere on the way.
    pub(super) fn open_directory(&self, path: &Path) -> Result<OwnedFd, String> {
        if path.as_os_str().is_empty() {
            return self
                .root
                .try_clone()
                .map_err(|error| crate::services::io_error_message(&error));
        }
        fs().open_dir(Entry::new(&self.root, path.as_os_str()))
            .map_err(errno_text)
    }

    pub(super) fn create_file(
        &self,
        placed: &Placed,
        mode: Option<u32>,
    ) -> Result<std::fs::File, Unmade> {
        fs().create_exclusive(
            placed.entry(),
            Mode::from_raw_mode(mode.map_or(0o666, permission_bits)),
        )
        .map(std::fs::File::from)
        .map_err(|error| Unmade::of(placed.parent.as_fd(), error, errno_text(error)))
    }

    /// Makes a symbolic link at `placed`.
    pub(super) fn create_symlink(
        &self,
        placed: &Placed,
        target: &OsStr,
        modified: Option<SystemTime>,
    ) -> Result<(), Unmade> {
        let entry = placed.entry();
        if let Err(error) = fs().symlink(target, entry) {
            if !taken(fs(), entry, error) && refuses_node(FileType::Symlink, error) {
                return Err(Unmade::Unheld(error));
            }
            return Err(Unmade::of(
                placed.parent.as_fd(),
                error,
                rust_i18n::t!(
                    "Could not create symbolic link “%{path}”: %{error}",
                    path = placed.path.display(),
                    error = errno_detail(error)
                )
                .into_owned(),
            ));
        }
        // A link's time is cosmetic, and FUSE daemons such as sshfs apply it to the target.
        if let Some(times) = modified.and_then(timestamps)
            && fs().statfs(placed.parent.as_fd()).map(|stat| stat.kind) != Some(FUSE_SUPER_MAGIC)
        {
            let _ = (self.calls.set_link_times)(placed.parent.as_fd(), &placed.name, &times);
        }
        Ok(())
    }

    pub(super) fn set_file_times(
        &self,
        file: &std::fs::File,
        modified: SystemTime,
    ) -> rustix::io::Result<()> {
        match timestamps(modified) {
            Some(times) => best_effort((self.calls.set_times)(file.as_fd(), &times)),
            None => Ok(()),
        }
    }

    /// Link symlinks themselves rather than following their targets outside staging.
    pub(super) fn create_hard_link(&self, placed: &Placed, target: &Path) -> Result<(), Unmade> {
        let unavailable = |error: String| {
            rust_i18n::t!(
                "Hard link “%{path}” refers to “%{target}”, which is not available in this extraction: %{error}",
                path = placed.path.display(),
                target = target.display(),
                error = error
            )
            .into_owned()
        };
        let target_name = target
            .file_name()
            .ok_or_else(|| unavailable(no_file_name()))?;
        let target_parent = self
            .open_directory(target.parent().unwrap_or_else(|| Path::new("")))
            .map_err(unavailable)?;
        fs().link(Entry::new(&target_parent, target_name), placed.entry())
            .map_err(|error| {
                Unmade::of(
                    placed.parent.as_fd(),
                    error,
                    rust_i18n::t!(
                        "Could not link “%{path}” to “%{target}”: %{error}",
                        path = placed.path.display(),
                        target = target.display(),
                        error = errno_detail(error)
                    )
                    .into_owned(),
                )
            })
    }

    pub(super) fn apply_directory_metadata(
        &self,
        path: &Path,
        metadata: MemberMetadata,
        umask: u32,
    ) -> Result<(), String> {
        let directory = self.open_directory(path).map_err(|error| {
            rust_i18n::t!(
                "Could not open “%{path}”: %{error}",
                path = path.display(),
                error = error
            )
            .into_owned()
        })?;
        if let Some(mode) = metadata.mode {
            let mode = rustix::fs::Mode::from_raw_mode(permission_bits(mode) & !umask);
            best_effort((self.calls.chmod)(directory.as_fd(), mode)).map_err(|error| {
                rust_i18n::t!(
                    "Could not restore permissions on “%{path}”: %{error}",
                    path = path.display(),
                    error = errno_detail(error)
                )
                .into_owned()
            })?;
        }
        if let Some(times) = metadata.modified.and_then(timestamps) {
            best_effort((self.calls.set_times)(directory.as_fd(), &times))
                .map_err(|error| modification_time_failed(path, error))?;
        }
        Ok(())
    }

    /// Keep failed output removable; the original metadata error is already reported.
    pub(super) fn reset_directory_mode(&self, path: &Path, umask: u32) {
        if let Ok(directory) = self.open_directory(path) {
            let mode = rustix::fs::Mode::from_raw_mode(0o777 & !umask);
            let _ = (self.calls.chmod)(directory.as_fd(), mode);
        }
    }

    /// Unlinks the leaf of `path` under the destination root.
    ///
    /// Used to discard a partially written member after cancellation or
    /// copy failure. Does not follow a final symbolic link.
    ///
    /// # Errors
    ///
    /// Returns an error if `path` has no file name, a parent cannot be opened,
    /// or the unlink fails.
    pub(super) fn remove_file(&self, path: &Path) -> Result<(), String> {
        let parent = self.open_directory(path.parent().unwrap_or_else(|| Path::new("")))?;
        let name = path.file_name().ok_or_else(no_file_name)?;
        fs().unlink(Entry::new(&parent, name), rustix::fs::AtFlags::empty())
            .map_err(|error| {
                rust_i18n::t!(
                    "Could not remove incomplete extraction %{path}: %{error}",
                    path = path.display(),
                    error = errno_detail(error)
                )
                .into_owned()
            })
    }
}

/// How members are named at a destination: by the rules of its mount, as
/// the extraction first saw it, with what its refusals taught it since.
#[derive(Clone)]
pub(super) struct MemberNames {
    policy: NamePolicy,
    site: Site,
}

impl MemberNames {
    fn of(destination: BorrowedFd<'_>) -> Self {
        let policy = NamePolicy::new(NameMode::Fix);
        let (key, seen) = policy.profiles.mount(fs(), destination);
        let site = policy.profiles.site(key, seen.names, destination);
        Self { policy, site }
    }

    fn rules(&self) -> FolderRules {
        self.policy.in_folder(&self.site)
    }

    fn fixed<'a>(&self, name: &'a OsStr, is_directory: bool) -> Fixed<'a> {
        fix_name(name, &self.rules().fixed, is_directory)
    }

    /// Whether the destination may hold symbolic links, as far as the
    /// extraction knows.
    pub(super) fn holds_links(&self) -> bool {
        self.policy.profiles.holds_links(&self.site)
    }

    /// Teaches the destination that it holds no symbolic links.
    pub(super) fn refuse_links(&self) {
        self.policy.profiles.refuse_links(&self.site);
    }

    /// Teaches a destination that learns that names also follow the
    /// Windows rules in `strictness`.
    fn learn(&self, strictness: Strictness) {
        if self.site.rules.learns {
            self.policy.profiles.learn(&self.site, strictness);
        }
    }
}

/// Why a member was not made.
pub(super) enum Unmade {
    /// The destination refused its name with the error.
    Refused(Errno, String),
    /// The destination cannot hold a symbolic link, as the error says.
    Unheld(Errno),
    Failed(String),
}

impl Unmade {
    /// A call that makes something in `directory` failed with `error`.
    fn of(directory: BorrowedFd<'_>, error: Errno, message: String) -> Self {
        if refuses_name(fs(), directory, error) {
            Self::Refused(error, message)
        } else {
            Self::Failed(message)
        }
    }
}

impl From<String> for Unmade {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<Unmade> for String {
    fn from(unmade: Unmade) -> Self {
        match unmade {
            Unmade::Refused(_, message) | Unmade::Failed(message) => message,
            Unmade::Unheld(error) => errno_text(error),
        }
    }
}

impl From<Unmade> for ArchiveError {
    fn from(unmade: Unmade) -> Self {
        Self::Failed(unmade.into())
    }
}

fn place_without_replacing(
    from_directory: &OwnedFd,
    from: &OsStr,
    to_directory: &OwnedFd,
    to: &OsStr,
) -> Result<(), PlaceError> {
    place(
        fs(),
        Entry::new(from_directory, from),
        Entry::new(to_directory, to),
        None,
    )
    .map(drop)
}

/// Never follows symlinks. With `files` disabled, any non-directory preserves its ancestors.
fn remove_tree(parent: BorrowedFd<'_>, name: &OsStr, files: bool) -> rustix::io::Result<bool> {
    let directory = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let mut children = Vec::new();
    for entry in rustix::fs::Dir::read_from(&directory)? {
        let entry = entry?;
        let child = OsStr::from_bytes(entry.file_name().to_bytes());
        if child == "." || child == ".." {
            continue;
        }
        let file_type = match entry.file_type() {
            rustix::fs::FileType::Unknown => rustix::fs::FileType::from_raw_mode(
                rustix::fs::statat(&directory, child, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?
                    .st_mode,
            ),
            file_type => file_type,
        };
        let is_directory = file_type == rustix::fs::FileType::Directory;
        if !is_directory && !files {
            return Ok(false);
        }
        children.push((child.to_os_string(), is_directory));
    }
    for (child, is_directory) in children {
        if !is_directory {
            rustix::fs::unlinkat(&directory, &child, rustix::fs::AtFlags::empty())?;
        } else if !remove_tree(directory.as_fd(), &child, files)? {
            return Ok(false);
        }
    }
    rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::REMOVEDIR)?;
    Ok(true)
}

/// Where members go in the staging folder. Each name is fixed by the
/// destination's rules, each folder is made once for the member path that
/// names it, and a name another member took, or that the destination finds
/// taken, is numbered past, so distinct folders are never merged. A name the
/// destination refuses is fixed by stricter rules, as copies do.
pub(super) struct ExtractNameResolver {
    names: MemberNames,
    /// Per member path of a folder, where the folder is in the staging
    /// folder, and its index.
    folders: HashMap<PathBuf, (PathBuf, usize)>,
    /// Per folder index and name, the member name that took it. Names the
    /// destination takes for one another are found taken there.
    taken: HashMap<(usize, OsString), OsString>,
    /// Members saved under another name, and why, by staging path.
    renamed: Vec<(PathBuf, PathBuf, RenameCause)>,
}

/// The name a member takes in a folder of the staging folder.
pub(super) struct Placed {
    pub(super) parent: Rc<OwnedFd>,
    pub(super) name: OsString,
    /// Where the member is in the staging folder.
    pub(super) path: PathBuf,
    /// Where it would be under its own name.
    pub(super) requested: PathBuf,
    folder: usize,
    member: OsString,
    cause: Option<RenameCause>,
}

impl Placed {
    fn entry(&self) -> Entry<'_> {
        Entry::new(&*self.parent, &self.name)
    }
}

/// A name claimed for a member in a folder.
struct Claim<T> {
    name: OsString,
    cause: Option<RenameCause>,
    /// What was made there; `None` where the member path named something
    /// else the extraction made.
    made: Option<T>,
}

impl ExtractNameResolver {
    pub(super) fn new(destination: &ExtractionDestination) -> Self {
        Self {
            names: destination.names.clone(),
            folders: HashMap::from([(PathBuf::new(), (PathBuf::new(), 0))]),
            taken: HashMap::new(),
            renamed: Vec::new(),
        }
    }

    /// The folder at the member path `path`, and the folders holding it,
    /// made in `staging` where the extraction has not made them yet;
    /// returns where it is in the staging folder.
    pub(super) fn folder(
        &mut self,
        staging: &ExtractionDestination,
        path: &Path,
    ) -> Result<PathBuf, Unmade> {
        self.made_folder(staging, path).map(|(made, _)| made)
    }

    fn made_folder(
        &mut self,
        staging: &ExtractionDestination,
        path: &Path,
    ) -> Result<(PathBuf, usize), Unmade> {
        let mut known = path;
        let mut missing = Vec::new();
        while !self.folders.contains_key(known) {
            missing.push(known);
            known = known.parent().unwrap_or_else(|| Path::new(""));
        }
        let (mut parent, mut index) = self.folders[known].clone();
        for member in missing.into_iter().rev() {
            let name = member.file_name().ok_or_else(no_file_name)?;
            let directory = staging.open_directory(&parent)?;
            let claim = self.claim(index, name, true, |candidate, _| {
                Ok(staging
                    .make_folder(directory.as_fd(), candidate)?
                    .then_some(()))
            })?;
            let made = parent.join(&claim.name);
            if claim.made.is_none() {
                // A member under a link's or a file's name fails there.
                staging.open_directory(&made)?;
            } else {
                self.taken
                    .insert((index, claim.name.clone()), name.to_os_string());
                if let Some(cause) = claim.cause {
                    self.renamed.push((parent.join(name), made.clone(), cause));
                }
            }
            index = self.folders.len();
            self.folders
                .insert(member.to_path_buf(), (made.clone(), index));
            parent = made;
        }
        Ok((parent, index))
    }

    /// Makes the member `path`, which is not a folder, with `make` under a
    /// free name in its folder, which is made where it is new. The name is
    /// the member's once [`Self::made`] says it was made there.
    pub(super) fn leaf<T>(
        &mut self,
        staging: &ExtractionDestination,
        path: &Path,
        mut make: impl FnMut(&Placed) -> Result<T, Unmade>,
    ) -> Result<(Placed, T), Unmade> {
        let member = path.file_name().ok_or_else(no_file_name)?;
        let (folder_path, folder) =
            self.made_folder(staging, path.parent().unwrap_or_else(|| Path::new("")))?;
        let parent = Rc::new(staging.open_directory(&folder_path)?);
        let claim = self.claim(folder, member, false, |candidate, cause| {
            if !staging.is_free(parent.as_fd(), candidate)? {
                return Ok(None);
            }
            let placed = Placed {
                parent: Rc::clone(&parent),
                name: candidate.to_os_string(),
                path: folder_path.join(candidate),
                requested: folder_path.join(member),
                folder,
                member: member.to_os_string(),
                cause,
            };
            make(&placed).map(|made| Some((placed, made)))
        })?;
        claim.made.ok_or_else(|| no_available_name(member).into())
    }

    /// The member `placed` was made there; returns where it is in the
    /// staging folder.
    pub(super) fn made(&mut self, placed: Placed) -> PathBuf {
        if let Some(cause) = placed.cause {
            self.renamed
                .push((placed.requested, placed.path.clone(), cause));
        }
        self.taken
            .insert((placed.folder, placed.name), placed.member);
        placed.path
    }

    /// Where the member `path` would be under its own name, in its folder,
    /// which is made where it is new.
    pub(super) fn requested(
        &mut self,
        staging: &ExtractionDestination,
        path: &Path,
    ) -> Result<PathBuf, Unmade> {
        let member = path.file_name().ok_or_else(no_file_name)?;
        let folder = self.folder(staging, path.parent().unwrap_or_else(|| Path::new("")))?;
        Ok(folder.join(member))
    }

    /// Where the member `path` would go, as far as the folders made so far
    /// tell, without making anything or numbering a name.
    pub(super) fn known(&self, path: &Path) -> PathBuf {
        let mut member = PathBuf::new();
        let mut known = PathBuf::new();
        let mut names = path.iter().peekable();
        while let Some(name) = names.next() {
            member.push(name);
            match self.folders.get(&member) {
                Some((made, _)) => known.clone_from(made),
                None => known.push(self.names.fixed(name, names.peek().is_some()).name),
            }
        }
        known
    }

    /// What was saved under another name, and why, by staging path.
    pub(super) fn take_renamed(&mut self) -> Vec<(PathBuf, PathBuf, RenameCause)> {
        std::mem::take(&mut self.renamed)
    }

    /// The first name for the member `name` in the folder `folder` that
    /// `make`, told why it is not `name`, makes: the name the destination's
    /// rules give, then where the destination refuses it, those stricter
    /// rules give in turn, which a destination that learns keeps.
    fn claim<T>(
        &self,
        folder: usize,
        name: &OsStr,
        is_directory: bool,
        mut make: impl FnMut(&OsStr, Option<RenameCause>) -> Result<Option<T>, Unmade>,
    ) -> Result<Claim<T>, Unmade> {
        let rules = self.names.rules();
        let fixed = fix_name(name, &rules.fixed, is_directory);
        let stricter = || {
            stricter_names(&fixed.name, &rules.fixed, is_directory)
                .into_iter()
                .map(|(strictness, stricter, cause)| {
                    (Some(strictness), stricter, fixed.cause.or(Some(cause)))
                })
        };
        let names = iter::once((None, fixed.name.to_os_string(), fixed.cause))
            .chain(iter::once_with(stricter).flatten());
        let mut refused = None;
        for (strictness, base, cause) in names {
            match self.numbered(
                folder,
                name,
                (&base, cause),
                is_directory,
                rules.limit,
                &mut make,
            ) {
                Ok(claim) => {
                    if let Some(strictness) = strictness.filter(|_| claim.made.is_some()) {
                        self.names.learn(strictness);
                    }
                    return Ok(claim);
                }
                Err(refusal @ Unmade::Refused(..)) => {
                    refused.get_or_insert(refusal);
                }
                Err(unmade) => return Err(unmade),
            }
        }
        Err(refused.unwrap_or_else(|| no_available_name(name).into()))
    }

    /// The first of `base`, a name for the member `name` that is not `name`
    /// for `cause`, and its numbered names, that no other member took in the
    /// folder `folder` and that `make` makes.
    fn numbered<T>(
        &self,
        folder: usize,
        name: &OsStr,
        (base, cause): (&OsStr, Option<RenameCause>),
        is_directory: bool,
        limit: NameLimit,
        make: &mut impl FnMut(&OsStr, Option<RenameCause>) -> Result<Option<T>, Unmade>,
    ) -> Result<Claim<T>, Unmade> {
        let mut taken_by = None;
        for index in 1..=NUMBERED_NAME_ATTEMPTS {
            let candidate = if index == 1 {
                base.to_os_string()
            } else {
                suffixed_name(base, index, is_directory, limit)
            };
            match self.taken.get(&(folder, candidate.clone())) {
                Some(owner) if is_directory && owner == name => {
                    return Ok(Claim {
                        name: candidate,
                        cause: None,
                        made: None,
                    });
                }
                Some(owner) => {
                    taken_by.get_or_insert(owner.as_os_str());
                    continue;
                }
                None => {}
            }
            let cause = rename_cause(name, cause, &candidate, taken_by);
            if let Some(made) = make(&candidate, cause)? {
                return Ok(Claim {
                    name: candidate,
                    cause,
                    made: Some(made),
                });
            }
        }
        Err(no_available_name(name).into())
    }
}

/// Why a member named `name` was saved as `saved`: the fix `fixed` gave
/// its name, that another member, `taken_by`, took the name first, unless
/// that member had the same name, or that the destination took it for
/// another.
fn rename_cause(
    name: &OsStr,
    fixed: Option<RenameCause>,
    saved: &OsStr,
    taken_by: Option<&OsStr>,
) -> Option<RenameCause> {
    if saved == name {
        return None;
    }
    if fixed.is_some() {
        return fixed;
    }
    match taken_by {
        Some(owner) if owner == name => None,
        Some(_) => Some(RenameCause::Taken),
        None => Some(RenameCause::Alias),
    }
}
