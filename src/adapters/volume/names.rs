// SPDX-License-Identifier: MIT

//! The rules a mount applies to the names it stores.

#[cfg(test)]
mod tests;

use std::ffi::OsStr;

/// Which characters a mount refuses besides `/` and NUL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NameRules {
    Posix,
    /// `< > : " \ | ? *` and U+0001 to U+001F.
    Windows,
}

/// Where dots and spaces are dropped from a name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Strip {
    None,
    /// Trailing dots and spaces.
    Trailing,
    /// Trailing dots and spaces, and leading spaces.
    Ends,
}

/// Which Windows device names (`CON`, `nul.txt`) a mount must not get.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Reserved {
    None,
    /// Those Windows 11 reserves: a bare device name, and `NUL` before any
    /// extension.
    Bare,
    /// A device name before any extension, as every Windows version reserves.
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NameEncoding {
    AnyBytes,
    Utf8,
    /// UTF-8 without characters above U+FFFF.
    Utf8Bmp,
    Ascii,
}

/// Private-use characters an SMB client stores for characters Windows
/// refuses, and so reads back as those characters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrivateUse {
    /// `mapposix`, the Services for Mac mapping.
    Sfm,
    /// `mapchars`, the Services for Unix mapping.
    Sfu,
}

/// A name fits when its UTF-8 and UTF-16 lengths both do.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct NameLimit {
    pub bytes: Option<u16>,
    pub units: Option<u16>,
    /// Folders must also fit within this many bytes, where a server
    /// creates them under a longer temporary name.
    pub folder_bytes: Option<u16>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NameProfile {
    pub rules: NameRules,
    pub strip: Strip,
    pub reserved: Reserved,
    pub encoding: NameEncoding,
    pub mapped: Option<PrivateUse>,
    pub limit: NameLimit,
    /// Whether names the mount refuses may teach it stricter rules, where
    /// nothing it reports shows its rules.
    pub learns: bool,
}

/// Where a mount's name rules come from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MountNames {
    Profile(NameProfile),
    /// The GVfs FUSE bridge, whose mount point is this many folders deep:
    /// the folder below it names the backend whose rules apply inside.
    PerBackend {
        depth: u16,
    },
}

const WINDOWS_UNITS: u16 = 255;
/// Samba creates a folder as `.::TMPNAME:D:<pid>%<u64>:<name>` first, which
/// takes up to 42 of the server's 255 bytes.
const SAMBA_FOLDER_BYTES: u16 = 213;
const SAMBA_BYTES: u16 = 255;
/// MTP strings count their terminator in one byte, and libmtp sends an
/// empty name for anything longer instead of failing.
const MTP_UNITS: u16 = 254;
const MTP_BYTES: u16 = 255;
const SFTP_BYTES: u16 = 255;

impl NameProfile {
    pub(crate) fn posix(bytes: Option<u16>) -> Self {
        Self {
            rules: NameRules::Posix,
            strip: Strip::None,
            reserved: Reserved::None,
            encoding: NameEncoding::AnyBytes,
            mapped: None,
            limit: NameLimit {
                bytes,
                ..NameLimit::default()
            },
            learns: false,
        }
    }

    /// FAT, exFAT and NTFS, which count a name in UTF-16 units.
    pub(crate) fn windows(reserved: Reserved, encoding: NameEncoding) -> Self {
        Self {
            rules: NameRules::Windows,
            strip: Strip::Trailing,
            reserved,
            encoding,
            mapped: None,
            limit: NameLimit {
                units: Some(WINDOWS_UNITS),
                ..NameLimit::default()
            },
            learns: false,
        }
    }

    /// A share without POSIX extensions: the server's Windows rules, in the
    /// bytes it stores and the UTF-16 units SMB carries.
    pub(crate) fn smb(bytes: Option<u16>) -> Self {
        Self {
            limit: NameLimit {
                bytes,
                units: Some(WINDOWS_UNITS),
                folder_bytes: Some(SAMBA_FOLDER_BYTES),
            },
            ..Self::windows(Reserved::Full, NameEncoding::Utf8)
        }
    }

    /// A share with POSIX extensions, which still creates folders under
    /// Samba's temporary names and cannot carry malformed UTF-8.
    pub(crate) fn smb_posix(bytes: Option<u16>) -> Self {
        Self {
            encoding: NameEncoding::Utf8,
            limit: NameLimit {
                bytes,
                folder_bytes: Some(SAMBA_FOLDER_BYTES),
                ..NameLimit::default()
            },
            ..Self::posix(bytes)
        }
    }

    /// A mount whose type tells nothing about its names.
    pub(crate) fn unknown(name_max: u32) -> Self {
        Self::posix(byte_limit(name_max)).learning()
    }

    pub(crate) fn learning(self) -> Self {
        Self {
            learns: true,
            ..self
        }
    }
}

/// `f_namelen` as a byte limit: 0 means unknown, and a FUSE daemon may
/// report anything.
pub(crate) fn byte_limit(name_max: u32) -> Option<u16> {
    (name_max != 0).then(|| name_max.clamp(64, 4095) as u16)
}

/// The rules of a GVfs backend, by its URI scheme or by the text before the
/// colon of its folder on the FUSE bridge (`smb-share:server=…`).
pub(crate) fn gvfs_profile(backend: &str) -> NameProfile {
    match backend {
        "smb" | "smb-share" => NameProfile::smb(Some(SAMBA_BYTES)),
        "mtp" => NameProfile {
            strip: Strip::Ends,
            limit: NameLimit {
                bytes: Some(MTP_BYTES),
                units: Some(MTP_UNITS),
                folder_bytes: None,
            },
            ..NameProfile::windows(Reserved::Full, NameEncoding::Utf8)
        },
        "sftp" => NameProfile::posix(Some(SFTP_BYTES)).learning(),
        _ => NameProfile::posix(None).learning(),
    }
}

/// The rules inside a GVfs FUSE bridge folder, by the folder the bridge
/// serves its backend from (`mtp:host=…`). Where that is unknown, the
/// strictest rules a backend needs apply, and refusals teach the rest.
pub(crate) fn gvfs_bridge_profile(backend_folder: Option<&OsStr>) -> NameProfile {
    let scheme = backend_folder.and_then(|folder| {
        let folder = folder.as_encoded_bytes();
        let colon = folder.iter().position(|byte| *byte == b':')?;
        std::str::from_utf8(&folder[..colon]).ok()
    });
    match scheme {
        Some(scheme) => gvfs_profile(scheme),
        None => NameProfile {
            limit: NameLimit {
                folder_bytes: Some(SAMBA_FOLDER_BYTES),
                ..gvfs_profile("mtp").limit
            },
            ..gvfs_profile("mtp").learning()
        },
    }
}
