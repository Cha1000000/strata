// SPDX-License-Identifier: MIT

#[cfg(test)]
mod tests;

use std::{
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gtk::gio;

/// Parsed without touching mounted filesystems, including unresponsive mounts.
#[derive(Clone)]
pub(crate) struct MountTable {
    entries: Arc<[MountEntry]>,
}

struct MountEntry {
    id: u64,
    device: (u32, u32),
    mount_point: PathBuf,
    mount_type: MountType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MountType {
    pub fs_type: String,
    pub super_options: String,
}

impl MountType {
    /// Whether an entry keeps its inode number: FAT and exFAT, libfuse
    /// without `use_ino` and cifs without server inode numbers number an
    /// entry afresh once the kernel has evicted it.
    pub(crate) fn stable_inodes(&self) -> bool {
        match family(&self.fs_type) {
            Family::Fat | Family::Fuse => false,
            Family::Smb => !self
                .super_options
                .split(',')
                .any(|option| option == "noserverino"),
            _ => true,
        }
    }

    /// How long the SMB client keeps a closed file's handle open
    /// (`closetimeo`, one second by default).
    pub(crate) fn deferred_close(&self) -> Option<Duration> {
        if family(&self.fs_type) != Family::Smb {
            return None;
        }
        let seconds = self
            .super_options
            .split(',')
            .find_map(|option| option.strip_prefix("closetimeo="))
            .and_then(|seconds| seconds.parse().ok())
            .unwrap_or(1);
        Some(Duration::from_secs(seconds))
    }

    /// A `fuseblk` mount whose device udev cannot tell may be exFAT.
    pub(crate) fn coarse_times(&self, fuse_format: Option<FuseFormat>) -> Option<CoarseTimes> {
        let here = |seconds| CoarseTimes {
            window: Duration::from_secs(seconds),
            stamped_here: true,
        };
        match (family(&self.fs_type), self.fs_type.as_str(), fuse_format) {
            (Family::Fat, ..) | (_, "fuseblk", Some(FuseFormat::Exfat) | None) => Some(here(2)),
            (_, "hfs" | "hfsplus", _) => Some(here(1)),
            // A one-second tick, and two seconds for the server's clock
            // running behind ours. A server further behind can still hide
            // a rewrite.
            (_, "fuse.sshfs", _) => Some(CoarseTimes {
                window: Duration::from_secs(3),
                stamped_here: false,
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CoarseTimes {
    pub window: Duration,
    /// Whether this machine's clock stamps them, as on a local disk,
    /// rather than a server's.
    pub stamped_here: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FuseFormat {
    Ntfs,
    Exfat,
}

impl FuseFormat {
    pub(crate) fn of(id_fs_type: &str) -> Option<Self> {
        match id_fs_type {
            "ntfs" => Some(Self::Ntfs),
            "exfat" => Some(Self::Exfat),
            _ => None,
        }
    }
}

/// Filesystems by their mountinfo type, which tells NFS versions and FUSE
/// apart where the `statfs` magic cannot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Family {
    Ext,
    Xfs,
    Btrfs,
    F2fs,
    Tmpfs,
    Bcachefs,
    Zfs,
    Overlay,
    /// vfat, msdos and the kernel's exFAT.
    Fat,
    Ntfs3,
    Nfs,
    Nfs4,
    Smb,
    Fuse,
    Other,
}

pub(crate) fn family(fs_type: &str) -> Family {
    match fs_type {
        "ext2" | "ext3" | "ext4" => Family::Ext,
        "xfs" => Family::Xfs,
        "btrfs" => Family::Btrfs,
        "f2fs" => Family::F2fs,
        "tmpfs" => Family::Tmpfs,
        "bcachefs" => Family::Bcachefs,
        "zfs" => Family::Zfs,
        "overlay" => Family::Overlay,
        "vfat" | "msdos" | "exfat" => Family::Fat,
        "ntfs3" => Family::Ntfs3,
        "nfs" => Family::Nfs,
        "nfs4" => Family::Nfs4,
        "cifs" | "smb3" => Family::Smb,
        "fuse" | "fuseblk" => Family::Fuse,
        fs_type if fs_type.starts_with("fuse.") => Family::Fuse,
        _ => Family::Other,
    }
}

/// One mount instance. A remount gets a new kernel mount ID; the device and
/// mount point guard against a reused ID matching a stale key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MountKey {
    pub(crate) mount_id: u64,
    pub(crate) device: (u32, u32),
    pub(crate) mount_point: PathBuf,
    pub(crate) fs_type: String,
}

#[cfg(test)]
static MOUNTINFO_FOR_TEST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Replaces `/proc/self/mountinfo` for the whole process, so only a test that
/// runs in its own process may use it.
#[cfg(test)]
pub(crate) fn set_mountinfo_for_isolated_test(mountinfo: Option<String>) {
    assert!(
        std::env::var_os("STRATA_ISOLATED_GTK_TEST").is_some(),
        "the mount table override needs an isolated test process"
    );
    *MOUNTINFO_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = mountinfo;
}

impl MountTable {
    pub(crate) fn current() -> Self {
        #[cfg(test)]
        if let Some(mountinfo) = MOUNTINFO_FOR_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Self::parse(mountinfo);
        }
        std::fs::read("/proc/self/mountinfo")
            .map(Self::parse)
            .unwrap_or_else(|_| Self::parse(b""))
    }

    pub(crate) async fn load() -> Self {
        gio::spawn_blocking(Self::current)
            .await
            .unwrap_or_else(|_| Self::parse(b""))
    }

    pub(crate) fn parse(mountinfo: impl AsRef<[u8]>) -> Self {
        let entries = mountinfo
            .as_ref()
            .split(|byte| *byte == b'\n')
            .filter_map(|line| {
                let separator = line.windows(3).position(|bytes| bytes == b" - ")?;
                let (front, back) = (&line[..separator], &line[separator + 3..]);
                let mut fields = front.split(|byte| *byte == b' ');
                let id = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
                let (major, minor) = std::str::from_utf8(fields.nth(1)?).ok()?.split_once(':')?;
                let device = (major.parse().ok()?, minor.parse().ok()?);
                let mount_point = fields.nth(1)?;
                let mut back = back.split(|byte| *byte == b' ');
                let fs_type = std::str::from_utf8(back.next()?).ok()?;
                let super_options = back
                    .nth(1)
                    .and_then(|options| std::str::from_utf8(options).ok())
                    .unwrap_or_default();
                Some(MountEntry {
                    id,
                    device,
                    mount_point: PathBuf::from(unescape(mount_point)),
                    mount_type: MountType {
                        fs_type: fs_type.to_owned(),
                        super_options: super_options.to_owned(),
                    },
                })
            })
            .collect::<Vec<_>>();
        Self {
            entries: entries.into(),
        }
    }

    pub(crate) fn fs_type_for(&self, path: &Path) -> Option<&str> {
        self.mount_type_for(path)
            .map(|mount_type| mount_type.fs_type.as_str())
    }

    pub(crate) fn mount_type_for(&self, path: &Path) -> Option<&MountType> {
        self.innermost(path).map(|entry| &entry.mount_type)
    }

    pub(crate) fn mount_type_of(&self, id: u64) -> Option<&MountType> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| &entry.mount_type)
    }

    pub(crate) fn mount_for(&self, path: &Path) -> Option<MountKey> {
        self.innermost(path).map(|entry| MountKey {
            mount_id: entry.id,
            device: entry.device,
            mount_point: entry.mount_point.clone(),
            fs_type: entry.mount_type.fs_type.clone(),
        })
    }

    pub(super) fn is_remote_path(&self, path: &Path) -> bool {
        self.fs_type_for(path).is_some_and(is_remote_fs_type)
    }

    /// Remote filesystems other than FUSE drivers for local block devices,
    /// such as ntfs-3g and exfat-fuse on a USB drive.
    pub(crate) fn is_network_path(&self, path: &Path) -> bool {
        self.fs_type_for(path)
            .is_some_and(|fs_type| fs_type != "fuseblk" && is_remote_fs_type(fs_type))
    }

    pub(crate) fn has_mount_inside(&self, directory: &Path) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.mount_point.parent() == Some(directory))
    }

    pub(super) fn query_may_block(&self, path: &Path) -> bool {
        // Prefix probes can trigger autofs even when a nested mount is local.
        self.is_remote_path(path)
            || self.entries.iter().any(|entry| {
                entry.mount_type.fs_type == "autofs" && path.starts_with(&entry.mount_point)
            })
    }

    pub(crate) fn is_mount_point(&self, path: &Path) -> bool {
        self.entries.iter().any(|entry| entry.mount_point == path)
    }

    pub(crate) fn mount_point_for(&self, path: &Path) -> Option<&Path> {
        self.innermost(path)
            .map(|entry| entry.mount_point.as_path())
    }

    /// Excludes virtual or potentially blocking filesystems from fallback scans.
    pub(crate) fn trash_scan_mounts(&self) -> impl Iterator<Item = &Path> {
        self.entries.iter().filter_map(|entry| {
            is_trash_scan_fs_type(&entry.mount_type.fs_type).then_some(entry.mount_point.as_path())
        })
    }

    fn innermost(&self, path: &Path) -> Option<&MountEntry> {
        self.entries
            .iter()
            .filter(|entry| path.starts_with(&entry.mount_point))
            .max_by_key(|entry| entry.mount_point.as_os_str().len())
    }
}

/// Includes `fuseblk`, the FUSE drivers for local block devices.
pub(crate) fn is_fuse_fs_type(fs_type: &str) -> bool {
    fs_type.starts_with("fuse")
}

/// FUSE filesystems that layer or encrypt local storage rather than reach a
/// remote provider.
pub(crate) fn is_local_fuse_overlay(fs_type: &str) -> bool {
    matches!(
        fs_type.strip_prefix("fuse."),
        Some(
            "mergerfs"
                | "gocryptfs"
                | "encfs"
                | "cryfs"
                | "securefs"
                | "bindfs"
                | "fuse-overlayfs"
                | "unionfs"
                | "unionfs-fuse"
                | "mhddfs"
                | "squashfuse"
                | "archivemount"
                | "fuse-zip"
                | "ciopfs"
        )
    )
}

fn is_trash_scan_fs_type(fs_type: &str) -> bool {
    !is_remote_fs_type(fs_type) && !is_virtual_fs_type(fs_type)
}

fn is_virtual_fs_type(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "proc"
            | "sysfs"
            | "devtmpfs"
            | "tmpfs"
            | "cgroup"
            | "cgroup2"
            | "overlay"
            | "autofs"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "ramfs"
            | "hugetlbfs"
            | "mqueue"
            | "bpf"
            | "pstore"
            | "configfs"
            | "fusectl"
            | "rpc_pipefs"
            | "nsfs"
            | "binfmt_misc"
            | "devpts"
            | "efivarfs"
    )
}

/// Includes FUSE because an unavailable userspace daemon can block filesystem calls.
pub(super) fn is_remote_fs_type(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "nfs"
            | "nfs4"
            | "cifs"
            | "smb"
            | "smb2"
            | "smb3"
            | "smbfs"
            | "ncpfs"
            | "afs"
            | "afp"
            | "9p"
            | "ceph"
            | "coda"
            | "davfs"
            | "glusterfs"
            | "gfs"
            | "gfs2"
            | "lustre"
            | "ocfs2"
            | "webdav"
            | "vboxsf"
            | "virtiofs"
    ) || is_fuse_fs_type(fs_type)
}

/// Decodes mountinfo's octal escapes without assuming pathname bytes are UTF-8.
fn unescape(field: impl AsRef<[u8]>) -> OsString {
    let bytes = field.as_ref();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let octal = (bytes[index] == b'\\' && index + 3 < bytes.len())
            .then(|| &bytes[index + 1..index + 4])
            .filter(|digits| digits.iter().all(|digit| (b'0'..=b'7').contains(digit)))
            .map(|digits| {
                digits
                    .iter()
                    .fold(0u32, |code, digit| code * 8 + u32::from(digit - b'0'))
            });
        match octal.and_then(|code| u8::try_from(code).ok()) {
            Some(byte) => {
                out.push(byte);
                index += 4;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    OsString::from_vec(out)
}
