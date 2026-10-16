// SPDX-License-Identifier: MIT

use std::os::unix::fs::MetadataExt;

use super::fs_quirks::{Op, Racer};
use super::*;
use crate::services::{RenameCause, RenamedItem};

fn copy_renaming(
    source: &Path,
    target: &Path,
    workers: Option<usize>,
) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let relative = |location: &Location| {
        location
            .native_path()
            .and_then(|path| path.strip_prefix(target).ok())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Ok(copy_reporting(source, target, workers)?
        .renamed
        .into_iter()
        .map(
            |RenamedItem {
                 requested,
                 saved_as,
                 cause,
             }| {
                assert_eq!(cause, RenameCause::Alias);
                (relative(&requested), relative(&saved_as))
            },
        )
        .collect())
}

#[test]
fn case_aliases_inside_a_copied_folder_are_numbered_and_reported() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    // Names are claimed in byte order where they may collide, so the later
    // one is numbered; elsewhere the one made last is.
    let names_never_collide = || Quirks {
        case_insensitive: true,
        ..Quirks::ext4()
    };
    // ciopfs folds case like FAT, and holds links, which FAT cannot.
    let claimed = || Quirks {
        mount_type: Some(("fuse.ciopfs", "rw")),
        case_insensitive: true,
        ..Quirks::default()
    };
    let rows: [(&str, Preset, &str, &str); 2] = [
        ("claimed", claimed, "makefile", "makefile (2)"),
        ("made", names_never_collide, "Makefile", "Makefile (2)"),
    ];
    for (row, preset, aliased, saved_as) in rows {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir_all(source.join("Docs"))?;
        fs::create_dir_all(source.join("docs"))?;
        fs::create_dir(&destination)?;
        fs::write(source.join("Docs/upper.txt"), b"upper")?;
        fs::write(source.join("docs/lower.txt"), b"lower")?;
        for name in ["Makefile", "makefile", "makefile (1)"] {
            fs::write(source.join(name), name)?;
        }
        std::os::unix::fs::symlink("upper", source.join("Link"))?;
        std::os::unix::fs::symlink("lower", source.join("link"))?;
        let _sources = FsQuirks::install(&source, Quirks::default());
        let _destination = FsQuirks::install(&destination, preset());
        let target = destination.join("copy");

        let renamed = copy_renaming(&source, &target, Some(1))?;

        let mut expected = vec![
            ("docs".to_owned(), "docs (1)".to_owned()),
            ("link".to_owned(), "link (1)".to_owned()),
            (aliased.to_owned(), saved_as.to_owned()),
        ];
        expected.sort();
        let mut renamed = renamed;
        renamed.sort();
        assert_eq!(renamed, expected, "{row}");
        let other = if aliased == "makefile" {
            "Makefile"
        } else {
            "makefile"
        };
        let mut expected = vec!["Docs", "Link", "docs (1)", "link (1)", "makefile (1)"];
        expected.extend([other, saved_as]);
        expected.sort_unstable();
        assert_eq!(names(&target)?, expected, "{row}");
        assert_eq!(fs::read_to_string(target.join(saved_as))?, aliased, "{row}");
        assert_eq!(fs::read_to_string(target.join(other))?, other, "{row}");
        assert_eq!(
            fs::read_to_string(target.join("makefile (1)"))?,
            "makefile (1)",
            "{row}: the sibling keeps its own name"
        );
        assert_eq!(fs::read(target.join("Docs/upper.txt"))?, b"upper", "{row}");
        assert_eq!(
            fs::read(target.join("docs (1)/lower.txt"))?,
            b"lower",
            "{row}"
        );
        assert_eq!(fs::read_link(target.join("Link"))?, Path::new("upper"));
        assert_eq!(fs::read_link(target.join("link (1)"))?, Path::new("lower"));
    }
    Ok(())
}

#[test]
fn aliases_of_long_names_are_numbered_within_the_limit() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let names_never_collide = || Quirks {
        case_insensitive: true,
        ..Quirks::ext4()
    };
    let claimed = || Quirks {
        mount_type: Some(("fuse.ciopfs", "rw")),
        case_insensitive: true,
        ..Quirks::default()
    };
    let (upper, lower) = (
        format!("A{}.txt", "a".repeat(250)),
        format!("{}.txt", "a".repeat(251)),
    );
    let rows: [(&str, Preset, &str, String); 2] = [
        (
            "claimed",
            claimed,
            &lower,
            format!("{} (1).txt", "a".repeat(247)),
        ),
        (
            "made",
            names_never_collide,
            &upper,
            format!("A{} (1).txt", "a".repeat(246)),
        ),
    ];
    for (row, preset, aliased, saved_as) in rows {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source)?;
        fs::create_dir(&destination)?;
        fs::write(source.join(&upper), "upper")?;
        fs::write(source.join(&lower), "lower")?;
        let _destination = FsQuirks::install(&destination, preset());
        let target = destination.join("copy");

        let renamed = copy_renaming(&source, &target, Some(1))?;

        assert_eq!(renamed, [(aliased.to_owned(), saved_as.clone())], "{row}");
        let kept = if aliased == upper { &lower } else { &upper };
        let mut expected = vec![kept.clone(), saved_as.clone()];
        expected.sort();
        assert_eq!(names(&target)?, expected, "{row}");
    }
    Ok(())
}

#[test]
fn a_linked_name_that_aliases_another_is_numbered_and_reported() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(&source)?;
    fs::create_dir(&destination)?;
    fs::write(source.join("a"), b"linked")?;
    fs::hard_link(source.join("a"), source.join("B"))?;
    fs::write(source.join("b"), b"other")?;
    let _sources = FsQuirks::install(&source, Quirks::default());
    let _destination = FsQuirks::install(
        &destination,
        Quirks {
            case_insensitive: true,
            ..Quirks::ext4()
        },
    );
    let target = destination.join("copy");

    // One worker takes the folder's files last first, so `B` is linked
    // after `b` is made.
    let renamed = copy_renaming(&source, &target, Some(1))?;

    assert_eq!(renamed, [("B".to_owned(), "B (1)".to_owned())]);
    assert_eq!(names(&target)?, ["B (1)", "a", "b"]);
    assert_eq!(
        fs::metadata(target.join("B (1)"))?.ino(),
        fs::metadata(target.join("a"))?.ino()
    );
    assert_eq!(fs::read(target.join("b"))?, b"other");
    Ok(())
}

#[test]
fn two_names_are_never_taken_for_one_where_the_listing_changes_their_case()
-> Result<(), Box<dyn Error>> {
    use super::fs_quirks::{Op, Racer, renumbered};
    use crate::services::{PasteItem, PasteRequest};
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    // ciopfs lists `Makefile` as `makefile` once it is made, as Samba with
    // `preserve case = no` does; a racer renames it so. Where inode numbers
    // are not kept, the entry then reports another number. FAT and SMB plan
    // such names apart before.
    let rows = [
        ("copy", false, false),
        ("move", true, false),
        ("unstable inodes", false, true),
    ];
    for (row, moving, renumber) in rows {
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let folder = sources.join("folder");
        let destination = root.path().join("destination");
        fs::create_dir_all(&folder)?;
        fs::create_dir(&destination)?;
        fs::write(folder.join("Makefile"), b"upper")?;
        fs::write(folder.join("makefile"), b"lower")?;
        let live = destination.clone();
        let racer: Racer = Box::new(move || {
            let stage = names(&live)
                .expect("destination")
                .into_iter()
                .find(|name| name.starts_with(".strata-copy-"))
                .expect("stage");
            let lowered = live.join(stage).join("makefile");
            fs::rename(lowered.with_file_name("Makefile"), &lowered).expect("lower the name");
            if renumber {
                renumbered(&lowered);
            }
        });
        let _sources = FsQuirks::install(&sources, Quirks::default());
        let _destination = FsQuirks::install(
            &destination,
            Quirks {
                mount_type: Some(("fuse.ciopfs", "rw")),
                case_insensitive: true,
                before: vec![(Op::CreateNode, "makefile", 0, racer)],
                ..Quirks::default()
            },
        );

        let events = run_paste(PasteRequest {
            id: OperationRequestId(1),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&folder),
                conflict: TransferConflict::FailIfExists,
                target_name: None,
            }],
            move_sources: moving,
        });

        let copied = destination.join("folder");
        let renamed = events
            .iter()
            .filter_map(|event| match event {
                OperationEvent::ItemsReported { report, .. } => Some(report.renamed.clone()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(
            renamed,
            [RenamedItem {
                requested: Location::local(copied.join("makefile")),
                saved_as: Location::local(copied.join("makefile (1)")),
                cause: RenameCause::Alias,
            }],
            "{row}"
        );
        assert_eq!(names(&copied)?, ["makefile", "makefile (1)"], "{row}");
        assert_eq!(fs::read(copied.join("makefile"))?, b"upper", "{row}");
        assert_eq!(fs::read(copied.join("makefile (1)"))?, b"lower", "{row}");
        assert_eq!(folder.exists(), !moving, "{row}");
    }
    Ok(())
}

#[test]
fn names_a_fat_target_takes_for_one_are_numbered_and_reported() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(&source)?;
    fs::create_dir(&destination)?;
    // `a:b` would be saved as `a_b`, which the real `a_b` keeps.
    for name in ["Makefile", "a:b", "a_b", "makefile"] {
        fs::write(source.join(name), name)?;
    }
    let _sources = FsQuirks::install(&source, Quirks::default());
    let _destination = FsQuirks::install(&destination, Quirks::vfat());
    let target = destination.join("copy");
    let options = super::super::copy_engine::Options {
        names: NamePolicy::new(NameMode::Fix),
        ..super::super::copy_engine::Options::default()
    };

    let (copied, report) = reporting(
        &source,
        super::super::copy_engine::Destination::New(target.clone(), Taken::Fail),
        options,
    );

    copied?;
    assert_eq!(
        renamed_beneath(&target, &report),
        [
            ("a:b", "a_b (1)", RenameCause::Invalid),
            ("makefile", "makefile (1)", RenameCause::Alias),
        ]
        .map(|(name, saved_as, cause)| (name.to_owned(), saved_as.to_owned(), cause))
    );
    assert_eq!(
        names(&target)?,
        ["Makefile", "a_b", "a_b (1)", "makefile (1)"]
    );
    assert_eq!(fs::read(target.join("a_b"))?, b"a_b");
    assert_eq!(fs::read(target.join("a_b (1)"))?, b"a:b");
    assert_eq!(fs::read(target.join("makefile (1)"))?, b"makefile");
    Ok(())
}

#[test]
fn top_level_items_a_destination_takes_for_an_earlier_one_are_numbered_and_reported()
-> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let casefolded = || Quirks {
        case_insensitive: true,
        ..Quirks::ext4()
    };
    // Past eight files a paste copies several at once.
    let padding = (0..8)
        .map(|index| format!("pad {index}"))
        .collect::<Vec<_>>();
    for (mount, preset) in [
        ("casefolded ext4", casefolded as Preset),
        ("cifs", Quirks::cifs),
    ] {
        for moving in [false, true] {
            for parallel in [false, true] {
                let context = format!("{mount}, moving: {moving}, parallel: {parallel}");
                let root = tempfile::tempdir()?;
                let source = root.path().join("source");
                let destination = root.path().join("destination");
                fs::create_dir_all(&source)?;
                fs::create_dir(&destination)?;
                let mut items = vec!["Makefile", "makefile"];
                if parallel {
                    items.extend(padding.iter().map(String::as_str));
                } else {
                    items.extend(["Docs", "docs"]);
                }
                for name in &items {
                    if name.ends_with("ocs") {
                        fs::create_dir(source.join(name))?;
                        fs::write(source.join(name).join("inside"), name)?;
                    } else {
                        fs::write(source.join(name), name)?;
                    }
                }
                let _destination = FsQuirks::install(&destination, preset());

                let events = run_paste(PasteRequest {
                    id: OperationRequestId(1530),
                    destination: Location::local(&destination),
                    items: items
                        .iter()
                        .map(|name| PasteItem {
                            source: Location::local(source.join(name)),
                            conflict: TransferConflict::FailIfExists,
                            target_name: None,
                        })
                        .collect(),
                    move_sources: moving,
                });

                let numbered = if parallel {
                    vec!["makefile"]
                } else {
                    vec!["docs", "makefile"]
                };
                let expected = numbered
                    .iter()
                    .map(|name| (name.to_string(), format!("{name} (1)"), RenameCause::Alias))
                    .collect::<Vec<_>>();
                assert_eq!(
                    renamed_beneath(&destination, &followed_report(&events)),
                    expected,
                    "{context}"
                );
                for name in numbered {
                    let saved_as = destination.join(format!("{name} (1)"));
                    let read = if name.ends_with("ocs") {
                        fs::read_to_string(saved_as.join("inside"))?
                    } else {
                        fs::read_to_string(saved_as)?
                    };
                    assert_eq!(read, name, "{context}");
                }
                assert_eq!(names(&destination)?.len(), items.len(), "{context}");
                assert_eq!(names(&source)?.is_empty(), moving, "{context}");
            }
        }
    }
    Ok(())
}

#[test]
fn smb_without_posix_extensions_numbers_case_pairs_without_looking_up_the_later_spelling()
-> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    // The cifs client turns server inode numbers off for the whole mount once
    // it looks a folder up under a second spelling.
    let later = ["docs", "makefile", "project/notes"];
    for (preset, numbered) in [(Quirks::cifs as Preset, true), (Quirks::cifs_posix, false)] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir_all(source.join("project"))?;
        fs::create_dir(&destination)?;
        for folder in ["Docs", "docs"] {
            fs::create_dir(source.join(folder))?;
            fs::write(source.join(folder).join("inside"), folder)?;
        }
        for file in ["Makefile", "makefile", "project/Notes", "project/notes"] {
            fs::write(source.join(file), file)?;
        }
        let scope = FsQuirks::install(&destination, preset());

        let events = run_paste(PasteRequest {
            id: OperationRequestId(1530),
            destination: Location::local(&destination),
            items: ["Docs", "Makefile", "docs", "makefile", "project"]
                .map(|item| PasteItem {
                    source: Location::local(source.join(item)),
                    conflict: TransferConflict::FailIfExists,
                    target_name: None,
                })
                .into(),
            move_sources: false,
        });

        let context = format!("{:?}", preset().mount_type);
        let expected = if numbered {
            later
                .map(|name| (name.to_owned(), format!("{name} (1)"), RenameCause::Alias))
                .into()
        } else {
            Vec::new()
        };
        assert_eq!(
            renamed_beneath(&destination, &followed_report(&events)),
            expected,
            "{context}"
        );
        for name in later {
            let saved_as = destination.join(if numbered {
                format!("{name} (1)")
            } else {
                name.to_owned()
            });
            let read = if name == "docs" {
                fs::read_to_string(saved_as.join("inside"))?
            } else {
                fs::read_to_string(saved_as)?
            };
            assert_eq!(read, name, "{context}");
            let spelling = Path::new(name).file_name().ok_or("no name")?;
            assert_eq!(
                scope.used_name(&spelling.to_string_lossy()),
                !numbered,
                "{context}: {name}"
            );
        }
    }
    Ok(())
}

#[test]
fn an_item_already_at_a_top_level_name_is_never_taken_for_one_the_paste_saved()
-> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let padding = (0..8).map(|index| format!("item {index}"));
    let spellings = ["Makefile", "makefile"].map(String::from);
    // Past eight files a paste copies several at once.
    let rows = [
        (
            "theirs at the same spelling",
            Some("item 3"),
            padding
                .clone()
                .chain(["item 8", "item 9"].map(String::from))
                .collect(),
            false,
        ),
        (
            "theirs at another spelling, at once",
            Some("MAKEFILE"),
            spellings.iter().cloned().chain(padding).collect(),
            false,
        ),
        (
            "theirs at another spelling, one at a time",
            Some("MAKEFILE"),
            spellings.to_vec(),
            false,
        ),
        (
            "theirs once the earlier spelling was saved",
            Some("MAKEFILE"),
            spellings.to_vec(),
            true,
        ),
        (
            "one name from two folders",
            None,
            vec!["one/notes.txt".to_string(), "two/notes.txt".to_string()],
            false,
        ),
    ];
    for (row, theirs, items, after_saved) in rows {
        let items: Vec<String> = items;
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&destination)?;
        for item in &items {
            let path = source.join(item);
            fs::create_dir_all(path.parent().ok_or("no parent")?)?;
            fs::write(path, item)?;
        }
        let moved_out = root.path().join("moved out");
        let mut before = Vec::new();
        match theirs {
            Some(theirs) if after_saved => {
                let (saved, taken, moved_out) = (
                    destination.join("Makefile"),
                    destination.join(theirs),
                    moved_out.clone(),
                );
                let racer: Racer = Box::new(move || {
                    fs::rename(&saved, &moved_out).expect("move the saved item out");
                    fs::write(&taken, b"theirs").expect("take its name");
                });
                before.push((Op::OpenSource, "makefile", 0, racer));
            }
            Some(theirs) => fs::write(destination.join(theirs), b"theirs")?,
            None => {}
        }
        let _source = FsQuirks::install(
            &source,
            Quirks {
                before,
                ..Quirks::ext4()
            },
        );
        let _destination = FsQuirks::install(
            &destination,
            Quirks {
                case_insensitive: true,
                ..Quirks::ext4()
            },
        );

        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();

        let _operation = LocalOperationProvider.paste(
            PasteRequest {
                id: OperationRequestId(1530),
                destination: Location::local(&destination),
                items: items
                    .iter()
                    .map(|item| PasteItem {
                        source: Location::local(source.join(item)),
                        conflict: TransferConflict::FailIfExists,
                        target_name: None,
                    })
                    .collect(),
                move_sources: false,
            },
            Rc::new(move |event| emitted.borrow_mut().push(event)),
        );
        drive_until_transfer_settles(&events);

        let events = events.take();
        assert!(
            matches!(
                terminal_transfer(&events),
                Some(OperationEvent::TransferFailed { .. })
            ),
            "{row}: {events:?}"
        );
        if let Some(theirs) = theirs {
            assert_eq!(fs::read(destination.join(theirs))?, b"theirs", "{row}");
        }
        let saved = names(&destination)?;
        assert!(
            saved.iter().all(|name| !name.ends_with(" (1)")),
            "{row}: {saved:?}"
        );
        assert_eq!(moved_out.exists(), after_saved, "{row}");
    }
    Ok(())
}
