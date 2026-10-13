// SPDX-License-Identifier: MIT

#[cfg(test)]
mod tests;

use std::{
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use gtk::gio;

/// Parsed without touching mounted filesystems, including unresponsive mounts.
#[derive(Clone)]
pub(crate) struct MountTable {
    entries: Arc<[MountEntry]>,
}

struct MountEntry {
    id: u64,
    mount_point: PathBuf,
    mount_type: MountType,
}

/// A mount's mountinfo type and superblock options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MountType {
    pub fs_type: String,
    pub super_options: String,
}

impl MountType {
    /// Whether an entry keeps its inode number: libfuse without `use_ino`
    /// and cifs without server inode numbers number an entry afresh once the
    /// kernel has evicted it.
    pub(crate) fn stable_inodes(&self) -> bool {
        match family(&self.fs_type) {
            Family::Fuse => false,
            Family::Smb => !self
                .super_options
                .split(',')
                .any(|option| option == "noserverino"),
            _ => true,
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
        "nfs" => Family::Nfs,
        "nfs4" => Family::Nfs4,
        "cifs" | "smb3" => Family::Smb,
        "fuse" | "fuseblk" => Family::Fuse,
        fs_type if fs_type.starts_with("fuse.") => Family::Fuse,
        _ => Family::Other,
    }
}

impl MountTable {
    pub(crate) fn current() -> Self {
        std::fs::read("/proc/self/mountinfo")
            .map(Self::parse)
            .unwrap_or_else(|_| Self::parse(b""))
    }

    /// [`Self::current`], read off the calling thread.
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
                let mount_point = fields.nth(3)?;
                let mut back = back.split(|byte| *byte == b' ');
                let fs_type = std::str::from_utf8(back.next()?).ok()?;
                let super_options = back
                    .nth(1)
                    .and_then(|options| std::str::from_utf8(options).ok())
                    .unwrap_or_default();
                Some(MountEntry {
                    id,
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

    /// The mount with this mountinfo id (`STATX_MNT_ID`).
    pub(crate) fn mount_type_of(&self, id: u64) -> Option<&MountType> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| &entry.mount_type)
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
    ) || fs_type.starts_with("fuse")
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
