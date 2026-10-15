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

use super::names::{NameEncoding, NameProfile, NameRules, PrivateUse, Reserved, Strip, byte_limit};

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

impl MountEntry {
    fn parts(&self) -> (&MountType, &Path) {
        (&self.mount_type, &self.mount_point)
    }
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

    /// `device_format` is udev's `ID_FS_TYPE`, read for `fuseblk` only, and
    /// `name_max` the mount's `f_namelen`, which only byte-counting
    /// filesystems mean. A `fuseblk` mount whose device udev cannot tell is
    /// taken for a Windows format, as for `coarse_times`.
    pub(crate) fn name_profile(&self, device_format: Option<&str>, name_max: u32) -> NameProfile {
        let has = |name| self.super_options.split(',').any(|option| option == name);
        let bytes = byte_limit(name_max);
        if fat_format(&self.fs_type, device_format).is_some() {
            return NameProfile::windows(Reserved::Bare, self.fat_encoding());
        }
        match (self.fs_type.as_str(), device_format) {
            ("ntfs3" | "ntfs", _) if has("windows_names") => {
                NameProfile::windows(Reserved::Full, NameEncoding::Utf8)
            }
            ("ntfs3" | "ntfs", _) => NameProfile::windows(Reserved::Bare, NameEncoding::Utf8),
            // udisks mounts ntfs-3g with `windows_names`, which mountinfo hides.
            ("fuseblk", Some("ntfs")) => NameProfile::windows(Reserved::Full, NameEncoding::Utf8),
            ("fuseblk", _) => NameProfile::windows(Reserved::Bare, NameEncoding::Utf8).learning(),
            ("cifs" | "smb3", _) if has("posix") || has("unix") && has("posixpaths") => {
                NameProfile::smb_posix(bytes)
            }
            ("cifs" | "smb3", _) => NameProfile {
                encoding: if has("nocase") {
                    NameEncoding::Utf8Bmp
                } else {
                    NameEncoding::Utf8
                },
                mapped: if has("mapposix") {
                    Some(PrivateUse::Sfm)
                } else if has("mapchars") {
                    Some(PrivateUse::Sfu)
                } else {
                    None
                },
                ..NameProfile::smb(bytes)
            },
            // Counted in UTF-16 units like the Windows formats, without their rules.
            ("hfsplus", _) => NameProfile {
                rules: NameRules::Posix,
                strip: Strip::None,
                ..NameProfile::windows(Reserved::None, NameEncoding::Utf8)
            },
            (fs_type, _) => match family(fs_type) {
                Family::Ext
                | Family::Xfs
                | Family::Btrfs
                | Family::F2fs
                | Family::Tmpfs
                | Family::Bcachefs
                | Family::Zfs
                | Family::Overlay
                | Family::Nfs
                | Family::Nfs4 => NameProfile::posix(bytes),
                _ => NameProfile::unknown(name_max),
            },
        }
    }

    /// Without `utf8`, vfat converts names through its I/O charset, and so
    /// does exFAT unless that is UTF-8. Code pages other than ASCII store
    /// other bytes on vfat and refuse them on exFAT, and are not supported.
    fn fat_encoding(&self) -> NameEncoding {
        let has = |name| self.super_options.split(',').any(|option| option == name);
        let iocharset = self
            .super_options
            .split(',')
            .find_map(|option| option.strip_prefix("iocharset="));
        match (self.fs_type.as_str(), iocharset) {
            ("vfat", Some("utf8")) if !has("utf8") => NameEncoding::Utf8Bmp,
            ("vfat" | "exfat", Some("ascii")) if !has("utf8") => NameEncoding::Ascii,
            _ => NameEncoding::Utf8,
        }
    }

    /// The bridge mounts every GVfs backend in a folder of its own.
    pub(crate) fn is_gvfs_bridge(&self) -> bool {
        self.fs_type == "fuse.gvfsd-fuse"
    }
}

pub(crate) const FAT32_MAX_FILE_SIZE: u64 = u32::MAX as u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FatFormat {
    Fat,
    Exfat,
}

/// FAT and exFAT, served by the kernel or by a FUSE daemon on a device udev
/// names. `device_format` is udev's `ID_FS_TYPE`, read for `fuseblk` only.
fn fat_format(fs_type: &str, device_format: Option<&str>) -> Option<FatFormat> {
    match (fs_type, device_format) {
        ("vfat" | "msdos", _) | ("fuseblk", Some("vfat")) => Some(FatFormat::Fat),
        ("exfat", _) | ("fuseblk", Some("exfat")) => Some(FatFormat::Exfat),
        _ => None,
    }
}

/// Drivers without symbolic links: the kernel's FAT and exFAT have no
/// `symlink`, and exfat-fuse implements none.
pub(crate) fn holds_symlinks(fs_type: &str, device_format: Option<&str>) -> Option<bool> {
    fat_format(fs_type, device_format).map(|_| false)
}

pub(crate) fn max_file_size(fs_type: &str, device_format: Option<&str>) -> Option<u64> {
    (fat_format(fs_type, device_format) == Some(FatFormat::Fat)).then_some(FAT32_MAX_FILE_SIZE)
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

impl MountTable {
    pub(crate) fn current() -> Self {
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
        self.innermost(path)
            .map(|entry| entry.mount_type.fs_type.as_str())
    }

    pub(crate) fn mount_of(&self, id: u64) -> Option<(&MountType, &Path)> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(MountEntry::parts)
    }

    pub(crate) fn mount_for(&self, path: &Path) -> Option<(&MountType, &Path)> {
        self.innermost(path).map(MountEntry::parts)
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
