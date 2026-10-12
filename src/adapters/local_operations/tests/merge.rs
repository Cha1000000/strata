// SPDX-License-Identifier: MIT

use rustix::io::Errno;

use super::fs_quirks::{Op, Trash};
use super::*;
use crate::adapters::volume::learn_mount_capability;

fn real_copy() -> StageCopy {
    Rc::new(|source, target, _directory, cancellable| {
        copy_recursively(source, target, true, cancellable, None)
    })
}

/// Substitutes Trash staging with a rename into `staging`, recording the
/// staged locations in call order.
fn rename_stage(staging: PathBuf) -> (StageOverwrite, Rc<RefCell<Vec<Location>>>) {
    fs::create_dir_all(&staging).expect("staging dir");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = calls.clone();
    let counter = Rc::new(Cell::new(0usize));
    let stage: StageOverwrite = Rc::new(move |location: Location, _cancellable| {
        recorded.borrow_mut().push(location.clone());
        let index = counter.get();
        counter.set(index + 1);
        let staging = staging.clone();
        Box::pin(async move {
            let path = location.native_path().ok_or_else(|| {
                glib::Error::new(gio::IOErrorEnum::Failed, "no local path to stage")
            })?;
            std::fs::rename(path, staging.join(format!("staged-{index}")))
                .map_err(|error| glib::Error::new(gio::IOErrorEnum::Failed, &error.to_string()))?;
            Ok(())
        })
    });
    (stage, calls)
}

#[test]
fn fat_merge_backs_up_sanitized_collisions_and_records_actual_paths() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("nested?"))?;
    fs::create_dir_all(target.join("nested_"))?;
    fs::write(source.join("nested?/a?.txt"), b"question")?;
    fs::write(source.join("nested?/a:.txt"), b"colon")?;
    fs::write(target.join("nested_/a_.txt"), b"original")?;
    let staging = root.path().join("staging");
    let (stage, staged) = rename_stage(staging.clone());
    let (plans, on_merged) = recorded_plans();
    glib::MainContext::default().block_on(merge_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
        MergeHooks {
            fat_family: true,
            copy_into_target: Rc::new(|source, target, _, cancellable| {
                copy_recursively_fat_family(source, target, true, cancellable, None)
            }),
            stage_overwrite: stage,
            on_merged: &on_merged,
        },
    ))?;
    assert_eq!(fs::read(staging.join("staged-0"))?, b"original");
    assert_eq!(fs::read(target.join("nested_/a_.txt"))?, b"colon");
    assert_eq!(fs::read(target.join("nested_/a_ (1).txt"))?, b"question");
    assert_eq!(
        *staged.borrow(),
        vec![Location::local(target.join("nested_/a_.txt"))]
    );
    let plans = plans.borrow();
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].created,
        vec![Location::local(target.join("nested_/a_ (1).txt"))]
    );
    assert_eq!(plans[0].overwritten, *staged.borrow());
    Ok(())
}

fn no_plan(_: MergePlan) {}

fn recorded_plans() -> (Rc<RefCell<Vec<MergePlan>>>, impl Fn(MergePlan)) {
    let plans = Rc::new(RefCell::new(Vec::new()));
    let recorded = plans.clone();
    (plans, move |plan| recorded.borrow_mut().push(plan))
}

#[test]
fn merging_folders_unites_contents_and_overwrites_shared_names() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(&source)?;
    fs::create_dir_all(&target)?;
    fs::write(source.join("incoming.txt"), b"new")?;
    fs::write(source.join("shared.txt"), b"incoming wins")?;
    fs::write(target.join("stays.txt"), b"keep me")?;
    fs::write(target.join("shared.txt"), b"old")?;
    let staging = root.path().join("staging");
    let (stage, staged) = rename_stage(staging.clone());
    let (plans, on_merged) = recorded_plans();

    let result = glib::MainContext::default().block_on(merge_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
        MergeHooks {
            fat_family: false,
            copy_into_target: real_copy(),
            stage_overwrite: stage,
            on_merged: &on_merged,
        },
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("incoming.txt"))?, b"new");
    assert_eq!(fs::read(target.join("shared.txt"))?, b"incoming wins");
    assert_eq!(fs::read(target.join("stays.txt"))?, b"keep me");
    assert!(source.exists(), "a copy merge keeps the source");
    assert_eq!(
        staged.borrow().as_slice(),
        &[Location::local(target.join("shared.txt"))],
        "the overwritten original is staged before the copy"
    );
    assert_eq!(fs::read(staging.join("staged-0"))?, b"old");
    let plans = plans.borrow();
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].created,
        vec![Location::local(target.join("incoming.txt"))]
    );
    assert_eq!(
        plans[0].overwritten,
        vec![Location::local(target.join("shared.txt"))]
    );
    Ok(())
}

#[test]
fn merging_recurses_into_subdirectories_shared_by_both_sides() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("nested"))?;
    fs::create_dir_all(target.join("nested"))?;
    fs::write(source.join("nested/incoming.txt"), b"new")?;
    fs::write(source.join("nested/shared.txt"), b"incoming wins")?;
    fs::write(target.join("nested/stays.txt"), b"keep me")?;
    fs::write(target.join("nested/shared.txt"), b"old")?;
    let (stage, _) = rename_stage(root.path().join("staging"));
    let (plans, on_merged) = recorded_plans();

    let result = glib::MainContext::default().block_on(merge_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
        MergeHooks {
            fat_family: false,
            copy_into_target: real_copy(),
            stage_overwrite: stage,
            on_merged: &on_merged,
        },
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("nested/incoming.txt"))?, b"new");
    assert_eq!(
        fs::read(target.join("nested/shared.txt"))?,
        b"incoming wins"
    );
    assert_eq!(fs::read(target.join("nested/stays.txt"))?, b"keep me");
    let plans = plans.borrow();
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].created,
        vec![Location::local(target.join("nested/incoming.txt"))]
    );
    assert_eq!(
        plans[0].overwritten,
        vec![Location::local(target.join("nested/shared.txt"))]
    );
    Ok(())
}

#[test]
fn a_new_subdirectory_is_reported_as_a_single_created_entry() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("sub/deep"))?;
    fs::create_dir_all(&target)?;
    fs::write(source.join("sub/deep/incoming.txt"), b"new")?;
    let (plans, on_merged) = recorded_plans();

    let result = glib::MainContext::default().block_on(merge_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        &on_merged,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("sub/deep/incoming.txt"))?, b"new");
    let plans = plans.borrow();
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].created,
        vec![Location::local(target.join("sub"))],
        "undo removes the topmost created directory, not each nested child"
    );
    assert!(plans[0].overwritten.is_empty());
    Ok(())
}

#[test]
fn a_staging_failure_aborts_the_merge_and_reports_only_what_was_staged()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(&source)?;
    fs::create_dir_all(&target)?;
    fs::write(source.join("a.txt"), b"new a")?;
    fs::write(source.join("b.txt"), b"new b")?;
    fs::write(target.join("a.txt"), b"old a")?;
    fs::write(target.join("b.txt"), b"old b")?;
    let staging = root.path().join("staging");
    fs::create_dir_all(&staging)?;
    let staging_into = staging.clone();
    let (plans, on_merged) = recorded_plans();
    let staged_once = Rc::new(Cell::new(false));
    let flagged = staged_once.clone();
    let stage: StageOverwrite = Rc::new(move |location: Location, _cancellable| {
        let staging = staging_into.clone();
        let flagged = flagged.clone();
        Box::pin(async move {
            if flagged.replace(true) {
                return Err(glib::Error::new(
                    gio::IOErrorEnum::Failed,
                    "injected staging failure",
                ));
            }
            let path = location
                .native_path()
                .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "no path"))?;
            std::fs::rename(path, staging.join("staged"))
                .map_err(|error| glib::Error::new(gio::IOErrorEnum::Failed, &error.to_string()))?;
            Ok(())
        })
    });

    let result = glib::MainContext::default().block_on(merge_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
        MergeHooks {
            fat_family: false,
            copy_into_target: real_copy(),
            stage_overwrite: stage,
            on_merged: &on_merged,
        },
    ));

    assert!(result.is_err());
    assert_eq!(fs::read(source.join("a.txt"))?, b"new a");
    assert_eq!(fs::read(source.join("b.txt"))?, b"new b");
    let moved_out = ["a.txt", "b.txt"]
        .iter()
        .filter(|name| !target.join(name).exists())
        .count();
    assert_eq!(
        moved_out, 1,
        "enumeration order is unspecified: exactly one original was staged"
    );
    let staged_contents = fs::read(staging.join("staged"))?;
    assert!(
        staged_contents == b"old a" || staged_contents == b"old b",
        "the staged file keeps its original content"
    );
    let plans = plans.borrow();
    assert_eq!(plans.len(), 1);
    assert!(plans[0].created.is_empty());
    assert_eq!(plans[0].overwritten.len(), 1, "only the staged original");
    Ok(())
}

#[test]
fn a_merge_move_removes_the_emptied_source() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(&source)?;
    fs::create_dir_all(&target)?;
    fs::write(source.join("incoming.txt"), b"new")?;
    fs::write(target.join("stays.txt"), b"keep me")?;

    let result = glib::MainContext::default().block_on(merge_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        true,
        gio::Cancellable::new(),
        &no_plan,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("incoming.txt"))?, b"new");
    assert_eq!(fs::read(target.join("stays.txt"))?, b"keep me");
    assert!(!source.exists());
    Ok(())
}

#[test]
fn merging_a_file_is_rejected_without_touching_either_side() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.txt");
    let target = root.path().join("target.txt");
    fs::write(&source, b"new")?;
    fs::write(&target, b"old")?;

    let result = glib::MainContext::default().block_on(merge_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        &no_plan,
    ));

    assert!(result.is_err());
    assert_eq!(fs::read(&source)?, b"new");
    assert_eq!(fs::read(&target)?, b"old");
    Ok(())
}

#[test]
fn merging_onto_a_file_is_rejected_without_touching_either_side() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(&source)?;
    fs::write(source.join("incoming.txt"), b"new")?;
    fs::write(&target, b"old")?;

    let result = glib::MainContext::default().block_on(merge_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        &no_plan,
    ));

    assert!(result.is_err());
    assert!(source.join("incoming.txt").exists());
    assert_eq!(fs::read(&target)?, b"old");
    Ok(())
}

#[test]
fn a_failed_merge_preserves_the_destinations_existing_contents() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(&source)?;
    fs::create_dir_all(&target)?;
    fs::write(source.join("incoming.txt"), b"new")?;
    fs::write(target.join("stays.txt"), b"keep me")?;

    let result = glib::MainContext::default().block_on(merge_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
        MergeHooks {
            fat_family: false,
            copy_into_target: Rc::new(|_, target, _directory, _| {
                Box::pin(async move {
                    fs::write(
                        target
                            .path()
                            .ok_or_else(|| super::io_error("missing target path"))?
                            .join("partial.txt"),
                        b"partial",
                    )
                    .map_err(super::io_error)?;
                    Err(glib::Error::new(
                        gio::IOErrorEnum::Cancelled,
                        "injected cancellation",
                    ))
                })
            }),
            stage_overwrite: Rc::new(trash_stage_overwrite),
            on_merged: &no_plan,
        },
    ));

    assert!(result.is_err());
    assert_eq!(fs::read(target.join("stays.txt"))?, b"keep me");
    assert!(source.join("incoming.txt").exists());
    Ok(())
}

#[test]
fn a_merged_paste_reports_no_created_location() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let destination = root.path().join("destination");
    fs::create_dir_all(sources.join("folder"))?;
    fs::create_dir_all(destination.join("folder"))?;
    fs::write(sources.join("folder/incoming.txt"), b"new")?;
    fs::write(destination.join("folder/stays.txt"), b"keep me")?;

    // No created location: copy undo trashes recorded locations, and the
    // merged destination held pre-existing contents.
    let created = run_paste_collecting_created(PasteRequest {
        id: OperationRequestId(81),
        destination: Location::local(&destination),
        items: vec![PasteItem {
            source: Location::local(sources.join("folder")),
            conflict: TransferConflict::Merge,
        }],
        move_sources: false,
    })?;

    assert!(created.into_iter().flatten().next().is_none());
    assert_eq!(fs::read(destination.join("folder/incoming.txt"))?, b"new");
    assert_eq!(fs::read(destination.join("folder/stays.txt"))?, b"keep me");
    Ok(())
}

fn staged_trash_fixture(
    root: &Path,
    name: &str,
    original: &Path,
    contents: &[u8],
) -> Result<RestoreEntry, Box<dyn Error>> {
    let uid = rustix::process::getuid().as_raw();
    let trash = root.join(format!(".Trash-{uid}"));
    fs::create_dir_all(trash.join("files"))?;
    fs::create_dir_all(trash.join("info"))?;
    let staged = trash.join("files").join(name);
    fs::write(&staged, contents)?;
    let info = trash.join("info").join(format!("{name}.trashinfo"));
    fs::write(
        &info,
        format!(
            "[Trash Info]\nPath={}\nDeletionDate=2026-01-01T00:00:00\n",
            original.display()
        ),
    )?;
    Ok(RestoreEntry {
        source: Location::local(&staged),
        display_name: name.to_owned(),
        original_target: Some(Location::local(original)),
        trash_info: Some(info),
        confirmed_destination: None,
        physical_path: Some(staged),
    })
}

fn staged_lookup(entries: HashMap<PathBuf, RestoreEntry>) -> StagedOriginalLookup {
    let entries = RefCell::new(entries);
    Rc::new(move |location, _cancellable| {
        let entry = location
            .native_path()
            .and_then(|path| entries.borrow_mut().remove(path));
        Box::pin(async move {
            entry.ok_or_else(|| {
                glib::Error::new(
                    gio::IOErrorEnum::NotFound,
                    "The original is no longer in Trash",
                )
            })
        })
    })
}

#[test]
fn merge_undo_uses_recorded_identity_when_trash_dates_collide() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for chosen in ["older.zip", "current.zip", "removed.zip"] {
        let root = tempfile::tempdir()?;
        let target = root.path().join("archive.zip");
        fs::write(&target, b"incoming archive")?;
        let mut staged = HashMap::new();
        for name in ["older.zip", "current.zip", "removed.zip"] {
            let entry = staged_trash_fixture(root.path(), name, &target, name.as_bytes())?;
            staged.insert(
                name,
                entry.source.native_path().expect("staged path").to_owned(),
            );
        }
        let original = TrashedOriginal::from_metadata(&fs::symlink_metadata(&staged[chosen])?);
        if chosen == "removed.zip" {
            fs::remove_file(&staged[chosen])?;
        }
        let trash_root = staged[chosen]
            .parent()
            .expect("trash files directory")
            .parent()
            .expect("trash root")
            .to_owned();
        let location = Location::local(&target);
        let originals = HashMap::from([(location.clone(), original)]);
        let lookup: StagedOriginalLookup = Rc::new(move |location, cancellable| {
            let path = location.native_path().expect("local original").to_owned();
            let entry = home_trash_entries_at(
                &trash_root,
                &HashSet::from([path.clone()]),
                &cancellable,
                &originals,
            )
            .remove(&path);
            Box::pin(async move { entry.ok_or_else(|| io_error("Recorded original is missing")) })
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();
        glib::MainContext::default().block_on(run_merge_undo(
            OperationRequestId(91),
            Vec::new(),
            vec![location],
            Rc::new(move |event| emitted.borrow_mut().push(event)),
            gio::Cancellable::new(),
            lookup,
        ));

        if chosen == "removed.zip" {
            assert_eq!(fs::read(&target)?, b"incoming archive");
            assert!(matches!(
                events.borrow().last(),
                Some(OperationEvent::CompletedWithErrors { .. })
            ));
        } else {
            assert_eq!(fs::read(&target)?, chosen.as_bytes());
            assert!(matches!(
                events.borrow().last(),
                Some(OperationEvent::Restored { .. })
            ));
        }
        for (name, path) in &staged {
            if *name != chosen {
                assert_eq!(fs::read(path)?, name.as_bytes());
            }
        }
    }
    Ok(())
}

#[test]
fn merge_undo_restores_staged_originals_and_removes_created() {
    crate::test_support::gtk_test(
        "adapters::local_operations::tests::merge::merge_undo_restores_staged_originals_and_removes_created",
        || check_merge_undo_restores_staged_originals_and_removes_created().expect("merge undo"),
    );
}

fn check_merge_undo_restores_staged_originals_and_removes_created() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let target = root.path().join("target");
    fs::create_dir_all(&target)?;
    let shared = target.join("shared.txt");
    let incoming = target.join("incoming.txt");
    let stays = target.join("stays.txt");
    fs::write(&shared, b"incoming wins")?;
    fs::write(&incoming, b"new")?;
    fs::write(&stays, b"keep me")?;
    let bookmarks = crate::adapters::bookmarks::pinned_places_path();
    fs::create_dir_all(bookmarks.parent().expect("bookmarks directory"))?;
    let pin = |path: &Path| format!("{} Pin\n", gio::File::for_path(path).uri());
    fs::write(
        &bookmarks,
        format!("{}{}{}", pin(&shared), pin(&incoming), pin(&stays)),
    )?;
    let incoming_identity = TrashedOriginal::at_path(&incoming).ok_or("identity")?;
    let staged = staged_trash_fixture(root.path(), "shared.txt", &shared, b"old")?;
    let staged_path = staged
        .source
        .native_path()
        .expect("staged path")
        .to_path_buf();
    let lookup = staged_lookup(HashMap::from([(shared.clone(), staged)]));

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(90),
        vec![Location::local(&incoming)],
        vec![Location::local(&shared)],
        Rc::new(move |event| emitted.borrow_mut().push(event)),
        gio::Cancellable::new(),
        lookup,
    ));

    assert!(
        matches!(
            events.borrow().last(),
            Some(OperationEvent::Restored { .. })
        ),
        "{:?}",
        events.borrow()
    );
    assert_eq!(
        fs::read(&shared)?,
        b"old",
        "the staged original is restored"
    );
    assert!(!incoming.exists(), "the merge-created file is removed");
    assert_eq!(
        trashed_items(&events.borrow()),
        [(Location::local(&incoming), incoming_identity)]
    );
    assert!(!staged_path.exists(), "the staged copy left the fake trash");
    assert_eq!(fs::read(&stays)?, b"keep me");
    assert!(target.exists(), "the destination folder itself survives");
    assert_eq!(
        fs::read_to_string(bookmarks)?,
        format!("{}{}", pin(&shared), pin(&stays))
    );
    Ok(())
}

#[test]
fn merge_undo_keeps_the_incoming_copy_when_no_staged_original_exists() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let target = root.path().join("target");
    fs::create_dir_all(&target)?;
    let shared = target.join("shared.txt");
    fs::write(&shared, b"incoming wins")?;

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(91),
        Vec::new(),
        vec![Location::local(&shared)],
        Rc::new(move |event| emitted.borrow_mut().push(event)),
        gio::Cancellable::new(),
        staged_lookup(HashMap::new()),
    ));

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::CompletedWithErrors { .. })
    ));
    assert_eq!(
        fs::read(&shared)?,
        b"incoming wins",
        "the lookup runs before the incoming copy is deleted"
    );
    Ok(())
}

#[test]
fn merge_undo_tolerates_created_paths_that_no_longer_exist() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let target = root.path().join("target");
    fs::create_dir_all(&target)?;
    let gone = target.join("already-removed.txt");

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(92),
        vec![Location::local(&gone)],
        Vec::new(),
        Rc::new(move |event| emitted.borrow_mut().push(event)),
        gio::Cancellable::new(),
        staged_lookup(HashMap::new()),
    ));

    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Restored { .. })
    ));
    Ok(())
}

#[test]
fn a_cancelled_merge_undo_reports_everything_not_attempted() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let target = root.path().join("target");
    fs::create_dir_all(&target)?;
    let shared = target.join("shared.txt");
    let incoming = target.join("incoming.txt");
    fs::write(&shared, b"incoming wins")?;
    fs::write(&incoming, b"new")?;
    let cancellable = gio::Cancellable::new();
    cancellable.cancel();

    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(93),
        vec![Location::local(&incoming)],
        vec![Location::local(&shared)],
        Rc::new(move |event| emitted.borrow_mut().push(event)),
        cancellable,
        staged_lookup(HashMap::new()),
    ));

    let events = events.borrow();
    let Some(OperationEvent::Cancelled { result, .. }) = events.last() else {
        panic!("expected cancellation, got {events:?}");
    };
    assert!(result.completed.is_empty());
    assert_eq!(
        result.not_attempted,
        vec![Location::local(&shared), Location::local(&incoming)]
    );
    assert_eq!(fs::read(&shared)?, b"incoming wins");
    assert_eq!(fs::read(&incoming)?, b"new");
    Ok(())
}

fn without_trash(quirks: Quirks) -> Quirks {
    Quirks {
        trash: Trash::Unsupported,
        ..quirks
    }
}

fn names(dir: &Path) -> Result<Vec<String>, io::Error> {
    let mut names = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

fn aside_folder(target: &Path) -> Result<Option<String>, io::Error> {
    Ok(names(target)?
        .into_iter()
        .find(|name| name.starts_with(".strata-merge-")))
}

fn write_files(root: &Path, files: &[(&str, &str)]) -> Result<(), io::Error> {
    for (path, contents) in files {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    Ok(())
}

fn merge_through_trash(
    source: &Path,
    target: &Path,
    move_source: bool,
    copy_into_target: StageCopy,
) -> (Result<(), glib::Error>, Vec<MergePlan>) {
    let (plans, on_merged) = recorded_plans();
    let result = glib::MainContext::default().block_on(merge_local_with(
        gio::File::for_path(source),
        gio::File::for_path(target),
        move_source,
        gio::Cancellable::new(),
        None,
        MergeHooks {
            fat_family: false,
            copy_into_target,
            stage_overwrite: Rc::new(trash_stage_overwrite),
            on_merged: &on_merged,
        },
    ));
    (result, plans.take())
}

#[test]
fn merge_without_trash_sets_originals_aside_and_removes_them_after_success()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (trash, quirks, move_source) in [
        (Trash::Unsupported, Quirks::default(), false),
        (Trash::Unsupported, Quirks::without_rename_flags(), true),
        (Trash::FalseWithoutError, Quirks::default(), false),
    ] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        write_files(
            &source,
            &[
                ("shared.txt", "new"),
                ("nested/deep/shared.txt", "new deep"),
                ("incoming.txt", "incoming"),
            ],
        )?;
        write_files(
            &target,
            &[
                ("shared.txt", "old"),
                ("nested/deep/shared.txt", "old deep"),
                ("nested/stays.txt", "keep me"),
            ],
        )?;
        let quirks = FsQuirks::install(root.path(), Quirks { trash, ..quirks });

        let (result, plans) = merge_through_trash(&source, &target, move_source, real_copy());

        assert!(result.is_ok(), "{trash:?}: {result:?}");
        assert!(used_aside(&quirks));
        assert_eq!(fs::read(target.join("shared.txt"))?, b"new");
        assert_eq!(fs::read(target.join("nested/deep/shared.txt"))?, b"new deep");
        assert_eq!(fs::read(target.join("nested/stays.txt"))?, b"keep me");
        assert_eq!(names(&target)?, ["incoming.txt", "nested", "shared.txt"]);
        assert_eq!(source.exists(), !move_source);
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].created,
            vec![Location::local(target.join("incoming.txt"))]
        );
        assert!(
            plans[0].overwritten.is_empty(),
            "originals deleted without Trash cannot be restored by undo"
        );
    }
    Ok(())
}

fn used_aside(quirks: &FsQuirks) -> bool {
    quirks.calls().iter().any(|call| {
        call.names
            .iter()
            .any(|name| name.to_string_lossy().starts_with(".strata-merge-"))
    })
}

#[test]
fn a_failed_merge_without_trash_puts_back_what_it_can_and_names_the_rest()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (failure, reason) in [
        (gio::IOErrorEnum::Failed, "injected copy failure"),
        (gio::IOErrorEnum::Cancelled, "already exists"),
    ] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        write_files(&source, &[("a.txt", "new a"), ("b.txt", "new b")])?;
        write_files(
            &target,
            &[("a.txt", "old a"), ("b.txt", "old b"), ("stays.txt", "keep me")],
        )?;
        let _quirks = FsQuirks::install(root.path(), without_trash(Quirks::default()));
        let partial: StageCopy = Rc::new(move |_, target, _, _| {
            Box::pin(async move {
                let path = target.path().ok_or_else(|| io_error("missing target path"))?;
                fs::write(path.join("a.txt"), b"partial").map_err(io_error)?;
                Err(glib::Error::new(failure, "injected copy failure"))
            })
        });

        let (result, plans) = merge_through_trash(&source, &target, false, partial);

        let error = result.expect_err("the copy failed");
        let folder = aside_folder(&target)?.ok_or("the displaced original was not kept")?;
        assert!(error.message().contains(&folder), "{error}");
        assert!(error.message().contains(reason), "{error}");
        assert_eq!(fs::read(target.join(&folder).join("a.txt"))?, b"old a");
        assert_eq!(names(&target.join(&folder))?, ["a.txt"]);
        assert_eq!(fs::read(target.join("a.txt"))?, b"partial");
        assert_eq!(fs::read(target.join("b.txt"))?, b"old b");
        assert_eq!(fs::read(target.join("stays.txt"))?, b"keep me");
        assert!(plans.iter().all(|plan| plan.overwritten.is_empty()));
    }
    Ok(())
}

#[test]
fn a_failed_aside_puts_every_original_back_before_merging() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let target_files = [
        ("a.txt", "old a"),
        ("b.txt", "old b"),
        ("mount/c.txt", "old c"),
    ];
    for case in ["no safe placement", "second aside fails", "nested mount"] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        write_files(&source, &[("a.txt", "new a"), ("b.txt", "new b"), ("mount/c.txt", "new c")])?;
        write_files(&target, &target_files)?;
        let quirks = match case {
            "no safe placement" => Quirks::without_placement(),
            "second aside fails" => Quirks {
                fail: vec![(Op::Rename, "*", 1, Errno::IO)],
                ..Quirks::default()
            },
            _ => Quirks::default(),
        };
        let quirks = FsQuirks::install(root.path(), without_trash(quirks));
        if case == "second aside fails" {
            learn_mount_capability(quirks.key(), |capabilities| {
                capabilities.no_replace = Some(true);
            });
        }
        let _nested = (case == "nested mount").then(|| {
            FsQuirks::install(&target.join("mount"), without_trash(Quirks::default()))
        });
        let copied = Rc::new(Cell::new(false));
        let copy_observed = copied.clone();

        let (result, plans) = merge_through_trash(
            &source,
            &target,
            false,
            Rc::new(move |_, _, _, _| {
                copy_observed.set(true);
                Box::pin(async { Ok(()) })
            }),
        );

        let error = result.expect_err(case);
        assert!(error.message().contains("out of the way"), "{case}: {error}");
        assert!(!copied.get(), "{case}");
        assert_eq!(used_aside(&quirks), case != "no safe placement", "{case}");
        for (path, contents) in target_files {
            assert_eq!(fs::read(target.join(path))?, contents.as_bytes(), "{case}");
        }
        assert_eq!(names(&target)?, ["a.txt", "b.txt", "mount"], "{case}");
        assert!(plans.iter().all(|plan| plan.overwritten.is_empty()));
    }
    Ok(())
}

#[test]
fn merge_originals_are_deleted_only_after_the_removable_flush() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    write_files(&source, &[("shared.txt", "new")])?;
    write_files(&target, &[("shared.txt", "old")])?;
    let _guard = RemovableFlushGuard::install(root.path(), None);
    let aside_during_flush = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = aside_during_flush.clone();
    let watched = target.clone();
    set_sync_observer(Some(Arc::new(move |_| {
        let kept = aside_folder(&watched).is_ok_and(|folder| folder.is_some());
        record
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(kept);
    })));
    let _quirks = FsQuirks::install(root.path(), without_trash(Quirks::default()));

    let (result, _) = merge_through_trash(&source, &target, true, real_copy());

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        *aside_during_flush
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
        [true],
        "one flush before the originals are deleted, shared with the move"
    );
    assert_eq!(fs::read(target.join("shared.txt"))?, b"new");
    assert_eq!(names(&target)?, ["shared.txt"]);
    assert!(!source.exists());
    Ok(())
}
