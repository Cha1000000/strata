// SPDX-License-Identifier: MIT

//! What the mount behind a folder says about itself: learned once per
//! mount from mountinfo, udev and `statfs`, and read once per operation.

#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    ffi::OsStr,
    os::fd::{AsFd, BorrowedFd},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use gtk::{gio, glib, prelude::*};

use super::{
    await_cancellable,
    fs_ops::{FsOps, fs, opened_path},
    io_error, open_local_parent_directory, os_error_detail, run_blocking,
};
use crate::{
    adapters::volume::{
        Family, FuseFormat, MountCapabilities, MountKey, MountNames, MountType, NameLimit,
        NameProfile, NameRules, family, gvfs_bridge_profile, gvfs_profile, holds_symlinks,
        learn_mount_capability, max_file_size, mount_capabilities,
    },
    services::NameMode,
};

/// The mount holding `dir`, with what its type tells learned on first sight.
pub(super) fn known_mount(fs: &dyn FsOps, dir: BorrowedFd<'_>) -> (MountKey, MountCapabilities) {
    let key = fs.mount_key(dir);
    (key, known(fs, dir, key))
}

fn known(fs: &dyn FsOps, dir: BorrowedFd<'_>, key: MountKey) -> MountCapabilities {
    let capabilities = mount_capabilities(key);
    if capabilities.family.is_some() {
        return capabilities;
    }
    learn_mount_type(fs, dir, key)
}

/// For `fuseblk`, udev's record of the device tells which daemon serves it.
fn learn_mount_type(fs: &dyn FsOps, dir: BorrowedFd<'_>, key: MountKey) -> MountCapabilities {
    let mount = fs.mount_type(dir);
    let mount_type = mount.as_ref().map(|(mount_type, _)| mount_type);
    let family = mount_type.map_or(Family::Other, |mount| family(&mount.fs_type));
    let stable = mount_type.is_none_or(MountType::stable_inodes);
    let device_format = mount_type
        .filter(|mount| mount.fs_type == "fuseblk")
        .and_then(|_| fs.device_format(dir).ok().flatten());
    let format = device_format.as_deref();
    let fuse_format = format.and_then(FuseFormat::of);
    let coarse_times = mount_type.and_then(|mount| mount.coarse_times(fuse_format));
    let deferred_close = mount_type.and_then(MountType::deferred_close);
    let names = match &mount {
        Some((mount_type, site)) if mount_type.is_gvfs_bridge() => match folders(&site.root).next()
        {
            // A bind mount of one backend's folder, or of a folder inside it.
            Some(backend) => MountNames::Profile(gvfs_bridge_profile(Some(backend))),
            None => MountNames::PerBackend {
                depth: u16::try_from(folders(&site.point).count()).unwrap_or(u16::MAX),
            },
        },
        _ => {
            let name_max = fs.statfs(dir).map_or(0, |stat| stat.name_max);
            MountNames::Profile(mount_type.map_or_else(
                || NameProfile::unknown(name_max),
                |mount| mount.name_profile(format, name_max),
            ))
        }
    };
    let symlinks = mount_type.and_then(|mount| holds_symlinks(&mount.fs_type, format));
    let max_file_size = mount_type.and_then(|mount| max_file_size(&mount.fs_type, format));
    let mut learned = MountCapabilities::default();
    learn_mount_capability(key, |capabilities| {
        capabilities.family = Some(family);
        capabilities.stable_inodes = Some(stable);
        capabilities.fuse_format = fuse_format;
        capabilities.coarse_times = coarse_times;
        capabilities.deferred_close = deferred_close;
        capabilities.names = Some(names);
        capabilities.symlinks = symlinks;
        capabilities.max_file_size = max_file_size;
        learned = *capabilities;
    });
    learned
}

fn folders(path: &Path) -> impl Iterator<Item = &OsStr> {
    path.components().filter_map(|component| match component {
        Component::Normal(folder) => Some(folder),
        _ => None,
    })
}

/// What an operation needs to know of a mount it writes into.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MountProfile {
    pub names: NameProfile,
    pub symlinks: Option<bool>,
    pub max_file_size: Option<u64>,
    pub family: Family,
}

impl MountProfile {
    /// Of a mount as `seen`, inside its folder `dir` where that is open.
    fn of(seen: MountCapabilities, dir: Option<BorrowedFd<'_>>) -> Self {
        Self {
            names: dir.map_or_else(|| NameProfile::unknown(0), |dir| names_in(seen.names, dir)),
            symlinks: seen.symlinks,
            max_file_size: seen.max_file_size,
            family: seen.family.unwrap_or(Family::Other),
        }
    }

    /// A GVfs location, known by its scheme alone.
    pub(super) fn gvfs(scheme: &str) -> Self {
        Self {
            names: gvfs_profile(scheme),
            symlinks: None,
            max_file_size: None,
            family: Family::Other,
        }
    }

    /// Whether the mount is a disk in a format Windows writes, as sticks and
    /// memory cards are.
    pub(super) fn windows_disk(&self) -> bool {
        self.names.rules == NameRules::Windows && self.family != Family::Smb
    }
}

/// One operation's view of the mounts it writes into. Each mount is read
/// once, so the operation's planner and workers agree even when a mount
/// change drops what was learned of the mounts meanwhile.
#[derive(Clone, Default)]
pub(super) struct Profiles(Arc<Mutex<HashMap<MountKey, MountCapabilities>>>);

impl Profiles {
    /// The mount holding `dir`, as this operation first saw it.
    pub(super) fn mount(
        &self,
        fs: &dyn FsOps,
        dir: BorrowedFd<'_>,
    ) -> (MountKey, MountCapabilities) {
        let key = fs.mount_key(dir);
        let seen = self.seen().get(&key).copied();
        let seen = seen.unwrap_or_else(|| {
            let capabilities = known(fs, dir, key);
            *self.seen().entry(key).or_insert(capabilities)
        });
        (key, seen)
    }

    #[cfg(test)]
    pub(super) fn of(&self, fs: &dyn FsOps, dir: BorrowedFd<'_>) -> MountProfile {
        MountProfile::of(self.mount(fs, dir).1, Some(dir))
    }

    fn seen(&self) -> MutexGuard<'_, HashMap<MountKey, MountCapabilities>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// How one transfer names what it brings onto another filesystem, with the
/// mounts it writes into as it first saw them.
#[derive(Clone)]
pub(super) struct NamePolicy {
    pub profiles: Profiles,
    pub mode: NameMode,
}

impl Default for NamePolicy {
    fn default() -> Self {
        Self::new(NameMode::Keep)
    }
}

impl NamePolicy {
    pub(super) fn new(mode: NameMode) -> Self {
        Self {
            profiles: Profiles::default(),
            mode,
        }
    }

    /// The rules of a mount whose names are `rules` that names brought onto
    /// it follow. Names given back keep everything but what no name the mount
    /// holds can have.
    pub(super) fn applied(&self, rules: NameProfile) -> NameProfile {
        match self.mode {
            NameMode::Fix => rules,
            NameMode::Keep => rules.encoding_rules(),
        }
    }

    /// `destination`, a folder `sources` are pasted into, through any
    /// symbolic links to the mount that holds it.
    pub(super) async fn destination(
        &self,
        destination: &gio::File,
        sources: &[gio::File],
        cancellable: &gio::Cancellable,
    ) -> PasteDestination {
        let folders = sources
            .iter()
            .map(|source| {
                source
                    .parent()
                    .filter(|_| source.is_native())
                    .and_then(|parent| parent.path())
            })
            .collect::<Vec<_>>();
        let Some(path) = destination.path().filter(|_| destination.is_native()) else {
            let scheme = destination.uri_scheme().unwrap_or_default();
            return PasteDestination {
                profile: MountProfile::gvfs(&scheme),
                crossing: folders
                    .iter()
                    .map(|folder| folder.as_ref().map(|_| true))
                    .collect(),
                filesystem: filesystem_id(destination, cancellable).await.ok().flatten(),
            };
        };
        let profiles = self.profiles.clone();
        let unknown = folders.len();
        gio::spawn_blocking(move || native_destination(&profiles, &path, &folders))
            .await
            .unwrap_or_else(|_| PasteDestination {
                profile: MountProfile::of(MountCapabilities::default(), None),
                crossing: vec![Some(true); unknown],
                filesystem: None,
            })
    }

    /// How names are given to what is copied from the folder `source` into
    /// the folder `target`.
    pub(super) async fn folder(
        &self,
        source: &gio::File,
        target: &gio::File,
        cancellable: &gio::Cancellable,
    ) -> Result<FolderNames, glib::Error> {
        let Some(target_path) = target.path().filter(|_| target.is_native()) else {
            let rules = gvfs_profile(target.uri_scheme().as_deref().unwrap_or_default());
            let same = !source.is_native()
                && match filesystem_id(source, cancellable).await? {
                    Some(id) => filesystem_id(target, cancellable).await?.as_ref() == Some(&id),
                    None => false,
                };
            return Ok(FolderNames {
                rules: self.applied(rules),
                fixing: !same,
            });
        };
        let source_path = source.path().filter(|_| source.is_native());
        let profiles = self.profiles.clone();
        let (rules, crossing) = run_blocking(move || {
            let fs = fs();
            let target = open_local_parent_directory(&target_path).map_err(io_error)?;
            let (key, seen) = profiles.mount(fs, target.as_fd());
            let rules = names_in(seen.names, target.as_fd());
            let crossing = match source_path {
                Some(source) => {
                    let source = open_local_parent_directory(&source).map_err(io_error)?;
                    let device = |dir| {
                        fs.stat_fd(dir)
                            .map(|stat| stat.identity.device)
                            .map_err(|error| io_error(os_error_detail(error)))
                    };
                    let source = (device(source.as_fd())?, fs.mount_key(source.as_fd()));
                    crosses(source, (device(target.as_fd())?, key), seen.names)
                }
                None => true,
            };
            Ok((rules, crossing))
        })
        .await?;
        Ok(FolderNames {
            rules: self.applied(rules),
            fixing: crossing,
        })
    }
}

/// What a paste knows of its destination before it plans the names of its
/// items there.
pub(super) struct PasteDestination {
    pub profile: MountProfile,
    /// Per item, whether its name comes from another filesystem, where GIO's
    /// ids of the filesystems need not decide.
    crossing: Vec<Option<bool>>,
    /// GIO's id of the filesystem of a destination that is not native.
    filesystem: Option<glib::GString>,
}

impl PasteDestination {
    /// Whether each item's name is fixed, given GIO's id of the filesystem
    /// of each.
    pub(super) fn fixing(&self, filesystems: &[Option<glib::GString>]) -> Vec<bool> {
        self.crossing
            .iter()
            .zip(filesystems)
            .map(|(crossing, filesystem)| {
                crossing.unwrap_or_else(|| filesystem.is_none() || *filesystem != self.filesystem)
            })
            .collect()
    }
}

/// [`NamePolicy::destination`] of a native folder, with the native folders
/// of the items pasted there.
fn native_destination(
    profiles: &Profiles,
    path: &Path,
    folders: &[Option<PathBuf>],
) -> PasteDestination {
    let fs = fs();
    let device = |dir: BorrowedFd<'_>| fs.stat_fd(dir).ok().map(|stat| stat.identity.device);
    let destination = open_local_parent_directory(path).ok().and_then(|dir| {
        let (key, seen) = profiles.mount(fs, dir.as_fd());
        let device = device(dir.as_fd())?;
        Some((
            MountProfile::of(seen, Some(dir.as_fd())),
            (device, key),
            seen.names,
        ))
    });
    let Some((profile, target, names)) = destination else {
        return PasteDestination {
            profile: MountProfile::of(MountCapabilities::default(), None),
            crossing: vec![Some(true); folders.len()],
            filesystem: None,
        };
    };
    let mut seen = HashMap::<&Path, bool>::new();
    let crossing = folders
        .iter()
        .map(|folder| {
            let Some(folder) = folder else {
                return Some(true);
            };
            let crossing = *seen.entry(folder).or_insert_with(|| {
                open_local_parent_directory(folder)
                    .ok()
                    .and_then(|dir| Some((device(dir.as_fd())?, fs.mount_key(dir.as_fd()))))
                    .is_none_or(|source| crosses(source, target, names))
            });
            Some(crossing)
        })
        .collect();
    PasteDestination {
        profile,
        crossing,
        filesystem: None,
    }
}

/// How names are given to what a transfer copies into one folder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FolderNames {
    pub rules: NameProfile,
    /// Whether the names come from another filesystem, and so are fixed.
    pub fixing: bool,
}

/// Whether names listed in a folder on `source`, a device and the mount it
/// was reached through, are brought onto another filesystem in a folder on
/// `target`, whose mount has `names`. The GVfs bridge serves every backend
/// from one device, so names going onto it always are.
pub(super) fn crosses(
    source: (u64, MountKey),
    target: (u64, MountKey),
    names: Option<MountNames>,
) -> bool {
    source != target || matches!(names, Some(MountNames::PerBackend { .. }))
}

/// GIO's id of the filesystem holding `file`, as Nautilus compares them.
pub(super) async fn filesystem_id(
    file: &gio::File,
    cancellable: &gio::Cancellable,
) -> Result<Option<glib::GString>, glib::Error> {
    let info = await_cancellable(file, cancellable, |file, cancellable, result| {
        file.query_info_async(
            gio::FILE_ATTRIBUTE_ID_FILESYSTEM,
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
            Some(cancellable),
            move |output| result.resolve(output),
        );
    })
    .await?;
    Ok(info.attribute_string(gio::FILE_ATTRIBUTE_ID_FILESYSTEM))
}

/// The name rules inside `dir`, on a mount whose rules are `names`.
pub(super) fn names_in(names: Option<MountNames>, dir: BorrowedFd<'_>) -> NameProfile {
    match names {
        Some(MountNames::Profile(names)) => names,
        Some(MountNames::PerBackend { depth }) => backend(dir, depth),
        None => NameProfile::unknown(0),
    }
}

/// The bridge names a backend's folder `<scheme>:<details>`, such as
/// `mtp:host=…` or `smb-share:server=…,share=…`.
fn backend(dir: BorrowedFd<'_>, depth: u16) -> NameProfile {
    let path = opened_path(dir).ok();
    gvfs_bridge_profile(
        path.as_deref()
            .and_then(|path| folders(path).nth(usize::from(depth))),
    )
}

/// How long a name may be in `dir`, for numbering a name taken there.
pub(super) fn name_limit(fs: &dyn FsOps, dir: BorrowedFd<'_>) -> NameLimit {
    names_in(known_mount(fs, dir).1.names, dir).limit
}

/// [`name_limit`] of `folder`, read off the main thread where it is native,
/// and from its scheme elsewhere.
pub(super) async fn folder_name_limit(folder: &gio::File) -> NameLimit {
    let Some(path) = folder.path().filter(|_| folder.is_native()) else {
        return gvfs_profile(folder.uri_scheme().as_deref().unwrap_or_default()).limit;
    };
    gio::spawn_blocking(move || {
        open_local_parent_directory(&path).map(|dir| name_limit(fs(), dir.as_fd()))
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default()
}
