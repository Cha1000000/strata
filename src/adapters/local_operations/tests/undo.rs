// SPDX-License-Identifier: MIT

use rustix::io::Errno;

use super::fs_quirks::{Op, Support};
use super::*;
use crate::services::RenameRequest;

fn drive_until_rename_settles(events: &Rc<RefCell<Vec<OperationEvent>>>) {
    wait_for_operation(events, |event| {
        matches!(
            event,
            OperationEvent::Renamed { .. }
                | OperationEvent::Failed { .. }
                | OperationEvent::Cancelled { .. }
        )
    });
}

#[test]
fn undoing_a_move_returns_each_item_to_its_original_directory() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for quirks in [Quirks::default, Quirks::sshfs, Quirks::vboxsf] {
        let root = tempfile::tempdir()?;
        let _scope = FsQuirks::install(root.path(), quirks());
        let origin = root.path().join("origin");
        let archive = root.path().join("archive");
        fs::create_dir_all(&origin)?;
        fs::create_dir_all(&archive)?;
        let moved = archive.join("report.txt");
        fs::write(&moved, b"contents")?;
        let original = inode(&moved);

        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();
        let _operation = LocalOperationProvider.undo_move(
            UndoMoveRequest {
                cleanup_locations: Vec::new(),
                id: OperationRequestId(40),
                items: vec![UndoMoveItem {
                    record: MoveRecord {
                        original: Location::local(origin.join("report.txt")),
                        current: Location::local(&moved),
                        renamed: Vec::new(),
                    },
                    conflict: TransferConflict::FailIfExists,
                }],
            },
            Rc::new(move |event| emitted.borrow_mut().push(event)),
        );

        drive_until_transfer_settles(&events);

        assert!(matches!(
            events.borrow().last(),
            Some(OperationEvent::Pasted { .. })
        ));
        assert!(names(&archive)?.is_empty());
        assert_eq!(names(&origin)?, ["report.txt"]);
        assert_eq!(inode(&origin.join("report.txt")), original);
        assert_eq!(fs::read(origin.join("report.txt"))?, b"contents");
    }
    Ok(())
}

#[test]
fn undoing_a_move_stops_at_an_unconfirmed_conflict() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let origin = root.path().join("origin");
    let archive = root.path().join("archive");
    fs::create_dir_all(&origin)?;
    fs::create_dir_all(&archive)?;
    let blocked = origin.join("report.txt");
    fs::write(&blocked, b"newer")?;
    let first = archive.join("notes.txt");
    let second = archive.join("report.txt");
    fs::write(&first, b"first")?;
    fs::write(&second, b"second")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.undo_move(
        UndoMoveRequest {
            cleanup_locations: Vec::new(),
            id: OperationRequestId(41),
            items: vec![
                UndoMoveItem {
                    record: MoveRecord {
                        original: Location::local(origin.join("notes.txt")),
                        current: Location::local(&first),
                        renamed: Vec::new(),
                    },
                    conflict: TransferConflict::FailIfExists,
                },
                UndoMoveItem {
                    record: MoveRecord {
                        original: Location::local(&blocked),
                        current: Location::local(&second),
                        renamed: Vec::new(),
                    },
                    conflict: TransferConflict::FailIfExists,
                },
            ],
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );

    drive_until_transfer_settles(&events);

    let completed = match events.borrow().last() {
        Some(OperationEvent::TransferFailed {
            completed_locations,
            ..
        }) => completed_locations.clone(),
        other => panic!("expected a transfer failure, got {other:?}"),
    };
    assert_eq!(completed, vec![Location::local(&first)]);
    assert_eq!(
        item_moves(&events.borrow()),
        [(
            Location::local(&first),
            Location::local(origin.join("notes.txt"))
        )]
    );
    assert_eq!(fs::read(&blocked)?, b"newer");
    assert_eq!(fs::read(&second)?, b"second");
    assert_eq!(fs::read(origin.join("notes.txt"))?, b"first");
    Ok(())
}

#[test]
fn a_confirmed_undo_conflict_replaces_the_newer_item() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let origin = root.path().join("origin");
    let archive = root.path().join("archive");
    fs::create_dir_all(&origin)?;
    fs::create_dir_all(&archive)?;
    let original = origin.join("report.txt");
    let moved = archive.join("report.txt");
    fs::write(&original, b"newer")?;
    fs::write(&moved, b"moved")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.undo_move(
        UndoMoveRequest {
            cleanup_locations: Vec::new(),
            id: OperationRequestId(42),
            items: vec![UndoMoveItem {
                record: MoveRecord {
                    original: Location::local(&original),
                    current: Location::local(&moved),
                    renamed: Vec::new(),
                },
                conflict: TransferConflict::ReplaceExisting,
            }],
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );

    drive_until_transfer_settles(&events);

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Pasted { .. })
    ));
    assert!(!moved.exists());
    assert_eq!(fs::read(&original)?, b"moved");
    Ok(())
}

#[test]
fn rename_undo_restores_a_native_name_byte_for_byte() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let original_parent = root.path().join("original");
    let current_parent = root.path().join("current");
    fs::create_dir_all(&original_parent)?;
    fs::create_dir_all(&current_parent)?;
    let original_name = OsString::from_vec(b"original-\xff.txt".to_vec());
    let original = original_parent.join(&original_name);
    let current = current_parent.join("renamed.txt");
    fs::write(&current, b"contents")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.undo_rename(
        UndoRenameRequest {
            id: OperationRequestId(43),
            current: Location::local(&current),
            original: Location::local(&original),
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );
    drive_until_rename_settles(&events);

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Renamed { .. })
    ));
    assert!(!current.exists());
    assert_eq!(fs::read(&original)?, b"contents");
    assert_eq!(
        original_name.as_encoded_bytes(),
        original
            .file_name()
            .expect("original test path should have a file name")
            .as_encoded_bytes()
    );
    Ok(())
}

#[test]
fn rename_undo_refuses_an_occupied_destination_and_can_retry() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let original_parent = root.path().join("original");
    let current_parent = root.path().join("current");
    fs::create_dir_all(&original_parent)?;
    fs::create_dir_all(&current_parent)?;
    let original = original_parent.join("item.txt");
    let current = current_parent.join("renamed.txt");
    fs::write(&original, b"occupant")?;
    fs::write(&current, b"renamed")?;
    let request = || UndoRenameRequest {
        id: OperationRequestId(44),
        current: Location::local(&current),
        original: Location::local(&original),
    };

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    {
        let _operation = LocalOperationProvider.undo_rename(
            request(),
            Rc::new(move |event| emitted.borrow_mut().push(event)),
        );
        drive_until_rename_settles(&events);
    }
    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Failed { .. })
    ));
    assert_eq!(fs::read(&original)?, b"occupant");
    assert_eq!(fs::read(&current)?, b"renamed");

    fs::remove_file(&original)?;
    events.borrow_mut().clear();
    let emitted = events.clone();
    let _operation = LocalOperationProvider.undo_rename(
        request(),
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );
    drive_until_rename_settles(&events);
    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Renamed { .. })
    ));
    assert!(!current.exists());
    assert_eq!(fs::read(&original)?, b"renamed");
    Ok(())
}

fn inode(path: &Path) -> u64 {
    std::os::unix::fs::MetadataExt::ino(&fs::symlink_metadata(path).expect("entry exists"))
}

fn settle(start: impl FnOnce(Rc<dyn Fn(OperationEvent)>) -> LoadHandle) -> Vec<OperationEvent> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = start(Rc::new(move |event| emitted.borrow_mut().push(event)));
    drive_until_rename_settles(&events);
    events.take()
}

fn rename(path: &Path, new_name: &str) -> Vec<OperationEvent> {
    let entry = file_entry(path);
    settle(|emit| {
        LocalOperationProvider.rename(
            RenameRequest {
                id: OperationRequestId(46),
                entry,
                new_name: new_name.to_owned(),
            },
            emit,
        )
    })
}

fn undo_rename(current: &Path, original: &Path) -> Vec<OperationEvent> {
    let request = UndoRenameRequest {
        id: OperationRequestId(47),
        current: Location::local(current),
        original: Location::local(original),
    };
    settle(|emit| LocalOperationProvider.undo_rename(request, emit))
}

fn failure(events: &[OperationEvent]) -> String {
    match events {
        [OperationEvent::Failed { message, .. }] => message.clone(),
        other => panic!("expected one failure, got {other:?}"),
    }
}

#[test]
fn renames_and_their_undo_keep_the_item_where_rename_flags_or_links_are_missing()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let linked_before_a_probe = || Quirks {
        rename_flags: Support::Rejected(Errno::NOSYS),
        ..Quirks::sshfs()
    };
    let cases: [Preset; 4] = [
        Quirks::without_rename_flags,
        Quirks::vboxsf,
        Quirks::sshfs,
        linked_before_a_probe,
    ];
    for quirks in cases {
        for folder in [false, true] {
            let root = tempfile::tempdir()?;
            let _scope = FsQuirks::install(root.path(), quirks());
            let source = root.path().join("item");
            let renamed = root.path().join("renamed");
            if folder {
                fs::create_dir(&source)?;
            } else {
                fs::write(&source, b"data")?;
            }
            let original = inode(&source);

            let events = rename(&source, "renamed");
            assert!(
                matches!(events.as_slice(), [OperationEvent::Renamed { .. }]),
                "{events:?}"
            );
            assert_eq!(inode(&renamed), original);

            let events = undo_rename(&renamed, &source);
            assert!(
                matches!(events.as_slice(), [OperationEvent::Renamed { .. }]),
                "{events:?}"
            );
            assert_eq!(inode(&source), original);
            assert_eq!(names(root.path())?, ["item"]);
        }
    }
    Ok(())
}

#[test]
fn a_rename_never_replaces_an_item_created_meanwhile() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for quirks in [Quirks::default, Quirks::vboxsf] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("item.txt");
        let taken = root.path().join("new.txt");
        fs::write(&source, b"mine")?;
        let raced = taken.clone();
        let _scope = FsQuirks::install(
            root.path(),
            Quirks {
                before: vec![(
                    Op::Rename,
                    "new.txt",
                    0,
                    Box::new(move || fs::write(&raced, b"theirs").expect("racer")),
                )],
                ..quirks()
            },
        );

        assert_eq!(
            failure(&rename(&source, "new.txt")),
            "\u{201c}new.txt\u{201d} already exists"
        );
        assert_eq!(fs::read(&source)?, b"mine");
        assert_eq!(fs::read(&taken)?, b"theirs");
    }
    Ok(())
}

#[test]
fn a_case_only_rename_and_its_undo_keep_the_item_or_say_where_it_was_left()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("report.txt");
    let renamed = root.path().join("Report.txt");
    fs::write(&source, b"data")?;
    let original = inode(&source);
    {
        let _scope = FsQuirks::install(
            root.path(),
            Quirks {
                case_insensitive: true,
                ..Quirks::default()
            },
        );
        assert!(matches!(
            rename(&source, "Report.txt").as_slice(),
            [OperationEvent::Renamed { .. }]
        ));
        assert_eq!(inode(&renamed), original);
        assert!(matches!(
            undo_rename(&renamed, &source).as_slice(),
            [OperationEvent::Renamed { .. }]
        ));
        assert_eq!(inode(&source), original);
    }

    let _scope = FsQuirks::install(
        root.path(),
        Quirks {
            case_insensitive: true,
            // Skips the direct attempts, then fails the second step and the way back.
            fail: vec![
                (Op::Rename, "Report.txt", 1, Errno::IO),
                (Op::Rename, "report.txt", 1, Errno::IO),
            ],
            ..Quirks::default()
        },
    );
    let message = failure(&rename(&source, "Report.txt"));

    let left = names(root.path())?;
    let [temporary] = left.as_slice() else {
        panic!("expected one entry, got {left:?}");
    };
    assert!(temporary.starts_with(".strata-rename-"), "{temporary}");
    assert_eq!(inode(&root.path().join(temporary)), original);
    assert_eq!(
        message,
        format!(
            "Could not rename \u{201c}report.txt\u{201d}; it was left as \u{201c}{temporary}\u{201d}: A device input/output error occurred"
        )
    );
    Ok(())
}

fn undo_one(
    id: u64,
    original: &Path,
    current: &Path,
    conflict: TransferConflict,
) -> (LoadHandle, Rc<RefCell<Vec<OperationEvent>>>) {
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let operation = LocalOperationProvider.undo_move(
        UndoMoveRequest {
            cleanup_locations: Vec::new(),
            id: OperationRequestId(id),
            items: vec![UndoMoveItem {
                record: MoveRecord {
                    original: Location::local(original),
                    current: Location::local(current),
                    renamed: Vec::new(),
                },
                conflict,
            }],
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );
    (operation, events)
}

#[test]
fn undoing_a_cross_volume_move_deletes_the_moved_item_only_once_it_is_flushed()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (id, flushed) in [(97, true), (98, false)] {
        let root = tempfile::tempdir()?;
        let origin = root.path().join("usb");
        let archive = root.path().join("archive");
        fs::create_dir_all(&origin)?;
        fs::create_dir_all(&archive)?;
        let current = archive.join("report.txt");
        fs::write(&current, b"contents")?;
        let during = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (record, watched) = (during.clone(), current.clone());
        let racer: super::fs_quirks::Racer = Box::new(move || {
            record
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(watched.exists());
        });
        let _archive = FsQuirks::install(&archive, Quirks::default());
        let _origin = FsQuirks::install(
            &origin,
            Quirks {
                before: vec![(Op::SyncFs, "*", 0, racer)],
                fail: if flushed {
                    Vec::new()
                } else {
                    vec![(Op::SyncFs, "*", 0, Errno::IO)]
                },
                ..Quirks::ext4()
            },
        );

        let (_operation, events) = undo_one(
            id,
            &origin.join("report.txt"),
            &current,
            TransferConflict::FailIfExists,
        );
        pump_until_transfer(&events);

        assert_eq!(
            *during.lock().unwrap_or_else(|error| error.into_inner()),
            [true],
            "flushed: {flushed}"
        );
        assert_eq!(fs::read(origin.join("report.txt"))?, b"contents");
        if flushed {
            assert!(matches!(
                terminal_transfer(&events.borrow()),
                Some(OperationEvent::Pasted { .. })
            ));
            assert!(!current.exists());
            continue;
        }
        assert!(matches!(terminal_transfer(&events.borrow()),
            Some(OperationEvent::TransferFailed { message, .. })
            if message.contains("writes may still be pending")));
        assert_eq!(fs::read(&current)?, b"contents");
    }
    Ok(())
}

#[test]
fn cancelled_rename_undo_reports_cancellation_without_moving_the_item() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let original_parent = root.path().join("original");
    let current_parent = root.path().join("current");
    fs::create_dir_all(&original_parent)?;
    fs::create_dir_all(&current_parent)?;
    let original = original_parent.join("item.txt");
    let current = current_parent.join("renamed.txt");
    fs::write(&current, b"renamed")?;
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let operation = LocalOperationProvider.undo_rename(
        UndoRenameRequest {
            id: OperationRequestId(45),
            current: Location::local(&current),
            original: Location::local(&original),
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );
    drop(operation);
    drive_until_rename_settles(&events);

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Cancelled { .. })
    ));
    assert!(current.exists());
    assert!(!original.exists());
    Ok(())
}

#[test]
fn undoing_a_grouping_reports_the_trashed_group_folder() {
    crate::test_support::gtk_test(
        "adapters::local_operations::tests::undo::undoing_a_grouping_reports_the_trashed_group_folder",
        || {
            let root = tempfile::tempdir().expect("root");
            let group = root.path().join("group");
            fs::create_dir(&group).expect("group folder");
            fs::write(group.join("report.txt"), b"contents").expect("grouped item");
            let identity = TrashedOriginal::at_path(&group).expect("identity");
            let events = Rc::new(RefCell::new(Vec::new()));
            let emitted = events.clone();
            let _operation = LocalOperationProvider.undo_move(
                UndoMoveRequest {
                    cleanup_locations: vec![Location::local(&group)],
                    id: OperationRequestId(42),
                    items: vec![UndoMoveItem {
                        record: MoveRecord {
                            original: Location::local(root.path().join("report.txt")),
                            current: Location::local(group.join("report.txt")),
                            renamed: Vec::new(),
                        },
                        conflict: TransferConflict::FailIfExists,
                    }],
                },
                Rc::new(move |event| emitted.borrow_mut().push(event)),
            );

            drive_until_transfer_settles(&events);

            let events = events.borrow();
            assert!(matches!(events.last(), Some(OperationEvent::Pasted { .. })));
            assert!(!group.exists());
            assert_eq!(
                trashed_items(&events),
                [(Location::local(&group), identity)]
            );
        },
    );
}

#[test]
fn undoing_a_move_gives_back_names_numbered_as_aliases() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for taken in [false, true] {
        let root = tempfile::tempdir()?;
        let origin = root.path().join("origin");
        let usb = root.path().join("usb");
        let project = origin.join("project");
        fs::create_dir_all(&project)?;
        fs::create_dir(&usb)?;
        fs::write(project.join("Makefile"), b"upper")?;
        fs::write(project.join("makefile"), b"lower")?;
        fs::create_dir(project.join("Docs"))?;
        fs::create_dir(project.join("docs"))?;
        fs::write(project.join("docs/Makefile"), b"inner upper")?;
        fs::write(project.join("docs/makefile"), b"inner lower")?;
        let restored = project.join("makefile");
        let racer: super::fs_quirks::Racer = {
            let restored = restored.clone();
            Box::new(move || fs::write(&restored, b"taken").expect("take the name"))
        };
        let _origin = FsQuirks::install(
            &origin,
            Quirks {
                before: taken
                    .then(|| (Op::Rename, "makefile (1)", 0, racer))
                    .into_iter()
                    .collect(),
                ..Quirks::default()
            },
        );
        let _usb = FsQuirks::install(&usb, Quirks::vfat());
        let moved = run_paste(PasteRequest {
            id: OperationRequestId(44),
            destination: Location::local(&usb),
            items: vec![PasteItem {
                source: Location::local(&project),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: true,
        });
        let renamed = moved
            .iter()
            .flat_map(|event| match event {
                OperationEvent::ItemsReported { report, .. } => report.renamed.clone(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names(&usb.join("project"))?,
            ["Docs", "Makefile", "docs (1)", "makefile (1)"]
        );

        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();
        let _operation = LocalOperationProvider.undo_move(
            UndoMoveRequest {
                cleanup_locations: Vec::new(),
                id: OperationRequestId(45),
                items: vec![UndoMoveItem {
                    record: MoveRecord {
                        original: Location::local(&project),
                        current: Location::local(usb.join("project")),
                        renamed,
                    },
                    conflict: TransferConflict::FailIfExists,
                }],
            },
            Rc::new(move |event| emitted.borrow_mut().push(event)),
        );
        drive_until_transfer_settles(&events);

        assert!(matches!(
            events.borrow().last(),
            Some(OperationEvent::Pasted { .. })
        ));
        assert_eq!(fs::read(project.join("Makefile"))?, b"upper");
        assert_eq!(fs::read(project.join("docs/makefile"))?, b"inner lower");
        assert_eq!(fs::read(project.join("docs/Makefile"))?, b"inner upper");
        let reported = events
            .borrow()
            .iter()
            .flat_map(|event| match event {
                OperationEvent::ItemsReported { report, .. } => report.renamed.clone(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        if taken {
            assert_eq!(
                names(&project)?,
                ["Docs", "Makefile", "docs", "makefile", "makefile (1)"]
            );
            assert_eq!(fs::read(&restored)?, b"taken");
            assert_eq!(fs::read(project.join("makefile (1)"))?, b"lower");
            assert_eq!(
                reported,
                [crate::services::RenamedItem {
                    requested: Location::local(&restored),
                    saved_as: Location::local(project.join("makefile (1)")),
                    cause: crate::services::RenameCause::Taken,
                }]
            );
        } else {
            assert_eq!(names(&project)?, ["Docs", "Makefile", "docs", "makefile"]);
            assert_eq!(fs::read(&restored)?, b"lower");
            assert!(reported.is_empty());
        }
    }
    Ok(())
}
