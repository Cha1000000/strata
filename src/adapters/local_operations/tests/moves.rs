// SPDX-License-Identifier: MIT

use std::os::unix::fs::MetadataExt;

use rustix::io::Errno;

use super::fs_quirks::{Op, Support};
use super::*;

#[test]
fn moving_a_directory_falls_back_to_a_safe_copy_when_the_move_needs_one()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-move-fallback-test-{unique}"));
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir_all(source.join("nested"))?;
    fs::write(source.join("top.txt"), b"top")?;
    fs::write(source.join("nested/child.txt"), b"child")?;

    let result = glib::MainContext::default().block_on(move_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        gio::Cancellable::new(),
        always_copy(),
    ));

    assert!(result.is_ok());
    assert!(!source.exists());
    assert_eq!(fs::read(target.join("top.txt"))?, b"top");
    assert_eq!(fs::read(target.join("nested/child.txt"))?, b"child");
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn a_failed_move_attempt_is_returned_without_falling_back() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-move-real-failure-test-{unique}"));
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir_all(&source)?;
    fs::write(source.join("top.txt"), b"top")?;

    let result = glib::MainContext::default().block_on(move_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        gio::Cancellable::new(),
        Rc::new(|_, _, _| {
            Box::pin(async {
                Err(glib::Error::new(
                    gio::IOErrorEnum::PermissionDenied,
                    "injected permission failure",
                ))
            })
        }),
    ));

    assert!(result.is_err_and(|error| error.matches(gio::IOErrorEnum::PermissionDenied)));
    assert_eq!(fs::read(source.join("top.txt"))?, b"top");
    assert!(!target.exists());
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn a_successful_move_attempt_is_used_without_falling_back_to_copy() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-move-success-test-{unique}"));
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir_all(&source)?;
    fs::write(source.join("top.txt"), b"top")?;

    let result = glib::MainContext::default().block_on(move_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        gio::Cancellable::new(),
        Rc::new(|source, target, _| {
            Box::pin(async move {
                fs::rename(
                    source.path().expect("native source"),
                    target.path().expect("native target"),
                )
                .map(|()| super::super::Moved::Done)
                .map_err(super::io_error)
            })
        }),
    ));

    assert!(result.is_ok());
    assert!(!source.exists());
    assert_eq!(fs::read(target.join("top.txt"))?, b"top");
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn a_plain_move_relocates_the_entry_via_the_hardened_rename_path() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.txt");
    let target = root.path().join("target.txt");
    fs::write(&source, b"payload")?;

    let result = glib::MainContext::default().block_on(move_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        gio::Cancellable::new(),
        None,
        false,
    ));

    assert!(result.is_ok());
    assert!(!source.exists());
    assert_eq!(fs::read(target)?, b"payload");
    Ok(())
}

fn move_native(source: &Path, target: &Path) -> Result<(), glib::Error> {
    glib::MainContext::default().block_on(move_local(
        gio::File::for_path(source),
        gio::File::for_path(target),
        gio::Cancellable::new(),
        None,
        false,
    ))
}

fn assert_refused_into_itself(result: Result<(), glib::Error>) {
    assert_eq!(
        result.map_err(|error| error.message().to_owned()),
        Err(crate::i18n::tr("Can\u{2019}t put a folder inside itself"))
    );
}

#[test]
fn moving_a_directory_into_its_own_child_fails_instead_of_deleting_it() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for quirks in [Quirks::default(), Quirks::without_placement()] {
        let root = tempfile::tempdir()?;
        let _scope = FsQuirks::install(root.path(), quirks);
        let source = root.path().join("source");
        fs::create_dir_all(source.join("nested"))?;
        fs::write(source.join("top.txt"), b"top")?;
        let target = source.join("nested").join("moved-source");

        assert_refused_into_itself(move_native(&source, &target));
        assert_eq!(fs::read(source.join("top.txt"))?, b"top");
        assert!(!target.exists());
        assert_eq!(fs::read_dir(source.join("nested"))?.count(), 0);
    }
    Ok(())
}

#[test]
fn a_move_never_copies_a_folder_into_itself_across_a_nested_mount() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let docs = root.path().join("docs");
    fs::create_dir_all(docs.join("Backup"))?;
    fs::write(docs.join("top.txt"), b"top")?;
    let link = root.path().join("link");
    std::os::unix::fs::symlink(&docs, &link)?;
    let _outer = FsQuirks::install(root.path(), Quirks::default());
    let _nested = FsQuirks::install(&docs.join("Backup"), Quirks::default());

    for target in [docs.join("Backup/docs"), link.join("Backup/docs")] {
        assert_refused_into_itself(move_native(&docs, &target));
        assert_eq!(fs::read(docs.join("top.txt"))?, b"top");
        assert_eq!(fs::read_dir(docs.join("Backup"))?.count(), 0);
    }
    Ok(())
}

#[test]
fn same_volume_move_on_a_flag_rejecting_filesystem_never_copies() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for rejection in [Errno::INVAL, Errno::NOSYS, Errno::OPNOTSUPP] {
        let root = tempfile::tempdir()?;
        let scope = FsQuirks::install(
            root.path(),
            Quirks {
                rename_flags: Support::Rejected(rejection),
                ..Quirks::default()
            },
        );
        let file = root.path().join("notes.txt");
        let folder = root.path().join("folder");
        fs::write(&file, b"notes")?;
        fs::create_dir(&folder)?;
        fs::write(folder.join("inside.txt"), b"inside")?;
        let destination = root.path().join("destination");
        fs::create_dir(&destination)?;
        let file_inode = fs::metadata(&file)?.ino();
        let folder_inode = fs::metadata(&folder)?.ino();

        move_native(&file, &destination.join("notes.txt"))?;
        move_native(&folder, &destination.join("folder"))?;

        assert_eq!(
            fs::metadata(destination.join("notes.txt"))?.ino(),
            file_inode
        );
        assert_eq!(
            fs::metadata(destination.join("folder"))?.ino(),
            folder_inode
        );
        assert_eq!(fs::read(destination.join("folder/inside.txt"))?, b"inside");
        assert!(!file.exists() && !folder.exists());
        assert_eq!(
            scope.count(Op::Create),
            usize::from(rejection == Errno::INVAL)
        );
    }
    Ok(())
}

#[test]
fn a_file_that_cannot_be_linked_is_moved_by_copying() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    // As protected_hardlinks refuses another user's file that a rename could move.
    let _scope = FsQuirks::install(
        root.path(),
        Quirks {
            fail: vec![(Op::Link, "moved.txt", 0, Errno::PERM)],
            ..Quirks::without_rename_flags()
        },
    );
    let source = root.path().join("notes.txt");
    fs::write(&source, b"notes")?;

    move_native(&source, &root.path().join("moved.txt"))?;

    assert_eq!(fs::read(root.path().join("moved.txt"))?, b"notes");
    assert!(!source.exists());
    Ok(())
}

#[test]
fn einval_from_every_rung_reports_an_invalid_name_and_keeps_the_source()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let flags_supported = || Quirks {
        invalid_name: Some(has_colon),
        ..Quirks::default()
    };
    let without_flags_or_links = || Quirks {
        invalid_name: Some(has_colon),
        rename_flags: Support::Rejected(Errno::INVAL),
        links: Support::Rejected(Errno::PERM),
        ..Quirks::default()
    };
    for (quirks, kind) in [
        (flags_supported(), "file"),
        (flags_supported(), "folder"),
        (without_flags_or_links(), "folder"),
        (without_flags_or_links(), "symlink"),
    ] {
        let root = tempfile::tempdir()?;
        let _scope = FsQuirks::install(root.path(), quirks);
        let source = root.path().join("source");
        match kind {
            "file" => fs::write(&source, b"kept")?,
            "folder" => fs::create_dir(&source)?,
            _ => std::os::unix::fs::symlink("elsewhere", &source)?,
        }
        let target = root.path().join("a:b");

        let error = move_native(&source, &target).expect_err("invalid name");

        assert_eq!(
            error.message(),
            rust_i18n::t!(
                "Could not move %{name}: %{error}",
                name = "source",
                error = crate::services::error_detail(crate::i18n::tr(
                    crate::services::INVALID_NAME_REASON
                ))
            ),
            "{kind}"
        );
        assert!(fs::symlink_metadata(&source).is_ok(), "{kind}");
        assert!(fs::symlink_metadata(&target).is_err(), "{kind}");
        assert_eq!(fs::read_dir(root.path())?.count(), 1, "{kind}");
    }
    Ok(())
}

#[test]
fn move_accepts_a_symlink_in_the_sources_parent_path() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let actual_parent = root.path().join("actual");
    let linked_parent = root.path().join("linked");
    fs::create_dir(&actual_parent)?;
    fs::write(actual_parent.join("source.txt"), b"keep")?;
    std::os::unix::fs::symlink(&actual_parent, &linked_parent)?;
    let target = root.path().join("target.txt");

    let result = glib::MainContext::default().block_on(move_local(
        gio::File::for_path(linked_parent.join("source.txt")),
        gio::File::for_path(&target),
        gio::Cancellable::new(),
        None,
        false,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert!(!actual_parent.join("source.txt").exists());
    assert_eq!(fs::read(target)?, b"keep");
    Ok(())
}

#[test]
fn move_accepts_a_symlink_in_the_destinations_parent_path() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.txt");
    let actual_destination = root.path().join("actual");
    let linked_destination = root.path().join("linked");
    fs::create_dir(&actual_destination)?;
    fs::write(&source, b"keep")?;
    std::os::unix::fs::symlink(&actual_destination, &linked_destination)?;

    let result = glib::MainContext::default().block_on(move_local(
        gio::File::for_path(&source),
        gio::File::for_path(linked_destination.join("target.txt")),
        gio::Cancellable::new(),
        None,
        false,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert!(!source.exists());
    assert_eq!(fs::read(actual_destination.join("target.txt"))?, b"keep");
    Ok(())
}

fn paste_move(
    id: u64,
    destination: &Path,
    source: &Path,
    conflict: TransferConflict,
) -> (LoadHandle, Rc<RefCell<Vec<OperationEvent>>>) {
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let operation = LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(id),
            destination: Location::local(destination),
            items: vec![PasteItem {
                source: Location::local(source),
                conflict,
            }],
            move_sources: true,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );
    (operation, events)
}

#[test]
fn cross_volume_move_keeps_the_source_when_the_removable_flush_fails() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source_dir = root.path().join("source");
    let destination = root.path().join("usb");
    fs::create_dir_all(&source_dir)?;
    fs::create_dir_all(&destination)?;
    let source = source_dir.join("notes.txt");
    fs::write(&source, b"keep-me")?;
    let _guard = RemovableFlushGuard::install(root.path(), Some(io::ErrorKind::Other));
    let _volumes = separate_volumes(&source_dir, &destination);

    let (_operation, events) =
        paste_move(90, &destination, &source, TransferConflict::FailIfExists);
    pump_until_transfer(&events);

    let observed = sync_probe_observations();
    assert!(
        matches!(
            terminal_transfer(&events.borrow()),
            Some(OperationEvent::TransferFailed { .. })
        ),
        "events={:?} syncs={} source_exists={}",
        events.borrow(),
        observed.len(),
        source.exists()
    );
    assert!(
        !events
            .borrow()
            .iter()
            .any(|event| matches!(event, OperationEvent::Pasted { .. }))
    );
    assert_eq!(fs::read(&source)?, b"keep-me");
    assert_eq!(fs::read(destination.join("notes.txt"))?, b"keep-me");
    assert!(!sync_probe_observations().is_empty());
    Ok(())
}

#[test]
fn cross_volume_move_flushes_the_removable_destination_before_deleting_the_source()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source_dir = root.path().join("source");
    let destination = root.path().join("usb");
    fs::create_dir_all(&source_dir)?;
    fs::create_dir_all(&destination)?;
    let source = source_dir.join("notes.txt");
    fs::write(&source, b"moved")?;
    let during = watch_source_during_sync(&source);
    let _guard = RemovableFlushGuard::install(root.path(), None);
    let _volumes = separate_volumes(&source_dir, &destination);

    let (_operation, events) =
        paste_move(91, &destination, &source, TransferConflict::FailIfExists);
    pump_until_transfer(&events);
    assert_eq!(
        during
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .first()
            .copied(),
        Some(true),
        "the destination flush must start before the source is deleted"
    );

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::Pasted { .. })
    ));
    assert!(!source.exists());
    assert_eq!(fs::read(destination.join("notes.txt"))?, b"moved");
    Ok(())
}

#[test]
fn cross_volume_replacing_move_keeps_the_source_when_the_removable_flush_fails()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source_dir = root.path().join("source");
    let destination = root.path().join("usb");
    fs::create_dir_all(&source_dir)?;
    fs::create_dir_all(&destination)?;
    let source = source_dir.join("notes.txt");
    fs::write(&source, b"incoming")?;
    fs::write(destination.join("notes.txt"), b"old")?;
    let _guard = RemovableFlushGuard::install(root.path(), Some(io::ErrorKind::Other));

    let _volumes = separate_volumes(&source_dir, &destination);

    let (_operation, events) =
        paste_move(92, &destination, &source, TransferConflict::ReplaceExisting);
    pump_until_transfer(&events);

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::TransferFailed { .. })
    ));
    assert!(
        !events
            .borrow()
            .iter()
            .any(|event| matches!(event, OperationEvent::Pasted { .. }))
    );
    assert_eq!(fs::read(&source)?, b"incoming");
    assert_eq!(fs::read(destination.join("notes.txt"))?, b"incoming");
    Ok(())
}

#[test]
fn cross_volume_replacing_move_flushes_the_removable_destination_before_deleting_the_source()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source_dir = root.path().join("source");
    let destination = root.path().join("usb");
    fs::create_dir_all(&source_dir)?;
    fs::create_dir_all(&destination)?;
    let source = source_dir.join("notes.txt");
    fs::write(&source, b"incoming")?;
    fs::write(destination.join("notes.txt"), b"old")?;
    let during = watch_source_during_sync(&source);
    let _guard = RemovableFlushGuard::install(root.path(), None);

    let _volumes = separate_volumes(&source_dir, &destination);

    let (_operation, events) =
        paste_move(93, &destination, &source, TransferConflict::ReplaceExisting);
    pump_until_transfer(&events);
    assert_eq!(
        during
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .first()
            .copied(),
        Some(true)
    );

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::Pasted { .. })
    ));
    assert!(!source.exists());
    assert_eq!(fs::read(destination.join("notes.txt"))?, b"incoming");
    Ok(())
}

#[test]
fn merging_move_onto_removable_media_keeps_the_source_when_the_flush_fails()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let destination = root.path().join("usb");
    fs::create_dir_all(source.join("folder"))?;
    fs::create_dir_all(destination.join("folder"))?;
    fs::write(source.join("folder/incoming.txt"), b"new")?;
    fs::write(destination.join("folder/stays.txt"), b"keep")?;
    let _guard = RemovableFlushGuard::install(root.path(), Some(io::ErrorKind::Other));

    let (_operation, events) = paste_move(
        94,
        &destination,
        &source.join("folder"),
        TransferConflict::Merge,
    );
    pump_until_transfer(&events);

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::TransferFailed { .. })
    ));
    assert!(
        !events
            .borrow()
            .iter()
            .any(|event| matches!(event, OperationEvent::Pasted { .. }))
    );
    assert_eq!(fs::read(source.join("folder/incoming.txt"))?, b"new");
    assert_eq!(fs::read(destination.join("folder/incoming.txt"))?, b"new");
    assert_eq!(fs::read(destination.join("folder/stays.txt"))?, b"keep");
    Ok(())
}

#[test]
fn merging_move_flushes_the_removable_destination_before_deleting_the_source()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let destination = root.path().join("usb");
    fs::create_dir_all(source.join("folder"))?;
    fs::create_dir_all(destination.join("folder"))?;
    fs::write(source.join("folder/incoming.txt"), b"new")?;
    fs::write(destination.join("folder/stays.txt"), b"keep")?;
    let folder = source.join("folder");
    let during = watch_source_during_sync(&folder);
    let _guard = RemovableFlushGuard::install(root.path(), None);

    let (_operation, events) = paste_move(
        95,
        &destination,
        &source.join("folder"),
        TransferConflict::Merge,
    );
    pump_until_transfer(&events);
    assert_eq!(
        during
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .first()
            .copied(),
        Some(true)
    );

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::Pasted { .. })
    ));
    assert!(!source.join("folder").exists());
    assert_eq!(fs::read(destination.join("folder/incoming.txt"))?, b"new");
    assert_eq!(fs::read(destination.join("folder/stays.txt"))?, b"keep");
    Ok(())
}

#[test]
fn same_filesystem_rename_is_not_held_for_the_removable_flush() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source_dir = root.path().join("source");
    let destination = root.path().join("usb");
    fs::create_dir_all(&source_dir)?;
    fs::create_dir_all(&destination)?;
    let source = source_dir.join("notes.txt");
    fs::write(&source, b"renamed")?;
    let during = watch_source_during_sync(&source);
    let _guard = RemovableFlushGuard::install(root.path(), Some(io::ErrorKind::Other));

    let (_operation, events) =
        paste_move(96, &destination, &source, TransferConflict::FailIfExists);
    pump_until_transfer(&events);
    let seen = during.lock().unwrap_or_else(|error| error.into_inner());
    assert!(
        !seen.is_empty() && seen.iter().all(|exists| !exists),
        "a same-filesystem rename finishes before the removable flush starts: {seen:?}"
    );

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::TransferFailed { .. })
    ));
    assert!(
        !events
            .borrow()
            .iter()
            .any(|event| matches!(event, OperationEvent::Pasted { .. }))
    );
    assert!(!source.exists());
    Ok(())
}

#[test]
fn same_filesystem_replacing_move_renames_without_waiting_for_the_flush()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source_dir = root.path().join("source");
    let destination = root.path().join("usb");
    fs::create_dir_all(&source_dir)?;
    fs::create_dir_all(&destination)?;
    let source = source_dir.join("notes.txt");
    fs::write(&source, b"incoming")?;
    fs::write(destination.join("notes.txt"), b"old")?;
    let during = watch_source_during_sync(&source);
    let _guard = RemovableFlushGuard::install(root.path(), Some(io::ErrorKind::Other));
    let _quirks = FsQuirks::install(
        root.path(),
        Quirks {
            trash: super::fs_quirks::Trash::Unsupported,
            ..Quirks::default()
        },
    );

    let (_operation, events) =
        paste_move(97, &destination, &source, TransferConflict::ReplaceExisting);
    pump_until_transfer(&events);
    let seen = during.lock().unwrap_or_else(|error| error.into_inner());
    assert!(
        !seen.is_empty() && seen.iter().all(|exists| !exists),
        "the source was renamed before the removable flush started: {seen:?}"
    );

    assert!(!source.exists());
    assert_eq!(fs::read(destination.join("notes.txt"))?, b"incoming");
    assert_eq!(fs::read_dir(&destination)?.count(), 1);
    Ok(())
}
