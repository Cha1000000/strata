// SPDX-License-Identifier: MIT

use rustix::{fs::Mode, io::Errno};

use super::super::{
    fs_ops::{Entry, fs},
    mount_profile::Profiles,
    naming::{
        Key, PlanEntry, Planned, fat_family_profile,
        fix::{Cause, fix_name},
        plan_names,
    },
};
use super::*;
use crate::adapters::volume::{
    NameEncoding, NameLimit, NameProfile, PrivateUse, Reserved, gvfs_bridge_profile, gvfs_profile,
};

#[test]
fn copy_suffix_parsing_and_candidate_naming() {
    for (input, stem, suffix) in [
        ("name", "name", None),
        ("name (1)", "name", Some(1u64)),
        ("name (2)", "name", Some(2)),
        ("name (42)", "name", Some(42)),
        ("name (foo)", "name (foo)", None),
        ("name (0)", "name (0)", None),
        ("name (2", "name (2", None),
        (
            "name (18446744073709551615)",
            "name (18446744073709551615)",
            None,
        ),
        (".gitignore", ".gitignore", None),
        (".gitignore (1)", ".gitignore", Some(1)),
        ("archive", "archive", None),
        ("archive (1)", "archive", Some(1)),
    ] {
        assert_eq!(
            parse_copy_suffix(OsStr::new(input)),
            (OsStr::new(stem), suffix)
        );
    }

    for (stem, extension, number, expected) in [
        ("name", ".ext", 1u64, "name (1).ext"),
        ("name", ".ext", 2, "name (2).ext"),
        ("name", "", 1, "name (1)"),
        ("name", "", 2, "name (2)"),
        (".gitignore", "", 1, ".gitignore (1)"),
        (".config", "", 2, ".config (2)"),
        ("archive", ".tar.gz", 1, "archive (1).tar.gz"),
        ("archive", ".tar.gz", 3, "archive (3).tar.gz"),
        ("backup.tar", ".gz", 1, "backup.tar (1).gz"),
    ] {
        assert_eq!(
            numbered_name(
                OsStr::new(stem),
                number,
                OsStr::new(extension),
                NameLimit::default(),
                false
            ),
            OsString::from(expected)
        );
    }
    assert_eq!(
        numbered_names(
            OsStr::new("a (18446744073709551614).txt"),
            false,
            NameLimit::default()
        )
        .take(2)
        .collect::<Vec<_>>(),
        ["a (18446744073709551615).txt", "a (1).txt"]
    );
}

#[test]
fn fixed_names_are_numbered_past_the_names_their_siblings_keep() {
    // An entry's name, whether it is a folder, whether it is fixed, the name
    // planned for it, and whether that is numbered as another kept name's
    // alias.
    let file =
        |name: &str, planned: &str| (name.to_owned(), false, true, planned.to_owned(), false);
    let folder =
        |name: &str, planned: &str| (name.to_owned(), true, true, planned.to_owned(), false);
    let alias = |name: &str, is_directory, planned: &str| {
        (
            name.to_owned(),
            is_directory,
            true,
            planned.to_owned(),
            true,
        )
    };
    let unfixed = |name: &str| (name.to_owned(), false, false, name.to_owned(), false);
    let cjk = |count| "\u{4e2d}".repeat(count);
    let rows = [
        (
            Key::Folded,
            vec![file("a:b", "a_b (1)"), file("a_b", "a_b")],
        ),
        (
            Key::Folded,
            vec![
                file("a?b", "a_b (3)"),
                file("a_b (1)", "a_b (1)"),
                file("a:b", "a_b (2)"),
                file("a_b", "a_b"),
            ],
        ),
        (
            Key::Folded,
            vec![file("A:b", "A_b (1)"), file("a_b", "a_b")],
        ),
        (Key::Exact, vec![file("A:b", "A_b"), file("a_b", "a_b")]),
        (
            Key::Folded,
            vec![file("A.txt", "A.txt"), alias("a.txt", false, "a (1).txt")],
        ),
        (
            Key::Folded,
            vec![
                alias("a.txt", false, "a (2).txt"),
                file("a (1).txt", "a (1).txt"),
                file("A.txt", "A.txt"),
            ],
        ),
        (
            Key::Folded,
            vec![
                alias("a_b", false, "a_b (2)"),
                file("a:b", "a_b (1)"),
                file("A_b", "A_b"),
            ],
        ),
        (
            Key::Folded,
            vec![
                folder("Photos.2024", "Photos.2024"),
                alias("photos.2024", true, "photos.2024 (1)"),
            ],
        ),
        (
            Key::Folded,
            vec![unfixed("a.txt"), alias("A.txt", false, "A (1).txt")],
        ),
        (
            Key::Exact,
            vec![file("A.txt", "A.txt"), file("a.txt", "a.txt")],
        ),
        (Key::Folded, vec![unfixed("x:y"), file("x?y", "x_y")]),
        (
            Key::Folded,
            vec![folder("v1.2:", "v1.2_ (1)"), folder("v1.2_", "v1.2_")],
        ),
        (
            Key::Folded,
            vec![
                file(
                    &format!("{}.txt.", cjk(250)),
                    &format!("{} (1).txt", cjk(247)),
                ),
                file(&format!("{}.txt", cjk(250)), &format!("{}.txt", cjk(250))),
            ],
        ),
    ];
    for (key, row) in rows {
        for reversed in [false, true] {
            let mut row = row.clone();
            if reversed {
                row.reverse();
            }
            let entries = row
                .iter()
                .map(|(name, is_directory, fixing, ..)| PlanEntry {
                    name: OsStr::new(name),
                    is_directory: *is_directory,
                    fixing: *fixing,
                })
                .collect::<Vec<_>>();
            let planned = row
                .iter()
                .map(|(.., planned, alias)| Planned {
                    target: OsString::from(planned),
                    alias: *alias,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                plan_names(&entries, &fat_family_profile(), key),
                Ok(planned),
                "{key:?} {row:?}"
            );
        }
    }
}

#[test]
fn planning_fails_where_a_fixed_name_has_no_free_number() {
    use super::super::placement::{NUMBERED_NAME_ATTEMPTS, PlaceError};
    let taken = std::iter::once(OsString::from("a_b"))
        .chain((1..=NUMBERED_NAME_ATTEMPTS).map(|number| OsString::from(format!("a_b ({number})"))))
        .collect::<Vec<_>>();
    let entries = taken
        .iter()
        .map(|name| PlanEntry {
            name,
            is_directory: false,
            fixing: false,
        })
        .chain([PlanEntry {
            name: OsStr::new("a:b"),
            is_directory: false,
            fixing: true,
        }])
        .collect::<Vec<_>>();
    assert_eq!(
        plan_names(&entries, &fat_family_profile(), Key::Exact),
        Err(PlaceError::Exists)
    );
}

#[test]
fn numbered_names_are_cut_to_the_limit_of_their_mount() {
    let bytes = NameProfile::posix(Some(255)).limit;
    let units = NameProfile::windows(Reserved::Bare, NameEncoding::Utf8).limit;
    let smb = NameProfile::smb(Some(255)).limit;
    let repeat = |text: &str, count| text.repeat(count);
    let (cjk, emoji, flag) = ("\u{4e2d}", "\u{1f600}", "\u{1f1eb}\u{1f1f7}");
    for (name, folder, limit, expected) in [
        (
            format!("{}.txt", repeat("a", 251)),
            false,
            bytes,
            format!("{} (1).txt", repeat("a", 247)),
        ),
        (
            format!("{} (1).txt", repeat("a", 247)),
            false,
            bytes,
            format!("{} (2).txt", repeat("a", 247)),
        ),
        (
            format!("{}.tar.gz", repeat("a", 247)),
            false,
            bytes,
            format!("{}.tar (1).gz", repeat("a", 244)),
        ),
        (
            format!("{}.v2", repeat("d", 250)),
            false,
            bytes,
            format!("{} (1).v2", repeat("d", 248)),
        ),
        (
            format!("{}.v2", repeat("d", 250)),
            true,
            bytes,
            format!("{}. (1)", repeat("d", 250)),
        ),
        (
            format!("x.{}", repeat("a", 250)),
            false,
            bytes,
            format!("x.{} (1)", repeat("a", 249)),
        ),
        (
            repeat(cjk, 85),
            false,
            bytes,
            format!("{} (1)", repeat(cjk, 83)),
        ),
        (
            repeat(cjk, 85),
            false,
            units,
            format!("{} (1)", repeat(cjk, 85)),
        ),
        (
            repeat(emoji, 126),
            false,
            units,
            format!("{} (1)", repeat(emoji, 125)),
        ),
        (
            format!("abcd{}", repeat(flag, 31)),
            false,
            bytes,
            format!("abcd{} (1)", repeat(flag, 30)),
        ),
        (
            repeat("a", 212),
            true,
            smb,
            format!("{} (1)", repeat("a", 209)),
        ),
        (
            repeat("a", 212),
            false,
            smb,
            format!("{} (1)", repeat("a", 212)),
        ),
    ] {
        assert_eq!(
            numbered_names(OsStr::new(&name), folder, limit).next(),
            Some(OsString::from(&expected)),
            "{name:?} {limit:?}"
        );
    }
}

#[test]
fn duplicating_a_file_preserves_non_utf8_name_bytes() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().to_path_buf();
    let source_name = OsString::from_vec(b"photo-\xff.jpg".to_vec());
    let source = destination.join(&source_name);
    fs::write(&source, b"original-content")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(15),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: false,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );

    while !events.borrow().iter().any(|event| {
        matches!(
            event,
            OperationEvent::Pasted { .. } | OperationEvent::TransferFailed { .. }
        )
    }) {
        glib::MainContext::default().iteration(true);
    }

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Pasted { .. })
    ));
    let duplicate_name = OsString::from_vec(b"photo-\xff (1).jpg".to_vec());
    let duplicate = destination.join(duplicate_name);
    assert_eq!(fs::read(duplicate)?, b"original-content");
    Ok(())
}

#[test]
fn duplicating_an_existing_numbered_name_advances_its_index() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().to_path_buf();
    let source = destination.join("photo (1).jpg");
    fs::write(&source, b"copy-content")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(11),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: false,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );

    while !events.borrow().iter().any(|event| {
        matches!(
            event,
            OperationEvent::Pasted { .. } | OperationEvent::TransferFailed { .. }
        )
    }) {
        glib::MainContext::default().iteration(true);
    }

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Pasted { .. })
    ));
    assert!(source.exists());
    assert_eq!(fs::read(&source)?, b"copy-content");
    let duplicate = destination.join("photo (2).jpg");
    assert!(duplicate.exists());
    assert_eq!(fs::read(&duplicate)?, b"copy-content");
    Ok(())
}

#[test]
fn duplicating_file_with_existing_numbered_name_advances_to_next_index()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().to_path_buf();
    let source = destination.join("photo.jpg");
    fs::write(&source, b"original")?;
    fs::write(destination.join("photo (1).jpg"), b"first copy")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(12),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: false,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );

    while !events.borrow().iter().any(|event| {
        matches!(
            event,
            OperationEvent::Pasted { .. } | OperationEvent::TransferFailed { .. }
        )
    }) {
        glib::MainContext::default().iteration(true);
    }

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Pasted { .. })
    ));
    assert_eq!(fs::read(destination.join("photo (2).jpg"))?, b"original");
    assert_eq!(fs::read(destination.join("photo (1).jpg"))?, b"first copy");
    Ok(())
}

#[test]
fn duplicating_a_directory_generates_numbered_name() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().to_path_buf();
    let source = destination.join("documents");
    fs::create_dir_all(&source)?;
    fs::write(source.join("notes.txt"), b"nested-file")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(13),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: false,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );

    while !events.borrow().iter().any(|event| {
        matches!(
            event,
            OperationEvent::Pasted { .. } | OperationEvent::TransferFailed { .. }
        )
    }) {
        glib::MainContext::default().iteration(true);
    }

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Pasted { .. })
    ));
    assert!(source.is_dir());
    let duplicate = destination.join("documents (1)");
    assert!(duplicate.is_dir());
    assert_eq!(fs::read(duplicate.join("notes.txt"))?, b"nested-file");
    Ok(())
}

#[test]
fn duplicating_hidden_file_generates_correct_numbered_name() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().to_path_buf();
    let source = destination.join(".gitignore");
    fs::write(&source, b"target/")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(30),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: false,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );

    while !events.borrow().iter().any(|event| {
        matches!(
            event,
            OperationEvent::Pasted { .. } | OperationEvent::TransferFailed { .. }
        )
    }) {
        glib::MainContext::default().iteration(true);
    }

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Pasted { .. })
    ));
    assert!(source.exists());
    assert_eq!(fs::read(destination.join(".gitignore (1)"))?, b"target/");
    Ok(())
}

#[test]
fn duplicating_multi_extension_file_uses_last_extension_for_candidate_name()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let destination = root.path().to_path_buf();
    let source = destination.join("backup.tar.gz");
    fs::write(&source, b"contents")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(31),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: false,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );

    while !events.borrow().iter().any(|event| {
        matches!(
            event,
            OperationEvent::Pasted { .. } | OperationEvent::TransferFailed { .. }
        )
    }) {
        glib::MainContext::default().iteration(true);
    }

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Pasted { .. })
    ));
    assert!(source.exists());
    assert_eq!(
        fs::read(destination.join("backup.tar (1).gz"))?,
        b"contents"
    );
    Ok(())
}

#[test]
fn pasted_names_fixed_for_fat_are_planned_after_the_names_kept() {
    use gtk::gio::FileType::{Directory, Regular};
    let destination = gio::File::for_path("/media/stick");
    let sources = [
        "/one/a:b",
        "/two/a_b",
        "/one/v1.2:",
        "/two/v1.2:",
        "/one/A:c",
        "/two/a_c",
        "/one/x",
        "/two/X",
        "/three/x",
    ]
    .map(gio::File::for_path);
    let mut kinds = [Some(Regular); 9];
    kinds[3] = Some(Directory);
    for (fat_family, expected) in [
        (
            false,
            ["a:b", "a_b", "v1.2:", "v1.2:", "A:c", "a_c", "x", "X", "x"],
        ),
        (
            true,
            [
                "a_b (1)",
                "a_b",
                "v1.2_",
                "v1.2_ (1)",
                "A_c (1)",
                "a_c",
                "x (1)",
                "X",
                "x (2)",
            ],
        ),
    ] {
        let targets = super::super::paste_targets(&sources, &destination, fat_family, &kinds)
            .expect("free names");
        assert_eq!(
            targets
                .iter()
                .map(|target| target.as_ref().and_then(gio::File::basename))
                .collect::<Vec<_>>(),
            expected.map(|name| Some(PathBuf::from(name))),
            "{fat_family}"
        );
    }
}

#[test]
fn transfer_targets_resolve_trash_leaf_names() {
    let destination = gio::File::for_path("/home/user/Documents");
    let target_name = |source: &gio::File| {
        let target =
            super::super::paste_targets(std::slice::from_ref(source), &destination, false, &[None])
                .ok()?
                .pop()??;
        Some(
            target
                .path()?
                .strip_prefix(destination.path()?)
                .ok()?
                .as_os_str()
                .to_os_string(),
        )
    };
    let local = gio::File::for_path("/home/user/notes.txt");
    assert_eq!(target_name(&local), Some(OsString::from("notes.txt")));

    let home_trash = gio::File::for_uri("trash:///report.txt");
    assert_eq!(target_name(&home_trash), Some(OsString::from("report.txt")));

    let external_trash =
        gio::File::for_uri("trash:///%5Cmnt%5CData%5C.Trash-1000%5Cfiles%5CSAMPLED");
    assert_eq!(
        target_name(&external_trash),
        Some(OsString::from("SAMPLED"))
    );

    let external_trash_numbered =
        gio::File::for_uri("trash:///%5Cmnt%5CData%5C.Trash-1000%5Cfiles%5CSAMPLED.2");
    assert_eq!(
        target_name(&external_trash_numbered),
        Some(OsString::from("SAMPLED.2"))
    );

    let home_trash_with_backslash = gio::File::for_uri("trash:///foo%5Cbar.txt");
    assert_eq!(
        target_name(&home_trash_with_backslash),
        Some(OsString::from("foo\\bar.txt"))
    );
}

/// The rules a destination's profile gives its names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Rules {
    Posix,
    /// FAT, exFAT and ntfs3, which store device names.
    Bare,
    /// `windows_names`, which refuses them.
    Full,
    Smb,
    SmbNocase,
    SmbPosix,
    Mtp,
    /// GVfs, through libsmbclient, which maps nothing.
    GvfsSmb,
    /// A GVfs bridge folder whose backend is unknown.
    AnyGvfs,
}

const LAB_MOUNTS: [(&str, Preset, Rules); 13] = [
    ("ext4", Quirks::ext4, Rules::Posix),
    ("nfs42", Quirks::nfs42, Rules::Posix),
    ("nfs3", Quirks::nfs3, Rules::Posix),
    ("sshfs", Quirks::sshfs, Rules::Posix),
    ("vfat", Quirks::vfat, Rules::Bare),
    ("exfat", Quirks::exfat, Rules::Bare),
    ("exfat-fuse", Quirks::exfat_fuse, Rules::Bare),
    ("ntfs3", Quirks::ntfs3, Rules::Bare),
    (
        "ntfs3 +windows_names",
        Quirks::ntfs3_windows_names,
        Rules::Full,
    ),
    ("ntfs-3g", Quirks::ntfs_3g, Rules::Full),
    ("cifs", Quirks::cifs, Rules::Smb),
    ("cifs-nocase", Quirks::cifs_nocase, Rules::SmbNocase),
    ("cifs-posix", Quirks::cifs_posix, Rules::SmbPosix),
];

struct Row {
    name: OsString,
    folder: bool,
    /// The name each kind of mount gets instead, where it gets another.
    fixed: Vec<(&'static [Rules], String, Cause)>,
}

impl Row {
    fn expected(&self, rules: Rules) -> (&OsStr, Option<Cause>) {
        self.fixed
            .iter()
            .find(|(applies, ..)| applies.contains(&rules))
            .map_or((self.name.as_os_str(), None), |(_, name, cause)| {
                (OsStr::new(name), Some(*cause))
            })
    }
}

fn row(name: impl AsRef<[u8]>, folder: bool, fixed: &[(&'static [Rules], &str, Cause)]) -> Row {
    Row {
        name: OsString::from_vec(name.as_ref().to_vec()),
        folder,
        fixed: fixed
            .iter()
            .map(|(rules, name, cause)| (*rules, (*name).to_owned(), *cause))
            .collect(),
    }
}

fn file(name: impl AsRef<[u8]>, fixed: &[(&'static [Rules], &str, Cause)]) -> Row {
    row(name, false, fixed)
}

/// Every name the fslab mounts were probed with, and more device names.
fn lab_corpus() -> Vec<Row> {
    use Cause::{Invalid, TooLong};
    use Rules::{AnyGvfs, Bare, Full, GvfsSmb, Mtp, Posix, Smb, SmbNocase, SmbPosix};
    const WINDOWS: &[Rules] = &[Bare, Full, Smb, SmbNocase, Mtp, GvfsSmb, AnyGvfs];
    const FULL: &[Rules] = &[Full, Smb, SmbNocase, Mtp, GvfsSmb, AnyGvfs];
    const MAPPED: &[Rules] = &[Smb, SmbNocase];
    const UTF8: &[Rules] = &[Bare, Full, Smb, SmbNocase, SmbPosix, Mtp, GvfsSmb, AnyGvfs];
    const ALL_BUT_MTP: &[Rules] = &[Posix, Bare, Full, Smb, SmbNocase, SmbPosix, GvfsSmb];
    const BYTES: &[Rules] = &[Posix, Smb, SmbNocase, SmbPosix, Mtp, GvfsSmb, AnyGvfs];
    const ASTRAL_BYTES: &[Rules] = &[Posix, Smb, SmbPosix, Mtp, GvfsSmb, AnyGvfs];
    const UNITS: &[Rules] = &[Bare, Full];
    const NOCASE: &[Rules] = &[SmbNocase];
    const MTP: &[Rules] = &[Mtp, AnyGvfs];
    const SAMBA_FOLDERS: &[Rules] = &[Smb, SmbNocase, SmbPosix, GvfsSmb, AnyGvfs];
    let repeat = |text: &str, count| text.repeat(count);
    let (cjk, emoji) = ("\u{4e2d}", "\u{1f600}");

    let mut rows = Vec::new();
    for forbidden in ['\\', ':', '*', '?', '"', '<', '>', '|'] {
        rows.push(file(format!("x{forbidden}y"), &[(WINDOWS, "x_y", Invalid)]));
    }
    for control in 1..0x20 {
        rows.push(file([b'x', control, b'y'], &[(WINDOWS, "x_y", Invalid)]));
    }
    rows.push(file("x\u{7f}y", &[]));
    for trailing in ["x.", "x..", "x ", "x. ", "x ."] {
        rows.push(file(trailing, &[(WINDOWS, "x", Invalid)]));
    }
    rows.extend([
        file("...", &[(WINDOWS, "_", Invalid)]),
        file(" ", &[(WINDOWS, "_", Invalid)]),
        file(" x", &[(MTP, "x", Invalid)]),
        file(" .x", &[]),
        file(".x", &[]),
        file("CON", &[(WINDOWS, "CON_", Invalid)]),
        file("con", &[(WINDOWS, "con_", Invalid)]),
        file("CON.txt", &[(FULL, "CON_.txt", Invalid)]),
        file("NUL", &[(WINDOWS, "NUL_", Invalid)]),
        file("COM1", &[(WINDOWS, "COM1_", Invalid)]),
        file("LPT9.log", &[(FULL, "LPT9_.log", Invalid)]),
        file("CONIN$", &[(WINDOWS, "CONIN$_", Invalid)]),
        file("COM\u{b9}", &[(WINDOWS, "COM\u{b9}_", Invalid)]),
        file("PRN.tar.gz", &[(FULL, "PRN_.tar.gz", Invalid)]),
        file("NUL.txt", &[(WINDOWS, "NUL_.txt", Invalid)]),
        file("aux.c", &[(FULL, "aux_.c", Invalid)]),
        file("CON .txt", &[(FULL, "CON_ .txt", Invalid)]),
        file("aux.. ..", &[(WINDOWS, "aux_", Invalid)]),
        file(repeat("a", 255), &[(MTP, &repeat("a", 254), TooLong)]),
        file(
            repeat("a", 256),
            &[
                (MTP, &repeat("a", 254), TooLong),
                (ALL_BUT_MTP, &repeat("a", 255), TooLong),
            ],
        ),
        file(repeat(cjk, 85), &[]),
        file(repeat(cjk, 86), &[(BYTES, &repeat(cjk, 85), TooLong)]),
        file(repeat(cjk, 255), &[(BYTES, &repeat(cjk, 85), TooLong)]),
        file(
            repeat(cjk, 256),
            &[
                (BYTES, &repeat(cjk, 85), TooLong),
                (UNITS, &repeat(cjk, 255), TooLong),
            ],
        ),
        file(repeat(emoji, 63), &[(NOCASE, &repeat("_", 63), Invalid)]),
        file(
            repeat(emoji, 64),
            &[
                (ASTRAL_BYTES, &repeat(emoji, 63), TooLong),
                (NOCASE, &repeat("_", 64), Invalid),
            ],
        ),
        file(
            format!("{}a", repeat(emoji, 127)),
            &[
                (ASTRAL_BYTES, &repeat(emoji, 63), TooLong),
                (NOCASE, &format!("{}a", repeat("_", 127)), Invalid),
            ],
        ),
        file(
            repeat(emoji, 128),
            &[
                (ASTRAL_BYTES, &repeat(emoji, 63), TooLong),
                (NOCASE, &repeat("_", 128), Invalid),
                (UNITS, &repeat(emoji, 127), TooLong),
            ],
        ),
        file(b"x\xffy", &[(UTF8, "x_y", Invalid)]),
        file(b"x\x80y", &[(UTF8, "x_y", Invalid)]),
        file(b"x\xc0\xafy", &[(UTF8, "x__y", Invalid)]),
        file(b"x\xed\xa0\x80y", &[(UTF8, "x___y", Invalid)]),
        file(b"x\xe4\xb8y", &[(UTF8, "x_y", Invalid)]),
        file("caf\u{e9}", &[]),
        file("cafe\u{301}", &[]),
        file("x\u{f022}y", &[(MAPPED, "x_y", Invalid)]),
        file("x\u{f001}y", &[(MAPPED, "x_y", Invalid)]),
        file("x\u{f029}y", &[(MAPPED, "x_y", Invalid)]),
        file("x\u{f026}y", &[]),
        file("x\u{f03a}y", &[]),
        file("x\u{ff1a}y", &[]),
        file(format!("a{emoji}b"), &[(NOCASE, "a_b", Invalid)]),
        file("\u{1d400}", &[(NOCASE, "_", Invalid)]),
        row(
            repeat("a", 255),
            true,
            &[
                (SAMBA_FOLDERS, &repeat("a", 213), TooLong),
                (MTP, &repeat("a", 254), TooLong),
            ],
        ),
        row(
            format!("{}.txt", repeat("x", 250)),
            true,
            &[(SAMBA_FOLDERS, &repeat("x", 213), TooLong)],
        ),
        file(format!("{}.txt", repeat("x", 250)), &[]),
    ]);
    rows
}

#[test]
fn each_mount_gets_a_name_it_stores_as_is_for_every_lab_name() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let corpus = lab_corpus();
    for (index, (mount, preset, rules)) in LAB_MOUNTS.into_iter().enumerate() {
        let quirk_root = root.path().join(index.to_string());
        fs::create_dir(&quirk_root)?;
        let _quirks = FsQuirks::install(&quirk_root, preset());
        let profile = Profiles::default()
            .of(fs(), open_local_parent_directory(&quirk_root)?.as_fd())
            .names;
        for (case, row) in corpus.iter().enumerate() {
            let fixed = fix_name(&row.name, &profile, row.folder);
            let context = format!("{mount}: {:?}", row.name);
            assert_eq!(
                (fixed.name.as_ref(), fixed.cause),
                row.expected(rules),
                "{context}"
            );
            assert_eq!(
                fix_name(&fixed.name, &profile, row.folder).name,
                fixed.name,
                "{context}"
            );
            // The root's own filesystem stops at 255 bytes, which UTF-16 mounts pass.
            if fixed.name.len() > 255 {
                continue;
            }
            let folder = quirk_root.join(case.to_string());
            fs::create_dir(&folder)?;
            let parent = open_local_parent_directory(&folder)?;
            let at = Entry::new(&parent, &fixed.name);
            let created = if row.folder {
                fs().mkdir(at, Mode::from_raw_mode(0o755))
            } else {
                fs().create_exclusive(at, Mode::from_raw_mode(0o644))
                    .map(drop)
            };
            assert_eq!(created, Ok(()), "{context}");
            let listed = fs::read_dir(&folder)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(listed, [fixed.name.into_owned()], "{context}");
        }
    }
    for (backend, profile, rules) in [
        ("mtp", gvfs_profile("mtp"), Rules::Mtp),
        ("smb", gvfs_profile("smb"), Rules::GvfsSmb),
        ("sftp", gvfs_profile("sftp"), Rules::Posix),
        ("unknown", gvfs_bridge_profile(None), Rules::AnyGvfs),
    ] {
        for row in &corpus {
            let fixed = fix_name(&row.name, &profile, row.folder);
            assert_eq!(
                (fixed.name.as_ref(), fixed.cause),
                row.expected(rules),
                "{backend}: {:?}",
                row.name
            );
        }
    }
    Ok(())
}

/// What a lab mount did with a name it was given as is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lab {
    Stored,
    Listed(&'static str),
    /// Listed under a Samba 8.3 alias.
    Mangled,
    Refused(Errno),
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    File,
    Folder,
    Symlink,
}

/// The lab's cells, in the order of [`LAB_MOUNTS`]: ext4, nfs42, nfs3,
/// sshfs, vfat, exfat, exfat-fuse, ntfs3, ntfs3 +windows_names, ntfs-3g,
/// cifs, cifs-nocase, cifs-posix. exfat-fuse stored overlong and surrogate
/// names and then failed; its preset refuses them with `EUCLEAN`.
fn lab_cells() -> Vec<(Vec<u8>, Kind, [Lab; 13])> {
    use Kind::{File, Folder, Symlink};
    use Lab::{Listed, Mangled, Stored as OK};
    let (inval, noent, ilseq) = (
        Lab::Refused(Errno::INVAL),
        Lab::Refused(Errno::NOENT),
        Lab::Refused(Errno::ILSEQ),
    );
    let windows = [
        OK, OK, OK, OK, inval, inval, noent, OK, inval, inval, OK, OK, OK,
    ];
    let reserved = [
        OK, OK, OK, OK, OK, OK, OK, OK, inval, inval, Mangled, Mangled, OK,
    ];
    let unreserved = [OK, OK, OK, OK, OK, OK, OK, OK, inval, inval, OK, OK, OK];
    let malformed = |fuse: Lab, ntfs_3g: Lab, posix: &'static str| {
        [
            OK,
            OK,
            OK,
            OK,
            inval,
            ilseq,
            fuse,
            inval,
            inval,
            ntfs_3g,
            noent,
            inval,
            Listed(posix),
        ]
    };
    let corrupts = Lab::Refused(Errno::UCLEAN);
    let mut rows = Vec::new();
    for forbidden in ":*?\"<>|".chars() {
        rows.push((format!("x{forbidden}y").into_bytes(), File, windows));
    }
    let mut backslash = windows;
    backslash[10] = inval;
    backslash[11] = inval;
    rows.push((b"x\\y".to_vec(), File, backslash));
    rows.push((b"x\x01y".to_vec(), File, windows));
    rows.push((b"x\x1fy".to_vec(), File, windows));
    rows.extend([
        (b"x\x7fy".to_vec(), File, [OK; 13]),
        (b" x".to_vec(), File, [OK; 13]),
        (b".x".to_vec(), File, [OK; 13]),
        (
            b"x.".to_vec(),
            File,
            [
                OK,
                OK,
                OK,
                OK,
                Listed("x"),
                Listed("x"),
                OK,
                OK,
                inval,
                inval,
                OK,
                OK,
                OK,
            ],
        ),
        (
            b"x ".to_vec(),
            File,
            [OK, OK, OK, OK, inval, OK, OK, OK, inval, inval, OK, OK, OK],
        ),
        (
            b"x .".to_vec(),
            File,
            [
                OK,
                OK,
                OK,
                OK,
                inval,
                Listed("x "),
                OK,
                OK,
                inval,
                inval,
                OK,
                OK,
                OK,
            ],
        ),
        (
            b"...".to_vec(),
            File,
            [
                OK, OK, OK, OK, noent, noent, OK, OK, inval, inval, OK, OK, OK,
            ],
        ),
        (b"CON".to_vec(), File, reserved),
        (b"con".to_vec(), File, reserved),
        (b"CON.txt".to_vec(), File, reserved),
        (b"NUL".to_vec(), File, reserved),
        (b"COM1".to_vec(), File, reserved),
        (b"PRN.tar.gz".to_vec(), File, reserved),
        (b"LPT9.log".to_vec(), File, unreserved),
        (b"CONIN$".to_vec(), File, [OK; 13]),
        ("COM\u{b9}".as_bytes().to_vec(), File, [OK; 13]),
        (b"x\xffy".to_vec(), File, malformed(ilseq, ilseq, "x?y")),
        (b"x\x80y".to_vec(), File, malformed(ilseq, ilseq, "x?y")),
        (
            b"x\xc0\xafy".to_vec(),
            File,
            malformed(corrupts, ilseq, "x??y"),
        ),
        (
            b"x\xed\xa0\x80y".to_vec(),
            File,
            malformed(corrupts, OK, "x???y"),
        ),
        ("cafe\u{301}".as_bytes().to_vec(), File, [OK; 13]),
        (
            "x\u{f022}y".as_bytes().to_vec(),
            File,
            [
                OK,
                OK,
                OK,
                OK,
                OK,
                OK,
                OK,
                OK,
                OK,
                OK,
                Listed("x:y"),
                Listed("x:y"),
                OK,
            ],
        ),
        ("x\u{f03a}y".as_bytes().to_vec(), File, [OK; 13]),
        ("x\u{ff1a}y".as_bytes().to_vec(), File, [OK; 13]),
        (
            "\u{1f600}".repeat(63).into_bytes(),
            File,
            [OK, OK, OK, OK, OK, OK, OK, OK, OK, OK, OK, inval, OK],
        ),
        (vec![b'a'; 255], File, [OK; 13]),
        (vec![b'a'; 217], Folder, [OK; 13]),
        (
            vec![b'a'; 218],
            Folder,
            [OK, OK, OK, OK, OK, OK, OK, OK, OK, OK, noent, noent, noent],
        ),
        (
            vec![b'a'; 256],
            File,
            [
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::BADMSG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
                Lab::Refused(Errno::NAMETOOLONG),
            ],
        ),
        (
            b"s".to_vec(),
            Symlink,
            [
                OK,
                OK,
                OK,
                OK,
                Lab::Refused(Errno::PERM),
                Lab::Refused(Errno::PERM),
                Lab::Refused(Errno::NOSYS),
                OK,
                OK,
                OK,
                Lab::Refused(Errno::OPNOTSUPP),
                Lab::Refused(Errno::OPNOTSUPP),
                OK,
            ],
        ),
    ]);
    rows
}

#[test]
fn each_preset_does_with_a_raw_name_what_its_lab_mount_did() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let rows = lab_cells();
    for (column, (mount, preset, _)) in LAB_MOUNTS.into_iter().enumerate() {
        let quirk_root = root.path().join(column.to_string());
        fs::create_dir(&quirk_root)?;
        let _quirks = FsQuirks::install(&quirk_root, preset());
        for (case, (name, kind, cells)) in rows.iter().enumerate() {
            let name = std::os::unix::ffi::OsStrExt::from_bytes(name.as_slice());
            let context = format!("{mount}: {name:?}");
            let folder = quirk_root.join(case.to_string());
            fs::create_dir(&folder)?;
            let parent = open_local_parent_directory(&folder)?;
            let at = Entry::new(&parent, name);
            let created = match kind {
                Kind::File => fs()
                    .create_exclusive(at, Mode::from_raw_mode(0o644))
                    .map(drop),
                Kind::Folder => fs().mkdir(at, Mode::from_raw_mode(0o755)),
                Kind::Symlink => fs().symlink(OsStr::new("t"), at),
            };
            let listed = fs::read_dir(&folder)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<Vec<_>, _>>()?;
            let outcome = match (created, listed.as_slice()) {
                (Err(error), []) => Lab::Refused(error),
                (Ok(()), [listed]) if listed == name => Lab::Stored,
                (Ok(()), [listed]) if cells[column] == Lab::Mangled => {
                    assert!(listed.as_encoded_bytes().contains(&b'~'), "{context}");
                    Lab::Mangled
                }
                (Ok(()), [listed]) => match cells[column] {
                    Lab::Listed(expected) if listed == expected => cells[column],
                    _ => panic!("{context}: listed as {listed:?}"),
                },
                (created, listed) => panic!("{context}: {created:?}, listed {listed:?}"),
            };
            assert_eq!(outcome, cells[column], "{context}");
        }
    }
    Ok(())
}

#[test]
fn cut_names_keep_their_extension_and_whole_clusters() {
    let bytes = NameProfile::posix(Some(255));
    let units = NameProfile::windows(Reserved::Bare, NameEncoding::Utf8);
    let full = NameProfile::windows(Reserved::Full, NameEncoding::Utf8);
    let narrow = NameProfile::posix(Some(64));
    let repeat = |text: &str, count| text.repeat(count);
    let hangul = "\u{1112}\u{1161}\u{11ab}";
    let flag = "\u{1f1eb}\u{1f1f7}";
    let conjunct = "\u{915}\u{94d}\u{937}";
    let scotland = "\u{1f3f4}\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}";
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    let heart = "\u{2764}\u{fe0f}";
    // Pango keeps consonants joined by a virama together from 1.54 on.
    let joins_conjuncts = gtk::pango::version_check(1, 54, 0).is_none();
    for (name, profile, folder, expected) in [
        (
            format!("abx{}.txt", repeat("\u{301}", 600)),
            units,
            false,
            format!("abx{}.txt", repeat("\u{301}", 248)),
        ),
        (
            format!("{}.txt", repeat(hangul, 40)),
            bytes,
            false,
            format!("{}.txt", repeat(hangul, 27)),
        ),
        (repeat(flag, 40), bytes, false, repeat(flag, 31)),
        (
            format!("abcd{}", repeat(conjunct, 40)),
            bytes,
            false,
            format!("abcd{}", repeat(conjunct, 27)),
        ),
        (repeat(scotland, 20), units, false, repeat(scotland, 18)),
        (
            format!("{}{}", repeat("a", 20), repeat(family, 12)),
            bytes,
            false,
            format!("{}{}", repeat("a", 20), repeat(family, 9)),
        ),
        (
            format!("aaaaaaa{}", repeat(scotland, 9)),
            bytes,
            false,
            format!("aaaaaaa{}", repeat(scotland, 8)),
        ),
        (
            format!("{}x{}", repeat("a", 230), repeat("\u{301}", 20)),
            bytes,
            false,
            format!("{}x{}", repeat("a", 230), repeat("\u{301}", 12)),
        ),
        (repeat(heart, 50), bytes, false, repeat(heart, 42)),
        (
            format!("{}.tar.gz", repeat("a", 300)),
            bytes,
            false,
            format!("{}.tar.gz", repeat("a", 248)),
        ),
        (
            format!("x.{}", repeat("a", 300)),
            bytes,
            false,
            format!("x.{}", repeat("a", 253)),
        ),
        (
            format!("{}.txt", repeat("a", 300)),
            bytes,
            true,
            repeat("a", 255),
        ),
        (
            format!("\u{1f3f4}{}.{}", repeat("\u{e0067}", 12), repeat("a", 31)),
            narrow,
            false,
            format!("\u{1f3f4}{}.{}", repeat("\u{e0067}", 7), repeat("a", 31)),
        ),
        (
            format!("CON{}x", repeat(" ", 300)),
            units,
            false,
            "CON_".to_owned(),
        ),
        (
            format!("CON{}x.txt", repeat(" ", 248)),
            full,
            false,
            format!("CON_{}.txt", repeat(" ", 247)),
        ),
    ] {
        if name.contains(conjunct) && !joins_conjuncts {
            continue;
        }
        let fixed = fix_name(OsStr::new(&name), &profile, folder);
        assert_eq!(
            (fixed.name.as_ref(), fixed.cause),
            (OsStr::new(&expected), Some(Cause::TooLong)),
            "{name:?}"
        );
        assert_eq!(
            fix_name(&fixed.name, &profile, folder).name,
            fixed.name,
            "{name:?}"
        );
    }
}

#[test]
fn device_names_get_a_mark_where_windows_reserves_them() {
    let bare = NameProfile::windows(Reserved::Bare, NameEncoding::Utf8);
    let full = NameProfile::windows(Reserved::Full, NameEncoding::Utf8);
    let ports = [
        "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "\u{b9}", "\u{b2}", "\u{b3}",
    ];
    let devices = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
        .map(String::from)
        .into_iter()
        .chain(
            ["COM", "LPT"]
                .into_iter()
                .flat_map(|port| ports.map(|number| format!("{port}{number}"))),
        );
    for device in devices {
        for word in [device.clone(), device.to_lowercase()] {
            let dotted = format!("{word}.txt");
            let marked = format!("{word}_.txt");
            let bare_dotted = if device == "NUL" { &marked } else { &dotted };
            for (profile, name, expected) in [
                (bare, &word, &format!("{word}_")),
                (bare, &dotted, bare_dotted),
                (full, &dotted, &marked),
            ] {
                let fixed = fix_name(OsStr::new(name), &profile, false).name;
                assert_eq!(fixed, OsStr::new(expected), "{name} {:?}", profile.reserved);
            }
        }
    }
    for name in ["COM10", "LPT", "CONOUT", "AUXX", "NULL.txt", "CON_"] {
        assert_eq!(
            fix_name(OsStr::new(name), &full, false).cause,
            None,
            "{name}"
        );
    }
}

#[test]
fn only_the_characters_an_smb_mapping_reads_back_are_replaced() {
    for (mapped, replaced, kept) in [
        (
            PrivateUse::Sfm,
            &[
                '\u{f001}', '\u{f01f}', '\u{f020}', '\u{f022}', '\u{f025}', '\u{f027}', '\u{f029}',
            ][..],
            &['\u{f000}', '\u{f026}', '\u{f02a}', '\u{f03a}'][..],
        ),
        (
            PrivateUse::Sfu,
            &[
                '\u{f02a}', '\u{f03a}', '\u{f03c}', '\u{f03e}', '\u{f03f}', '\u{f07c}',
            ][..],
            &['\u{f001}', '\u{f022}', '\u{f029}', '\u{f05c}'][..],
        ),
    ] {
        let profile = NameProfile {
            mapped: Some(mapped),
            ..NameProfile::smb(Some(255))
        };
        for (characters, changed) in [(replaced, true), (kept, false)] {
            for character in characters {
                let name = format!("x{character}y");
                let fixed = fix_name(OsStr::new(&name), &profile, false).name;
                let expected = if changed { "x_y" } else { name.as_str() };
                assert_eq!(fixed, OsStr::new(expected), "{mapped:?} {name:?}");
            }
        }
    }
}

#[test]
fn fixing_a_fixed_name_changes_nothing() {
    let starts: [&[u8]; 6] = [b"a", b" ", b"CON", b"nul", b"\xff", "\u{4e2d}".as_bytes()];
    let runs: [&[u8]; 14] = [
        b"a",
        b" ",
        b".",
        b":",
        b"\x01",
        b"\xed\xa0",
        "\u{4e2d}".as_bytes(),
        "\u{1f600}".as_bytes(),
        "\u{301}".as_bytes(),
        "\u{200d}".as_bytes(),
        "\u{1f1eb}".as_bytes(),
        "\u{f022}".as_bytes(),
        "\u{915}\u{94d}".as_bytes(),
        b"CON",
    ];
    let ends: [&[u8]; 5] = [b"", b".txt", b".tar.gz", b" .", "\u{301}.c".as_bytes()];
    let profiles = [
        NameProfile::posix(Some(255)),
        NameProfile::posix(Some(64)),
        NameProfile::windows(Reserved::Bare, NameEncoding::Utf8),
        NameProfile::windows(Reserved::Full, NameEncoding::Utf8Bmp),
        NameProfile::windows(Reserved::Bare, NameEncoding::Ascii),
        NameProfile {
            mapped: Some(PrivateUse::Sfm),
            ..NameProfile::smb(Some(255))
        },
        NameProfile::smb_posix(Some(255)),
        gvfs_profile("mtp"),
    ];
    let mut state = 0x9e37_79b9_u32;
    let mut next = |below: usize| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as usize % below
    };
    for _ in 0..2000 {
        let mut name = starts[next(starts.len())].to_vec();
        for _ in 0..=next(3) {
            let run = runs[next(runs.len())];
            for _ in 0..=next(120) {
                name.extend_from_slice(run);
            }
        }
        name.extend_from_slice(ends[next(ends.len())]);
        let name = OsString::from_vec(name);
        let folder = next(2) == 0;
        for profile in &profiles {
            let fixed = fix_name(&name, profile, folder).name;
            assert_eq!(
                fix_name(&fixed, profile, folder).name,
                fixed,
                "{name:?} {profile:?}"
            );
            let limit = profile.limit;
            let bytes = limit
                .folder_bytes
                .filter(|_| folder)
                .into_iter()
                .chain(limit.bytes)
                .min();
            assert!(
                bytes.is_none_or(|bytes| fixed.len() <= usize::from(bytes)),
                "{name:?} {profile:?}"
            );
            if profile.encoding != NameEncoding::AnyBytes {
                let utf8 = fixed.to_str();
                assert!(utf8.is_some(), "{name:?} {profile:?}");
                let held =
                    utf8.unwrap_or_default()
                        .chars()
                        .all(|character| match profile.encoding {
                            NameEncoding::Ascii => character.is_ascii(),
                            NameEncoding::Utf8Bmp => character <= '\u{ffff}',
                            NameEncoding::AnyBytes | NameEncoding::Utf8 => true,
                        });
                assert!(held, "{name:?} {profile:?}");
                let units = utf8.map_or(0, |fixed| fixed.encode_utf16().count());
                assert!(
                    limit.units.is_none_or(|limit| units <= usize::from(limit)),
                    "{name:?} {profile:?}"
                );
            }
            assert!(
                !fixed.as_encoded_bytes().starts_with(b"."),
                "{name:?} {profile:?}"
            );
        }
    }
}
