// SPDX-License-Identifier: MIT

use std::{error::Error, fs, os::fd::AsFd, path::Path};

use super::super::{
    fs_ops::fs,
    open_local_parent_directory,
    tests::fs_quirks::{FsQuirks, Preset, Quirks},
};
use super::{MountProfile, Profiles};
use crate::adapters::volume::{
    FAT32_MAX_FILE_SIZE, MountNames, NameEncoding, NameLimit, NameProfile, NameRules, PrivateUse,
    Reserved, Strip, gvfs_profile, learn_mount_capability,
};

fn profile_of(profiles: &Profiles, folder: &Path) -> Result<MountProfile, Box<dyn Error>> {
    let folder = open_local_parent_directory(folder)?;
    Ok(profiles.of(fs(), folder.as_fd()))
}

fn posix(bytes: Option<u16>, learns: bool) -> NameProfile {
    NameProfile {
        rules: NameRules::Posix,
        strip: Strip::None,
        reserved: Reserved::None,
        encoding: NameEncoding::AnyBytes,
        mapped: None,
        limit: NameLimit {
            bytes,
            ..NameLimit::default()
        },
        learns,
    }
}

fn windows(reserved: Reserved, learns: bool) -> NameProfile {
    NameProfile {
        rules: NameRules::Windows,
        strip: Strip::Trailing,
        reserved,
        encoding: NameEncoding::Utf8,
        mapped: None,
        limit: NameLimit {
            units: Some(255),
            ..NameLimit::default()
        },
        learns,
    }
}

#[test]
fn each_mount_is_profiled_from_what_its_folder_reports() -> Result<(), Box<dyn Error>> {
    let profile = |names, symlinks, max_file_size| MountProfile {
        names,
        symlinks,
        max_file_size,
    };
    let smb = |encoding| NameProfile {
        encoding,
        mapped: Some(PrivateUse::Sfm),
        limit: NameLimit {
            bytes: Some(255),
            units: Some(255),
            folder_bytes: Some(213),
        },
        ..windows(Reserved::Full, false)
    };
    let unknown_udev = || Quirks {
        device_format: None,
        ..Quirks::exfat_fuse()
    };
    let zfs = || Quirks {
        mount_type: Some(("zfs", "rw,xattr,posixacl")),
        name_max: Some(1023),
        ..Quirks::default()
    };
    let silent_daemon = || Quirks {
        name_max: Some(0),
        ..Quirks::sshfs()
    };
    let no_symlinks = Some(false);
    let fat32 = Some(FAT32_MAX_FILE_SIZE);
    let cases: [(&str, Preset, MountProfile); 12] = [
        (
            "vfat",
            Quirks::vfat,
            profile(windows(Reserved::Bare, false), no_symlinks, fat32),
        ),
        (
            "exfat",
            Quirks::exfat,
            profile(windows(Reserved::Bare, false), no_symlinks, None),
        ),
        (
            "exfat-fuse",
            Quirks::exfat_fuse,
            profile(windows(Reserved::Bare, false), no_symlinks, None),
        ),
        (
            "fuseblk without udev",
            unknown_udev,
            profile(windows(Reserved::Bare, true), None, None),
        ),
        (
            "ntfs3",
            Quirks::ntfs3,
            profile(windows(Reserved::Bare, false), None, None),
        ),
        (
            "ntfs3 windows_names",
            Quirks::ntfs3_windows_names,
            profile(windows(Reserved::Full, false), None, None),
        ),
        (
            "ntfs-3g",
            Quirks::ntfs_3g,
            profile(windows(Reserved::Full, false), None, None),
        ),
        (
            "cifs",
            Quirks::cifs,
            profile(smb(NameEncoding::Utf8), None, None),
        ),
        (
            "cifs nocase",
            Quirks::cifs_nocase,
            profile(smb(NameEncoding::Utf8Bmp), None, None),
        ),
        (
            "cifs posix",
            Quirks::cifs_posix,
            profile(
                NameProfile {
                    encoding: NameEncoding::Utf8,
                    limit: NameLimit {
                        bytes: Some(255),
                        folder_bytes: Some(213),
                        ..NameLimit::default()
                    },
                    ..posix(None, false)
                },
                None,
                None,
            ),
        ),
        ("zfs", zfs, profile(posix(Some(1023), false), None, None)),
        (
            "sshfs reporting no name length",
            silent_daemon,
            profile(posix(None, true), None, None),
        ),
    ];
    for (case, quirks, expected) in cases {
        let root = tempfile::tempdir()?;
        let _scope = FsQuirks::install(root.path(), quirks());

        assert_eq!(
            profile_of(&Profiles::default(), root.path())?,
            expected,
            "{case}"
        );
    }
    Ok(())
}

#[test]
fn the_gvfs_bridge_takes_the_rules_of_each_backend_from_its_folder() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let _bridge = FsQuirks::install(
        root.path(),
        Quirks {
            mount_type: Some(("fuse.gvfsd-fuse", "rw,user_id=1000,group_id=1000")),
            ..Quirks::default()
        },
    );
    let phone = root
        .path()
        .join("mtp:host=Google_Pixel_8_Pro_3A101FDJG0012B");
    let share = root.path().join("smb-share:server=nas.local,share=media");
    let server = root.path().join("sftp:host=example.org,user=me");
    let cloud = root.path().join("dav:host=cloud.example.org,ssl=true");
    // A folder named like a backend deeper inside one changes nothing.
    let nested = phone.join("Internal storage/smb-share:server=x,share=y");
    for folder in [&share, &server, &cloud, &nested] {
        fs::create_dir_all(folder)?;
    }
    let profiles = Profiles::default();

    for (folder, backend) in [
        (phone.join("Internal storage"), "mtp"),
        (nested, "mtp"),
        (share, "smb"),
        (server, "sftp"),
        (cloud, "dav"),
        (root.path().to_path_buf(), ""),
    ] {
        assert_eq!(
            profile_of(&profiles, &folder)?.names,
            gvfs_profile(backend),
            "{}",
            folder.display()
        );
    }
    Ok(())
}

#[test]
fn an_operation_keeps_the_profile_it_read_first_when_its_mount_changes()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let scope = FsQuirks::install(root.path(), Quirks::ntfs3());
    let operation = Profiles::default();
    let before = profile_of(&operation, root.path())?;
    let remounted = windows(Reserved::Full, false);

    learn_mount_capability(scope.key(), |capabilities| {
        capabilities.names = Some(MountNames::Profile(remounted));
    });

    assert_eq!(profile_of(&operation, root.path())?, before);
    assert_eq!(profile_of(&operation.clone(), root.path())?, before);
    assert_eq!(
        profile_of(&Profiles::default(), root.path())?.names,
        remounted
    );
    Ok(())
}
