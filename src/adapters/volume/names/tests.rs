// SPDX-License-Identifier: MIT

use super::{NameEncoding, NameLimit, NameProfile, NameRules, Reserved, Strip, gvfs_profile};

fn limit(bytes: Option<u16>, units: Option<u16>, folder_bytes: Option<u16>) -> NameLimit {
    NameLimit {
        bytes,
        units,
        folder_bytes,
    }
}

#[test]
fn gvfs_backends_get_the_rules_of_what_serves_them() {
    let posix = |bytes| NameProfile {
        rules: NameRules::Posix,
        strip: Strip::None,
        reserved: Reserved::None,
        encoding: NameEncoding::AnyBytes,
        mapped: None,
        limit: limit(bytes, None, None),
        learns: true,
    };
    // libsmbclient maps nothing, so the server's Windows rules apply as they
    // are, and Samba still creates folders under its temporary names.
    let smb = NameProfile {
        rules: NameRules::Windows,
        strip: Strip::Trailing,
        reserved: Reserved::Full,
        encoding: NameEncoding::Utf8,
        mapped: None,
        limit: limit(Some(255), Some(255), Some(213)),
        learns: false,
    };
    // libmtp sends an empty name past 254 units or 255 bytes, and Android
    // trims surrounding spaces.
    let mtp = NameProfile {
        strip: Strip::Ends,
        limit: limit(Some(255), Some(254), None),
        ..smb
    };
    for (backend, profile) in [
        ("smb", smb),
        ("smb-share", smb),
        ("mtp", mtp),
        ("sftp", posix(Some(255))),
        ("dav", posix(None)),
        ("google-drive", posix(None)),
        ("", posix(None)),
    ] {
        assert_eq!(gvfs_profile(backend), profile, "{backend}");
    }
}
