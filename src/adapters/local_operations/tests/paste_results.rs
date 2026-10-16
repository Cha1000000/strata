// SPDX-License-Identifier: MIT

use super::fs_quirks::{Op, Support};
use super::*;

#[test]
fn a_copy_reports_the_destination_it_created_and_no_moved_item() -> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("photo.jpg");
    let destination = root.path().join("album");
    fs::write(&source, b"original-content")?;
    fs::create_dir(&destination)?;

    let events = run_paste(PasteRequest {
        id: OperationRequestId(70),
        destination: Location::local(&destination),
        items: vec![PasteItem {
            source: Location::local(&source),
            conflict: TransferConflict::FailIfExists,
        }],
        move_sources: false,
    });

    let created = events
        .iter()
        .filter_map(|event| match event {
            OperationEvent::TransferProgress {
                created_location, ..
            } => created_location.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        created,
        vec![Location::local(destination.join("photo.jpg"))]
    );
    assert!(item_moves(&events).is_empty());
    Ok(())
}

#[test]
fn duplicating_a_file_preserves_contents_and_reports_the_generated_name()
-> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let root = tempfile::tempdir()?;
    let destination = root.path().to_path_buf();
    let source = destination.join("photo.jpg");
    fs::write(&source, b"original-content")?;

    let created = run_paste_collecting_created(PasteRequest {
        id: OperationRequestId(71),
        destination: Location::local(&destination),
        items: vec![PasteItem {
            source: Location::local(&source),
            conflict: TransferConflict::FailIfExists,
        }],
        move_sources: false,
    })?;

    assert_eq!(
        created.into_iter().flatten().collect::<Vec<_>>(),
        vec![Location::local(destination.join("photo (1).jpg"))]
    );
    assert_eq!(fs::read(&source)?, b"original-content");
    assert_eq!(
        fs::read(destination.join("photo (1).jpg"))?,
        b"original-content"
    );
    Ok(())
}

#[test]
fn a_copy_that_replaces_an_existing_item_reports_its_destination() -> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("photo.jpg");
    let destination = root.path().join("album");
    fs::write(&source, b"new-content")?;
    fs::create_dir(&destination)?;
    fs::write(destination.join("photo.jpg"), b"old-content")?;

    let created = run_paste_collecting_created(PasteRequest {
        id: OperationRequestId(72),
        destination: Location::local(&destination),
        items: vec![PasteItem {
            source: Location::local(&source),
            conflict: TransferConflict::ReplaceExisting,
        }],
        move_sources: false,
    })?;

    assert_eq!(
        created.into_iter().flatten().collect::<Vec<_>>(),
        vec![Location::local(destination.join("photo.jpg"))]
    );
    Ok(())
}

#[test]
fn a_move_reports_where_each_item_landed_instead_of_a_created_destination()
-> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let root = tempfile::tempdir()?;
    let photo = root.path().join("photo.jpg");
    let notes = root.path().join("notes");
    let destination = root.path().join("album");
    fs::write(&photo, b"original-content")?;
    fs::create_dir(&notes)?;
    fs::create_dir(&destination)?;

    let events = run_paste(PasteRequest {
        id: OperationRequestId(73),
        destination: Location::local(&destination),
        items: [&photo, &notes]
            .map(|source| PasteItem {
                source: Location::local(source),
                conflict: TransferConflict::FailIfExists,
            })
            .into(),
        move_sources: true,
    });

    assert!(!events.iter().any(|event| matches!(
        event,
        OperationEvent::TransferProgress {
            created_location: Some(_),
            ..
        }
    )));
    assert_eq!(
        item_moves(&events),
        [
            (
                Location::local(&photo),
                Location::local(destination.join("photo.jpg"))
            ),
            (
                Location::local(&notes),
                Location::local(destination.join("notes"))
            ),
        ]
    );
    Ok(())
}

#[test]
fn an_item_replaced_without_trash_is_left_out_of_the_paste_undo() -> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    for move_sources in [false, true] {
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let destination = root.path().join("destination");
        fs::create_dir(&sources)?;
        fs::create_dir(&destination)?;
        fs::write(sources.join("replace.txt"), b"incoming")?;
        fs::write(sources.join("new.txt"), b"new")?;
        fs::write(destination.join("replace.txt"), b"only copy")?;
        let _quirks = FsQuirks::install(
            root.path(),
            Quirks {
                trash: super::fs_quirks::Trash::Unsupported,
                fs_type: Some(0x6969),
                ..Quirks::default()
            },
        );

        let events = run_paste(PasteRequest {
            id: OperationRequestId(74),
            destination: Location::local(&destination),
            items: vec![
                PasteItem {
                    source: Location::local(sources.join("replace.txt")),
                    conflict: TransferConflict::ReplaceExisting,
                },
                PasteItem {
                    source: Location::local(sources.join("new.txt")),
                    conflict: TransferConflict::FailIfExists,
                },
            ],
            move_sources,
        });

        let created = events
            .iter()
            .filter_map(|event| match event {
                OperationEvent::TransferProgress {
                    created_location, ..
                } => created_location.clone(),
                _ => None,
            })
            .collect::<Vec<_>>();
        let expected_created = if move_sources {
            Vec::new()
        } else {
            vec![Location::local(destination.join("new.txt"))]
        };
        assert_eq!(created, expected_created);
        assert!(!events.iter().any(|event| matches!(
            event,
            OperationEvent::Merged { .. } | OperationEvent::ItemTrashed { .. }
        )));
        let expected_moves = if move_sources {
            ["replace.txt", "new.txt"]
                .map(|name| {
                    (
                        Location::local(sources.join(name)),
                        Location::local(destination.join(name)),
                    )
                })
                .to_vec()
        } else {
            Vec::new()
        };
        assert_eq!(item_moves(&events), expected_moves);
        assert_eq!(fs::read(destination.join("replace.txt"))?, b"incoming");
    }
    Ok(())
}

#[test]
fn a_cancel_that_cuts_off_an_item_counts_it_as_not_attempted() -> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let bulk = super::super::MAX_LOCAL_IO_WORKERS + 1;
    // A move runs in the foreground and a copy in the background; enough
    // files take the bulk path, which copies several at once.
    for (moving, files) in [(true, 2), (false, 2), (true, bulk), (false, bulk)] {
        let row = format!("moving: {moving}, files: {files}");
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let destination = root.path().join("destination");
        fs::create_dir(&sources)?;
        fs::create_dir(&destination)?;
        let items = (0..files)
            .map(|index| sources.join(format!("file-{index}.bin")))
            .collect::<Vec<_>>();
        for item in &items {
            fs::write(item, vec![0x5a; 4 * 1024 * 1024])?;
        }
        let cancel = CancelFromMain::default();
        let _sources = FsQuirks::install(&sources, Quirks::default());
        let _destination = FsQuirks::install(
            &destination,
            Quirks {
                before: vec![
                    (Op::CopyRange, "*", 0, cancel.racer()),
                    (Op::Write, "*", 0, cancel.racer()),
                ],
                reflink: Support::Rejected(rustix::io::Errno::OPNOTSUPP),
                ..Quirks::default()
            },
        );
        let browser = crate::app::Browser::new(Rc::new(crate::adapters::LocalFileSource));
        browser.set_operation_provider(Rc::new(LocalOperationProvider));
        let ended = Rc::new(RefCell::new(None));
        let observed = ended.clone();
        browser.observe(move |event| {
            use crate::app::BrowserEvent;
            match crate::test_support::operations::operation_event(event) {
                BrowserEvent::OperationCancelled {
                    completed,
                    failed,
                    not_attempted,
                    ..
                } => {
                    observed.replace(Some(Ok((*completed, *failed, *not_attempted))));
                }
                event
                @ (BrowserEvent::TransferCompleted | BrowserEvent::OperationFailed { .. }) => {
                    observed.replace(Some(Err(format!("{event:?}"))));
                }
                _ => {}
            }
        });
        browser.transfer(
            Location::local(&destination),
            items
                .iter()
                .map(|source| PasteItem {
                    source: Location::local(source),
                    conflict: TransferConflict::FailIfExists,
                })
                .collect(),
            moving,
            false,
        );
        let id = browser.last_started_operation().expect("the transfer");
        assert_eq!(browser.background_file_operation(id), !moving, "{row}");

        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(20);
        while ended.borrow().is_none() {
            assert!(Instant::now() < deadline, "{row}: the transfer never ended");
            cancel.serve(|| browser.cancel_operation(id));
            context.iteration(false);
            thread::sleep(Duration::from_millis(1));
        }

        assert!(cancel.served(), "{row}");
        let Some(Ok((completed, failed, not_attempted))) = ended.take() else {
            panic!("{row}: the transfer was not cancelled");
        };
        assert_eq!(failed, 0, "{row}");
        assert!(not_attempted > 0, "{row}");
        assert_eq!(completed + not_attempted, files, "{row}");
        if files == 2 {
            assert_eq!(completed, 0, "{row}");
        }
        assert_eq!(fs::read_dir(&destination)?.count(), completed, "{row}");
        let intact = items
            .iter()
            .filter(|item| fs::metadata(item).is_ok_and(|item| item.len() == 4 * 1024 * 1024))
            .count();
        assert_eq!(intact, if moving { not_attempted } else { files }, "{row}");
    }
    Ok(())
}

#[test]
fn only_a_cut_off_between_local_locations_counts_as_not_attempted() {
    let local = gio::File::for_path("/tmp/strata-cut-off");
    let remote = gio::File::for_uri("sftp://host/strata-cut-off");
    for (source, target, untouched) in [
        (&local, &local, true),
        (&local, &remote, false),
        (&remote, &local, false),
    ] {
        assert_eq!(
            super::super::undone_when_cancelled(source, target),
            untouched,
            "{} to {}",
            source.uri(),
            target.uri()
        );
    }
    assert!(!super::super::deletion_undone_when_cancelled(
        &Location::uri("sftp://host/strata-cut-off"),
        false
    ));
}
