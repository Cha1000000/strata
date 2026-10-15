// SPDX-License-Identifier: MIT

//! What the mount behind a folder says about itself: learned once per
//! mount from mountinfo, udev and `statfs`, and read once per operation.

#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    ffi::OsStr,
    os::fd::BorrowedFd,
    path::{Component, Path},
    sync::{Arc, Mutex, MutexGuard},
};

use super::fs_ops::{FsOps, opened_path};
use crate::adapters::volume::{
    Family, FuseFormat, MountCapabilities, MountKey, MountNames, MountType, NameProfile, family,
    gvfs_profile, holds_symlinks, learn_mount_capability, max_file_size, mount_capabilities,
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
        Some((mount_type, mount_point)) if mount_type.is_gvfs_bridge() => MountNames::PerBackend {
            depth: u16::try_from(folders(mount_point).count()).unwrap_or(u16::MAX),
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
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "transfers take their profiles from here next")
)]
pub(super) struct MountProfile {
    pub names: NameProfile,
    pub symlinks: Option<bool>,
    pub max_file_size: Option<u64>,
}

/// One operation's view of the mounts it writes into. Each mount is read
/// once, so the operation's planner and workers agree even when a mount
/// change drops what was learned of the mounts meanwhile.
#[derive(Clone, Default)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "transfers take their profiles from here next")
)]
pub(super) struct Profiles(Arc<Mutex<HashMap<MountKey, MountCapabilities>>>);

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "transfers take their profiles from here next")
)]
impl Profiles {
    pub(super) fn of(&self, fs: &dyn FsOps, dir: BorrowedFd<'_>) -> MountProfile {
        let key = fs.mount_key(dir);
        let seen = self.seen().get(&key).copied();
        let seen = seen.unwrap_or_else(|| {
            let capabilities = known(fs, dir, key);
            *self.seen().entry(key).or_insert(capabilities)
        });
        let names = match seen.names {
            Some(MountNames::Profile(names)) => names,
            Some(MountNames::PerBackend { depth }) => Self::backend(dir, depth),
            None => NameProfile::unknown(0),
        };
        MountProfile {
            names,
            symlinks: seen.symlinks,
            max_file_size: seen.max_file_size,
        }
    }

    fn seen(&self) -> MutexGuard<'_, HashMap<MountKey, MountCapabilities>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// The bridge names a backend's folder `<scheme>:<details>`, such as
    /// `mtp:host=…` or `smb-share:server=…,share=…`.
    fn backend(dir: BorrowedFd<'_>, depth: u16) -> NameProfile {
        let path = opened_path(dir).unwrap_or_default();
        let scheme = folders(&path).nth(usize::from(depth)).and_then(|folder| {
            let folder = folder.as_encoded_bytes();
            let colon = folder.iter().position(|byte| *byte == b':')?;
            std::str::from_utf8(&folder[..colon]).ok()
        });
        gvfs_profile(scheme.unwrap_or_default())
    }
}
