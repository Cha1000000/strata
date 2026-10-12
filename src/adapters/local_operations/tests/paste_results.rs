// SPDX-License-Identifier: MIT

use super::*;

#[test]
fn a_copy_reports_the_destination_it_created_and_no_moved_item() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
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
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
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
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
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
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
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
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
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
