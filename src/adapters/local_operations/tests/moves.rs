// SPDX-License-Identifier: MIT

use std::os::unix::fs::MetadataExt;

use rustix::io::Errno;

use super::fs_quirks::{Op, Support};
use super::*;
use crate::services::{KeptCause, KeptItem, SpecialKind, TransferReport};

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
        Rc::new(|_, _, _, _| {
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
        Rc::new(|source, target, _, _| {
            Box::pin(async move {
                fs::rename(
                    source.path().expect("native source"),
                    target.path().expect("native target"),
                )
                .map(|()| super::super::Moved::Done(target))
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
        Taken::Fail,
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
    glib::MainContext::default()
        .block_on(move_local(
            gio::File::for_path(source),
            gio::File::for_path(target),
            Taken::Fail,
            gio::Cancellable::new(),
            None,
            false,
        ))
        .map(drop)
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
    for quirks in [Quirks::default(), Quirks::vboxsf()] {
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
fn items_without_noreplace_or_a_usable_link_are_renamed_without_copying()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // As protected_hardlinks refuses another user's file that a rename could move.
    let refused = || Quirks {
        fail: vec![(Op::Link, "moved", 0, Errno::PERM)],
        ..Quirks::without_rename_flags()
    };
    let linked_before_a_probe = || Quirks {
        rename_flags: Support::Rejected(Errno::NOSYS),
        ..Quirks::sshfs()
    };
    let cases: [(&str, Preset, bool); 6] = [
        ("refused", refused, false),
        ("sshfs", Quirks::sshfs, false),
        ("sshfs", Quirks::sshfs, true),
        ("linked before a probe", linked_before_a_probe, false),
        ("vboxsf", Quirks::vboxsf, false),
        ("vboxsf", Quirks::vboxsf, true),
    ];
    for (case, quirks, folder) in cases {
        let root = tempfile::tempdir()?;
        let _scope = FsQuirks::install(root.path(), quirks());
        let source = root.path().join("item");
        let moved = root.path().join("moved");
        if folder {
            fs::create_dir(&source)?;
            fs::write(source.join("inside.txt"), b"inside")?;
        } else {
            fs::write(&source, b"notes")?;
        }
        let inode = fs::metadata(&source)?.ino();
        let progress =
            TransferProgressTracker::new(OperationRequestId(1), None, None, Rc::new(|_| {}));

        glib::MainContext::default()
            .block_on(move_local(
                gio::File::for_path(&source),
                gio::File::for_path(&moved),
                Taken::Fail,
                gio::Cancellable::new(),
                Some(progress.clone()),
                false,
            ))
            .map_err(|error| format!("{case}: {error}"))?;

        let case = format!("{case}, folder: {folder}");
        assert_eq!(fs::metadata(&moved)?.ino(), inode, "{case}");
        assert_eq!(progress.completed_files.get(), 0, "{case}");
        assert_eq!(progress.transferred_bytes.get(), 0, "{case}");
        assert_eq!(names(root.path())?, ["moved"], "{case}");
    }
    Ok(())
}

#[test]
fn items_no_step_can_place_are_moved_by_copying() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // As SFTP without `posix-rename` refuses to rename over the reserved file.
    let rename_over_refused = || Quirks {
        fail: vec![(Op::Rename, "moved", 1, Errno::PERM)],
        ..Quirks::sshfs()
    };
    let cases: [(&str, Preset, bool); 3] = [
        ("links unsettled", Quirks::without_placement, false),
        ("rename over refused", rename_over_refused, false),
        (
            "folders unplaceable",
            Quirks::without_folder_placement,
            true,
        ),
    ];
    for (case, quirks, folder) in cases {
        let root = tempfile::tempdir()?;
        let _scope = FsQuirks::install(root.path(), quirks());
        let source = root.path().join("item");
        let moved = root.path().join("moved");
        if folder {
            fs::create_dir(&source)?;
            fs::write(source.join("inside.txt"), b"inside")?;
        } else {
            fs::write(&source, b"inside")?;
        }
        let inode = fs::metadata(&source)?.ino();

        move_native(&source, &moved).map_err(|error| format!("{case}: {error}"))?;

        assert_ne!(fs::metadata(&moved)?.ino(), inode, "{case}");
        let contents = if folder {
            moved.join("inside.txt")
        } else {
            moved.clone()
        };
        assert_eq!(fs::read(contents)?, b"inside", "{case}");
        assert_eq!(names(root.path())?, ["moved"], "{case}");
    }
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
        Taken::Fail,
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
        Taken::Fail,
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

#[test]
fn keep_both_move_renumbers_when_its_name_is_taken_meanwhile() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for across_volumes in [false, true] {
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let destination = root.path().join("destination");
        fs::create_dir_all(&sources)?;
        fs::create_dir_all(&destination)?;
        let source = sources.join("report.txt");
        fs::write(&source, b"incoming")?;
        fs::write(destination.join("report.txt"), b"existing")?;
        let taken = destination.join("report (1).txt");
        let racer = |taken: PathBuf| -> super::fs_quirks::Racer {
            Box::new(move || {
                if fs::symlink_metadata(&taken).is_err() {
                    fs::write(&taken, b"taken meanwhile").expect("take the name");
                }
            })
        };
        let racing = || Quirks {
            before: vec![
                (Op::Rename, "report (1).txt", 0, racer(taken.clone())),
                (Op::LinkFd, "report (1).txt", 0, racer(taken.clone())),
            ],
            ..Quirks::default()
        };
        let _scopes = if across_volumes {
            vec![
                FsQuirks::install(&sources, Quirks::default()),
                FsQuirks::install(&destination, racing()),
            ]
        } else {
            vec![FsQuirks::install(root.path(), racing())]
        };

        let events = run_paste(PasteRequest {
            id: OperationRequestId(78),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&source),
                conflict: TransferConflict::KeepBoth,
            }],
            move_sources: true,
        });

        let moved = destination.join("report (2).txt");
        assert_eq!(
            item_moves(&events),
            [(Location::local(&source), Location::local(&moved))],
            "across volumes: {across_volumes}"
        );
        assert_eq!(fs::read(&taken)?, b"taken meanwhile");
        assert_eq!(fs::read(&moved)?, b"incoming");
        assert!(!source.exists());
    }
    Ok(())
}

/// What the transfer reported about items inside the ones it moved.
fn reported(events: &RefCell<Vec<OperationEvent>>) -> TransferReport {
    let mut reported = TransferReport::default();
    for event in events.borrow().iter() {
        if let OperationEvent::ItemsReported { report, .. } = event {
            reported.extend(report.clone());
        }
    }
    reported
}

fn kept(location: PathBuf, cause: KeptCause) -> KeptItem {
    KeptItem {
        location: Location::local(location),
        cause,
    }
}

/// A destination for the move of `folder` with `conflict`, where an item of
/// that name exists unless the move creates it.
fn destination_for(
    root: &Path,
    conflict: TransferConflict,
) -> Result<(PathBuf, FsQuirks), Box<dyn Error>> {
    let destination = root.join("destination");
    fs::create_dir_all(&destination)?;
    match conflict {
        TransferConflict::Merge => fs::create_dir(destination.join("folder"))?,
        TransferConflict::ReplaceExisting => {
            fs::create_dir(destination.join("folder"))?;
            fs::write(destination.join("folder/old.txt"), b"old")?;
        }
        _ => {}
    }
    let quirks = Quirks {
        trash: super::fs_quirks::Trash::Unsupported,
        ..Quirks::default()
    };
    let scope = FsQuirks::install(&destination, quirks);
    Ok((destination, scope))
}

const MOVES: [TransferConflict; 3] = [
    TransferConflict::FailIfExists,
    TransferConflict::Merge,
    TransferConflict::ReplaceExisting,
];

#[test]
fn a_move_leaves_special_files_it_cannot_copy_at_the_source() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for conflict in MOVES {
        let root = tempfile::tempdir()?;
        let source_dir = root.path().join("source");
        let folder = source_dir.join("folder");
        fs::create_dir_all(&folder)?;
        fs::write(folder.join("notes.txt"), b"keep-me")?;
        let _socket = std::os::unix::net::UnixListener::bind(folder.join("socket"))?;
        let _source = FsQuirks::install(&source_dir, Quirks::default());
        let (destination, _destination) = destination_for(root.path(), conflict)?;

        let (_operation, events) = paste_move(91, &destination, &folder, conflict);
        pump_until_transfer(&events);

        assert!(
            matches!(
                terminal_transfer(&events.borrow()),
                Some(OperationEvent::Pasted { .. })
            ),
            "{conflict:?}: {:?}",
            events.borrow()
        );
        assert_eq!(
            names(&destination.join("folder"))?,
            ["notes.txt"],
            "{conflict:?}"
        );
        assert_eq!(names(&folder)?, ["socket"], "{conflict:?}");
        assert_eq!(
            reported(&events),
            TransferReport {
                kept: vec![kept(
                    folder.join("socket"),
                    KeptCause::Skipped(SpecialKind::Socket)
                )],
                ..TransferReport::default()
            },
            "{conflict:?}"
        );
    }
    Ok(())
}

#[test]
fn a_move_keeps_source_entries_changed_or_added_after_their_copy() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for conflict in MOVES {
        let root = tempfile::tempdir()?;
        let source_dir = root.path().join("source");
        let folder = source_dir.join("folder");
        fs::create_dir_all(folder.join("sub"))?;
        fs::write(folder.join("a.txt"), b"a")?;
        fs::write(folder.join("sub/b.txt"), b"b")?;
        let changed = folder.join("sub/b.txt");
        let added = folder.join("added.txt");
        let (rewrite, add) = (changed.clone(), added.clone());
        // Once the copy is done, as the deletion opens the moved folder.
        let racer: super::fs_quirks::Racer = Box::new(move || {
            fs::write(&rewrite, b"rewritten").expect("rewrite a copied entry");
            fs::write(&add, b"added").expect("add an entry");
        });
        let _source = FsQuirks::install(
            &source_dir,
            Quirks {
                before: vec![(Op::OpenDir, "folder", 1, racer)],
                ..Quirks::default()
            },
        );
        let (destination, _destination) = destination_for(root.path(), conflict)?;

        let (_operation, events) = paste_move(92, &destination, &folder, conflict);
        pump_until_transfer(&events);

        assert!(
            matches!(
                terminal_transfer(&events.borrow()),
                Some(OperationEvent::Pasted { .. })
            ),
            "{conflict:?}: {:?}",
            events.borrow()
        );
        let moved = destination.join("folder");
        assert_eq!(fs::read(moved.join("a.txt"))?, b"a", "{conflict:?}");
        assert_eq!(fs::read(moved.join("sub/b.txt"))?, b"b", "{conflict:?}");
        assert_eq!(names(&folder)?, ["added.txt", "sub"], "{conflict:?}");
        assert_eq!(fs::read(&changed)?, b"rewritten", "{conflict:?}");
        assert_eq!(fs::read(&added)?, b"added", "{conflict:?}");
        assert_eq!(
            reported(&events).kept,
            [kept(changed, KeptCause::Changed)],
            "{conflict:?}"
        );
    }
    Ok(())
}

#[test]
fn a_file_written_during_its_own_move_stays_at_the_source() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let rewrite = |path: &Path, contents: &'static [u8]| -> super::fs_quirks::Racer {
        let path = path.to_path_buf();
        Box::new(move || fs::write(&path, contents).expect("rewrite the source"))
    };
    // A linked file whose names are claimed, as names that may collide are
    // on NFS, and the moved file itself.
    let cases: [(&str, usize, Preset); 4] = [
        ("in a folder", 1, Quirks::default),
        ("in a folder", 2, Quirks::default),
        ("linked", 2, Quirks::nfs3),
        ("selected", 2, Quirks::default),
    ];
    for (kind, rewrites, destination_quirks) in cases {
        let case = format!("{kind}, rewrites: {rewrites}");
        let root = tempfile::tempdir()?;
        let source_dir = root.path().join("source");
        let folder = source_dir.join("folder");
        fs::create_dir_all(&folder)?;
        let db = folder.join("db");
        fs::write(&db, b"first")?;
        let reads = if kind == "linked" {
            fs::hard_link(&db, folder.join("DB"))?;
            "*"
        } else {
            fs::write(folder.join("notes.txt"), b"notes")?;
            "db"
        };
        // Each pass reads the data, then the end of the file.
        let mut before = vec![(Op::Read, reads, 0, rewrite(&db, b"second write"))];
        if rewrites == 2 {
            before.push((Op::Read, reads, 1, rewrite(&db, b"third")));
        }
        let _source = FsQuirks::install(
            &source_dir,
            Quirks {
                before,
                ..Quirks::default()
            },
        );
        let destination = root.path().join("destination");
        fs::create_dir_all(&destination)?;
        let _destination = FsQuirks::install(&destination, destination_quirks());
        let moved = if kind == "selected" { &db } else { &folder };

        let (_operation, events) =
            paste_move(93, &destination, moved, TransferConflict::FailIfExists);
        pump_until_transfer(&events);

        let terminal = terminal_transfer(&events.borrow()).cloned();
        if kind == "selected" {
            let Some(OperationEvent::TransferFailed { message, .. }) = terminal else {
                panic!("{case}: {:?}", events.borrow());
            };
            assert_eq!(
                message,
                rust_i18n::t!("%{name} changed while it was being copied", name = "db"),
                "{case}"
            );
            assert!(names(&destination)?.is_empty(), "{case}");
            assert_eq!(fs::read(&db)?, b"third", "{case}");
            continue;
        }
        assert!(
            matches!(terminal, Some(OperationEvent::Pasted { .. })),
            "{case}: {:?}",
            events.borrow()
        );
        let mut left = reported(&events).kept;
        left.sort_by_key(|item| item.location.native_path().map(Path::to_path_buf));
        if rewrites == 1 {
            assert_eq!(
                fs::read(destination.join("folder/db"))?,
                b"second write",
                "{case}"
            );
            assert!(!folder.exists(), "{case}");
            assert!(left.is_empty(), "{case}");
        } else if kind == "linked" {
            assert!(names(&destination.join("folder"))?.is_empty(), "{case}");
            assert_eq!(names(&folder)?, ["DB", "db"], "{case}");
            assert_eq!(fs::read(&db)?, b"third", "{case}");
            let changed = |name| kept(folder.join(name), KeptCause::Changed);
            assert_eq!(left, [changed("DB"), changed("db")], "{case}");
        } else {
            assert_eq!(names(&destination.join("folder"))?, ["notes.txt"], "{case}");
            assert_eq!(names(&folder)?, ["db"], "{case}");
            assert_eq!(fs::read(&db)?, b"third", "{case}");
            assert_eq!(left, [kept(db.clone(), KeptCause::Changed)], "{case}");
        }
    }
    Ok(())
}

#[test]
fn moving_hard_linked_files_across_volumes_deletes_every_name() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source_dir = root.path().join("source");
    let folder = source_dir.join("folder");
    fs::create_dir_all(folder.join("sub"))?;
    fs::write(folder.join("a"), b"linked")?;
    fs::hard_link(folder.join("a"), folder.join("sub/b"))?;
    let outside = source_dir.join("outside");
    fs::write(&outside, b"outside")?;
    fs::hard_link(&outside, folder.join("c"))?;
    let destination = root.path().join("destination");
    fs::create_dir_all(&destination)?;
    let _volumes = separate_volumes(&source_dir, &destination);

    let (_operation, events) =
        paste_move(94, &destination, &folder, TransferConflict::FailIfExists);
    pump_until_transfer(&events);

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::Pasted { .. })
    ));
    let moved = destination.join("folder");
    let (a, b) = (
        fs::metadata(moved.join("a"))?,
        fs::metadata(moved.join("sub/b"))?,
    );
    assert_eq!((a.ino(), a.nlink()), (b.ino(), 2));
    assert_eq!(fs::read(moved.join("c"))?, b"outside");
    assert!(!folder.exists());
    assert_eq!(fs::metadata(&outside)?.nlink(), 1);
    assert_eq!(reported(&events), TransferReport::default());
    Ok(())
}

#[test]
fn moves_compare_inode_numbers_only_where_the_source_keeps_them() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let mounted = |fs_type, options| {
        move || Quirks {
            mount_type: Some((fs_type, options)),
            ..Quirks::default()
        }
    };
    let cases: [(&str, Box<dyn Fn() -> Quirks>, bool); 4] = [
        ("sshfs", Box::new(Quirks::sshfs), false),
        (
            "cifs noserverino",
            Box::new(mounted("cifs", "rw,noserverino")),
            false,
        ),
        ("cifs", Box::new(mounted("cifs", "rw,serverino")), true),
        ("vfat", Box::new(mounted("vfat", "rw,flush")), false),
    ];
    for (case, quirks, kept_at_source) in cases {
        let root = tempfile::tempdir()?;
        let source_dir = root.path().join("source");
        let folder = source_dir.join("folder");
        fs::create_dir_all(folder.join("sub"))?;
        fs::write(folder.join("a.txt"), b"a")?;
        fs::write(folder.join("sub/b.txt"), b"b")?;
        let evicted = [
            folder.clone(),
            folder.join("a.txt"),
            folder.join("sub"),
            folder.join("sub/b.txt"),
        ];
        // The kernel evicts every entry between the copy and the deletion.
        let racer: super::fs_quirks::Racer = Box::new(move || {
            for path in &evicted {
                super::fs_quirks::renumbered(path);
            }
        });
        let _source = FsQuirks::install(
            &source_dir,
            Quirks {
                before: vec![(Op::OpenDir, "folder", 1, racer)],
                ..quirks()
            },
        );
        let destination = root.path().join("destination");
        fs::create_dir_all(&destination)?;
        let _destination = FsQuirks::install(&destination, Quirks::default());

        let (_operation, events) =
            paste_move(95, &destination, &folder, TransferConflict::FailIfExists);
        pump_until_transfer(&events);

        assert!(
            matches!(
                terminal_transfer(&events.borrow()),
                Some(OperationEvent::Pasted { .. })
            ),
            "{case}: {:?}",
            events.borrow()
        );
        assert_eq!(
            fs::read(destination.join("folder/sub/b.txt"))?,
            b"b",
            "{case}"
        );
        assert_eq!(folder.exists(), kept_at_source, "{case}");
        let expected = if kept_at_source {
            vec![kept(folder.clone(), KeptCause::Changed)]
        } else {
            Vec::new()
        };
        assert_eq!(reported(&events).kept, expected, "{case}");
    }
    Ok(())
}

#[test]
fn a_source_entry_that_cannot_be_deleted_is_kept_with_the_reason() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source_dir = root.path().join("source");
    let folder = source_dir.join("folder");
    fs::create_dir_all(folder.join("read-only"))?;
    fs::write(folder.join("a.txt"), b"a")?;
    let child = folder.join("read-only/child.txt");
    fs::write(&child, b"child")?;
    // As in a folder whose mode is 0555, for any user but root.
    let _source = FsQuirks::install(
        &source_dir,
        Quirks {
            fail: vec![(Op::Unlink, "child.txt", 0, Errno::ACCESS)],
            ..Quirks::default()
        },
    );
    let destination = root.path().join("destination");
    fs::create_dir_all(&destination)?;
    let _destination = FsQuirks::install(&destination, Quirks::default());

    let (_operation, events) =
        paste_move(96, &destination, &folder, TransferConflict::FailIfExists);
    pump_until_transfer(&events);

    assert!(matches!(
        terminal_transfer(&events.borrow()),
        Some(OperationEvent::Pasted { .. })
    ));
    assert_eq!(
        fs::read(destination.join("folder/read-only/child.txt"))?,
        b"child"
    );
    assert_eq!(names(&folder)?, ["read-only"]);
    assert_eq!(fs::read(&child)?, b"child");
    let reason = super::super::os_error_detail(Errno::ACCESS);
    assert_eq!(
        reported(&events).kept,
        [kept(child, KeptCause::Undeletable(reason))]
    );
    Ok(())
}

#[test]
fn moving_to_a_remote_target_keeps_a_source_entry_changed_during_the_copy()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let folder = root.path().join("folder");
    fs::create_dir_all(&folder)?;
    fs::write(folder.join("a.txt"), b"a")?;
    fs::write(folder.join("b.txt"), b"b")?;
    let stand_in = root.path().join("remote");
    let target = gio::File::for_uri("strata-test-remote:///folder");
    assert!(!target.is_native());
    let (changed, copied_to) = (folder.join("b.txt"), stand_in.clone());
    // GIO copies to the remote target, and the source changes meanwhile.
    let remote_copy: super::super::RemoteCopy = Rc::new(move |source, target, _, cancellable| {
        let (changed, copied_to) = (changed.clone(), copied_to.clone());
        Box::pin(async move {
            copy_recursively(source, gio::File::for_path(copied_to), false, cancellable).await?;
            fs::write(&changed, b"changed").map_err(io_error)?;
            Ok(target)
        })
    });
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let progress = TransferProgressTracker::new(
        OperationRequestId(1),
        None,
        None,
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );

    let moved = glib::MainContext::default().block_on(super::super::move_local_with_progress(
        gio::File::for_path(&folder),
        target.clone(),
        Taken::Fail,
        gio::Cancellable::new(),
        super::super::Copying {
            progress: Some(progress),
            fat_family: false,
        },
        super::super::attempt_move(None),
        remote_copy,
    ))?;

    assert!(moved.equal(&target));
    assert_eq!(fs::read(stand_in.join("a.txt"))?, b"a");
    assert_eq!(fs::read(stand_in.join("b.txt"))?, b"b");
    assert_eq!(names(&folder)?, ["b.txt"]);
    assert_eq!(
        reported(&events).kept,
        [kept(folder.join("b.txt"), KeptCause::Changed)]
    );
    Ok(())
}
