// SPDX-License-Identifier: MIT

use std::{os::unix::fs::MetadataExt, sync::atomic::Ordering};

use rustix::io::Errno;

use super::fs_quirks::{Op, Racer, Trash};
use super::*;
use crate::{
    adapters::volume::learn_mount_capability,
    services::{KeptCause, KeptItem, PlacedSnapshot, ReplacedOriginal},
};

fn real_copy() -> CopyStep {
    super::super::merge_copy(None, false)
}

fn gio_merge_copy(fat_family: bool) -> CopyStep {
    Rc::new(
        move |source, target, copy: super::super::MergeCopy, cancellable| {
            Box::pin(async move {
                super::super::copy_recursively_with_progress(
                    source,
                    target,
                    super::super::CopyInto::Existing,
                    cancellable,
                    super::super::CopyRecord::Placing(copy.placing),
                    None,
                    fat_family,
                )
                .await
                .map(|()| MergeCopied::default())
            })
        },
    )
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
fn fat_merge_overwrites_the_names_it_plans_and_keeps_the_names_that_need_no_fix()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (row, copy) in [
        ("engine", super::super::merge_copy(None, true)),
        ("GIO", gio_merge_copy(true)),
    ] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir_all(source.join("nested?"))?;
        fs::create_dir_all(target.join("nested_"))?;
        for (name, contents) in [
            ("a?.txt", "question"),
            ("a:.txt", "colon"),
            ("b:.txt", "b colon"),
            ("b_.txt", "b kept"),
            ("C.txt", "upper c"),
            ("c.txt", "lower c"),
        ] {
            fs::write(source.join("nested?").join(name), contents)?;
        }
        // `b_ (1).txt` is what an earlier merge of `b:.txt` saved.
        for (name, contents) in [
            ("a_.txt", "original a"),
            ("b_.txt", "original b"),
            ("b_ (1).txt", "earlier b colon"),
        ] {
            fs::write(target.join("nested_").join(name), contents)?;
        }
        let _sources = FsQuirks::install(&source, Quirks::default());
        let _target = FsQuirks::install(&target, Quirks::vfat());
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
                copy_into_target: copy,
                stage_overwrite: stage,
                on_merged: &on_merged,
            },
        ))?;
        let nested = target.join("nested_");
        for (name, contents) in [
            ("a_.txt", "colon"),
            ("a_ (1).txt", "question"),
            ("b_.txt", "b kept"),
            ("b_ (1).txt", "b colon"),
            ("C.txt", "upper c"),
            ("c (1).txt", "lower c"),
        ] {
            assert_eq!(
                fs::read(nested.join(name))?,
                contents.as_bytes(),
                "{row}: {name}"
            );
        }
        assert_eq!(names(&nested)?.len(), 6, "{row}");
        let mut staged_contents = (0..staged.borrow().len())
            .map(|index| fs::read_to_string(staging.join(format!("staged-{index}"))))
            .collect::<io::Result<Vec<_>>>()?;
        staged_contents.sort();
        assert_eq!(
            staged_contents,
            ["earlier b colon", "original a", "original b"],
            "{row}"
        );
        let plans = plans.borrow();
        assert_eq!(plans.len(), 2, "{row}");
        assert_eq!(
            plans[0].created.iter().collect::<HashSet<_>>(),
            ["a_ (1).txt", "C.txt", "c (1).txt"]
                .map(|name| Location::local(nested.join(name)))
                .iter()
                .collect(),
            "{row}"
        );
        assert_eq!(plans[0].overwritten, *staged.borrow(), "{row}");
        assert_eq!(
            plans[1].placed.keys().collect::<HashSet<_>>(),
            plans[0].overwritten.iter().collect(),
            "{row}"
        );
    }
    Ok(())
}

#[test]
fn a_merge_off_fat_keeps_the_names_fat_would_fix() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (row, copy) in [("engine", real_copy()), ("GIO", gio_merge_copy(false))] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir_all(source.join("nested"))?;
        fs::create_dir_all(target.join("nested"))?;
        for (name, contents) in [("a:b", "colon"), ("C.txt", "upper"), ("c.txt", "lower")] {
            fs::write(source.join("nested").join(name), contents)?;
        }
        fs::write(target.join("nested/a_b"), "original")?;
        let (stage, staged) = rename_stage(root.path().join("staging"));
        let (plans, on_merged) = recorded_plans();

        glib::MainContext::default().block_on(merge_local_with(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            false,
            gio::Cancellable::new(),
            None,
            MergeHooks {
                fat_family: false,
                copy_into_target: copy,
                stage_overwrite: stage,
                on_merged: &on_merged,
            },
        ))?;

        let nested = target.join("nested");
        for (name, contents) in [
            ("a:b", "colon"),
            ("a_b", "original"),
            ("C.txt", "upper"),
            ("c.txt", "lower"),
        ] {
            assert_eq!(
                fs::read(nested.join(name))?,
                contents.as_bytes(),
                "{row}: {name}"
            );
        }
        assert!(staged.borrow().is_empty(), "{row}");
        assert_eq!(
            plans.borrow()[0].created.iter().collect::<HashSet<_>>(),
            ["a:b", "C.txt", "c.txt"]
                .map(|name| Location::local(nested.join(name)))
                .iter()
                .collect(),
            "{row}"
        );
    }
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
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].created,
        vec![Location::local(target.join("incoming.txt"))]
    );
    assert_eq!(
        plans[0].overwritten,
        vec![Location::local(target.join("shared.txt"))]
    );
    assert_eq!(
        placed_paths(&plans[1].placed),
        [target.join("shared.txt")],
        "undo trashes what was created, so only what the copy put over an original is recorded"
    );
    Ok(())
}

fn placed_paths(placed: &HashMap<Location, PlacedSnapshot>) -> Vec<PathBuf> {
    let mut paths = (placed.keys())
        .filter_map(|location| location.native_path().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    paths.sort();
    paths
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
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].created,
        vec![Location::local(target.join("nested/incoming.txt"))]
    );
    assert_eq!(
        plans[0].overwritten,
        vec![Location::local(target.join("nested/shared.txt"))]
    );
    assert_eq!(
        placed_paths(&plans[1].placed),
        [target.join("nested/shared.txt")],
        "undo trashes what was created, so only what the copy put over an original is recorded"
    );
    Ok(())
}

#[test]
fn a_merge_from_a_location_without_paths_records_what_it_copied_over_an_original()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    gio::resources_register_include!("strata.gresource")?;
    let source = gio::File::for_uri("resource:///io/github/lgse/Strata/brand");
    assert!(!source.is_native());
    let root = tempfile::tempdir()?;
    let target = root.path().join("brand");
    let overwritten = target.join("strata-logo-white.svg");
    write_files(
        &target,
        &[("strata-logo-white.svg", "old"), ("stays.txt", "keep me")],
    )?;
    let (stage, _) = rename_stage(root.path().join("staging"));
    let (plans, on_merged) = recorded_plans();

    let result = glib::MainContext::default().block_on(merge_local_with(
        source,
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
    assert_ne!(fs::read(&overwritten)?, b"old");
    let (_, staged, placed) = undo_parts(plans.take());
    assert_eq!(staged, [Location::local(&overwritten)]);
    assert_eq!(
        placed.keys().collect::<Vec<_>>(),
        [&Location::local(&overwritten)]
    );
    Ok(())
}

#[test]
fn a_merge_from_a_location_without_paths_records_the_folders_it_creates_once_filled()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    gio::resources_register_include!("strata.gresource")?;
    let source = gio::File::for_uri("resource:///io/github/lgse/Strata/icons/scalable");
    let root = tempfile::tempdir()?;
    let target = root.path().join("scalable");
    write_files(&target, &[("stays.txt", "keep me")])?;
    let _scope = FsQuirks::install(root.path(), without_trash(Quirks::default()));
    let (plans, on_merged) = recorded_plans();

    let result = glib::MainContext::default().block_on(merge_local(
        source,
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        &on_merged,
    ));

    assert!(result.is_ok(), "{result:?}");
    let [actions, apps] = ["actions", "apps"].map(|name| target.join(name));
    let parts = undo_parts(plans.take());
    assert_eq!(placed_paths(&parts.2), [actions.clone(), apps.clone()]);
    let edited = fs::read_dir(&apps)?.next().ok_or("an app icon")??.path();
    fs::write(&edited, b"edited after the merge")?;

    let events = run_undo(parts, staged_lookup(HashMap::new()));

    assert!(!actions.exists(), "{events:?}");
    assert_eq!(fs::read(&edited)?, b"edited after the merge");
    assert_eq!(names(&target)?, ["apps", "stays.txt"]);
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
    let _scope = FsQuirks::install(root.path(), without_trash(Quirks::default()));
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
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].created,
        vec![Location::local(target.join("sub"))],
        "undo removes the topmost created directory, not each nested child"
    );
    assert!(plans[0].overwritten.is_empty());
    assert_eq!(
        placed_paths(&plans[1].placed),
        [target.join("sub")],
        "without Trash, it is recorded whole"
    );
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
fn merge_places_a_read_only_incoming_folder() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("incoming"))?;
    fs::create_dir_all(&target)?;
    fs::write(source.join("incoming/file.txt"), b"new")?;
    fs::write(target.join("stays.txt"), b"keep me")?;
    let incoming = source.join("incoming");
    fs::set_permissions(&incoming, fs::Permissions::from_mode(0o555))?;

    let result = glib::MainContext::default().block_on(merge_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        &no_plan,
    ));

    let placed = fs::metadata(target.join("incoming")).map(|metadata| metadata.mode() & 0o7777);
    for folder in [&incoming, &target.join("incoming")] {
        if folder.is_dir() {
            fs::set_permissions(folder, fs::Permissions::from_mode(0o755))?;
        }
    }
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(placed?, 0o555);
    assert_eq!(fs::read(target.join("incoming/file.txt"))?, b"new");
    assert_eq!(fs::read(target.join("stays.txt"))?, b"keep me");
    Ok(())
}

fn visible_names(folder: &Path) -> io::Result<Vec<String>> {
    Ok(names(folder)?
        .into_iter()
        .filter(|name| !name.starts_with(".strata-"))
        .collect())
}

#[test]
fn a_merge_keeps_hard_links_only_within_one_merge_stage() -> Result<(), Box<dyn Error>> {
    use super::fs_quirks::Racer;
    use std::os::unix::fs::MetadataExt;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("new"))?;
    fs::create_dir_all(source.join("sub"))?;
    fs::create_dir_all(target.join("sub"))?;
    fs::write(source.join("a"), b"linked")?;
    for name in ["a2", "new/c", "sub/b"] {
        fs::hard_link(source.join("a"), source.join(name))?;
    }
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (live, seen_at_link) = (target.clone(), seen.clone());
    let racer: Racer = Box::new(move || {
        let names = visible_names(&live).expect("list the merged folder");
        seen_at_link.lock().expect("record").extend(names);
    });
    let scope = FsQuirks::install(
        &target,
        Quirks {
            before: vec![(Op::Link, "*", 0, racer)],
            ..Quirks::default()
        },
    );
    learn_mount_capability(scope.key(), |capabilities| {
        capabilities.hard_links = Some(true);
    });

    let result = glib::MainContext::default().block_on(merge_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        &no_plan,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        *seen.lock().map_err(|error| error.to_string())?,
        ["sub"],
        "names are linked before their stage is placed"
    );
    let [a, a2, c, b] = ["a", "a2", "new/c", "sub/b"].map(|name| {
        fs::metadata(target.join(name)).map(|metadata| (metadata.ino(), metadata.nlink()))
    });
    let (a, b) = (a?, b?);
    assert_eq!((a2?, c?), (a, a));
    assert_eq!(a.1, 3);
    assert_ne!(b.0, a.0);
    assert_eq!(b.1, 1);
    assert_eq!(fs::read(target.join("sub/b"))?, b"linked");
    Ok(())
}

#[test]
fn a_merged_copy_replaced_before_linking_is_not_linked() -> Result<(), Box<dyn Error>> {
    use super::fs_quirks::Racer;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir(&source)?;
    fs::create_dir(&target)?;
    fs::write(source.join("a"), b"linked")?;
    fs::hard_link(source.join("a"), source.join("a2"))?;
    let live = target.clone();
    let racer: Racer = Box::new(move || {
        let stage = names(&live)
            .expect("list the merged folder")
            .into_iter()
            .find(|name| name.starts_with(".strata-copy-"))
            .map(|name| live.join(name));
        for copied in stage
            .iter()
            .flat_map(|stage| [stage.join("a"), stage.join("a2")])
        {
            if copied.exists() {
                let other = live.join("other");
                fs::write(&other, b"other").expect("write the other item");
                fs::rename(&other, &copied).expect("replace the copy");
            }
        }
    });
    let scope = FsQuirks::install(
        &target,
        Quirks {
            before: vec![(Op::Link, "*", 0, racer)],
            ..Quirks::default()
        },
    );
    learn_mount_capability(scope.key(), |capabilities| {
        capabilities.hard_links = Some(true);
    });

    let result = glib::MainContext::default().block_on(merge_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        &no_plan,
    ));

    assert!(result.is_err(), "{result:?}");
    assert_eq!(names(&target)?, Vec::<String>::new());
    Ok(())
}

#[test]
fn merge_never_overwrites_an_entry_created_during_the_merge() -> Result<(), Box<dyn Error>> {
    use super::fs_quirks::Racer;
    use crate::services::RenameCause;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    type Renamed = &'static [(&'static str, &'static str, RenameCause)];
    let rows: [(&str, &str, Preset, Renamed); 2] = [
        (
            "notes.txt",
            "notes (1).txt",
            Quirks::default,
            &[("notes.txt", "notes (1).txt", RenameCause::Taken)],
        ),
        (
            "photos",
            "photos (1)",
            Quirks::vfat,
            &[
                ("photos", "photos (1)", RenameCause::Taken),
                (
                    "photos (1)/makefile",
                    "photos (1)/makefile (1)",
                    RenameCause::Alias,
                ),
            ],
        ),
    ];
    for (taken, saved_as, preset, reported) in rows {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir_all(source.join("photos"))?;
        fs::create_dir(&target)?;
        fs::write(source.join("notes.txt"), b"incoming")?;
        for name in ["Makefile", "makefile"] {
            fs::write(source.join("photos").join(name), name)?;
        }
        let live = target.clone();
        let racer: Racer = Box::new(move || {
            fs::write(live.join(taken), b"racer").expect("take the name");
        });
        let _scope = FsQuirks::install(
            &target,
            Quirks {
                before: vec![(Op::Rename, taken, 0, racer)],
                ..preset()
            },
        );

        let (merged, report) = merge_reporting(&source, &target, None);

        assert!(merged.is_ok(), "{taken}: {merged:?}");
        assert_eq!(fs::read(target.join(taken))?, b"racer", "{taken}");
        let mut expected = vec!["notes.txt", "photos", saved_as];
        expected.sort_unstable();
        assert_eq!(names(&target)?, expected, "{taken}");
        let ours = target.join(saved_as);
        if taken == "photos" {
            assert_eq!(fs::read(ours.join("Makefile"))?, b"Makefile");
            assert_eq!(fs::read(ours.join("makefile (1)"))?, b"makefile");
        } else {
            assert_eq!(fs::read(ours)?, b"incoming");
        }
        let reported = reported
            .iter()
            .map(|(requested, saved_as, cause)| {
                (requested.to_string(), saved_as.to_string(), *cause)
            })
            .collect::<Vec<_>>();
        assert_eq!(renamed_beneath(&target, &report), reported, "{taken}");
    }
    Ok(())
}

#[test]
fn an_item_put_at_a_placed_name_right_after_placing_stays_there() -> Result<(), Box<dyn Error>> {
    use super::fs_quirks::Racer;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // A program that saves by renaming over the name does so as soon as the
    // child is placed: by a rename, or by a link and the stage's unlink.
    let rows: [(&str, Preset, Op); 2] = [
        ("rename", Quirks::default, Op::Rename),
        ("link", Quirks::without_rename_flags, Op::Unlink),
    ];
    for (row, preset, placed) in rows {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source)?;
        fs::create_dir(&target)?;
        fs::write(source.join("a.txt"), b"incoming")?;
        let saved = root.path().join("saved.txt");
        fs::write(&saved, b"saved by another program")?;
        let over = target.join("a.txt");
        let racer: Racer = Box::new(move || {
            fs::rename(&saved, &over).expect("rename over the placed name");
        });
        let _scope = FsQuirks::install(
            &target,
            Quirks {
                after: vec![(placed, "a.txt", 0, racer)],
                ..preset()
            },
        );

        let (merged, _) = merge_reporting(&source, &target, None);

        assert!(merged.is_ok(), "{row}: {merged:?}");
        assert_eq!(
            fs::read(target.join("a.txt"))?,
            b"saved by another program",
            "{row}"
        );
        assert_eq!(names(&target)?, ["a.txt"], "{row}");
    }
    Ok(())
}

fn renamed_beneath(
    target: &Path,
    report: &crate::services::TransferReport,
) -> Vec<(String, String, crate::services::RenameCause)> {
    let relative = |location: &Location| {
        location
            .native_path()
            .and_then(|path| path.strip_prefix(target).ok())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let mut renamed = report
        .renamed
        .iter()
        .map(|item| {
            (
                relative(&item.requested),
                relative(&item.saved_as),
                item.cause,
            )
        })
        .collect::<Vec<_>>();
    renamed.sort_by(|left, right| left.0.cmp(&right.0));
    renamed
}

#[test]
fn a_stopped_merge_reports_only_what_it_placed() -> Result<(), Box<dyn Error>> {
    use super::fs_quirks::Racer;
    use crate::services::RenameCause;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("z"))?;
    fs::create_dir(&target)?;
    fs::write(source.join("a.txt"), b"incoming")?;
    for name in ["Makefile", "makefile"] {
        fs::write(source.join("z").join(name), name)?;
    }
    let live = target.clone();
    let racer: Racer = Box::new(move || {
        fs::write(live.join("a.txt"), b"racer").expect("take the name");
    });
    let _scope = FsQuirks::install(
        &target,
        Quirks {
            before: vec![(Op::Rename, "a.txt", 0, racer)],
            fail: vec![(Op::Rename, "z", 0, Errno::IO)],
            ..Quirks::vfat()
        },
    );

    let (merged, report) = merge_reporting(&source, &target, None);

    assert!(merged.is_err(), "{merged:?}");
    assert_eq!(names(&target)?, ["a (1).txt", "a.txt"]);
    assert_eq!(fs::read(target.join("a (1).txt"))?, b"incoming");
    assert_eq!(
        renamed_beneath(&target, &report),
        [("a.txt".into(), "a (1).txt".into(), RenameCause::Taken)]
    );
    Ok(())
}

#[test]
fn an_unconfirmed_placement_keeps_its_stage() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir(&source)?;
    fs::create_dir(&target)?;
    fs::write(source.join("leaf"), b"incoming")?;
    // The placing rename's reply is lost, and the leaf is found at neither
    // name: it went back into the stage under another one.
    let live = target.clone();
    let racer: super::fs_quirks::Racer = Box::new(move || {
        let stage = names(&live)
            .expect("live folder")
            .into_iter()
            .find(|name| name.starts_with(".strata-copy-"))
            .expect("stage");
        fs::rename(live.join("leaf"), live.join(stage).join("moved")).expect("move the leaf");
    });
    let _scope = FsQuirks::install(
        &target,
        Quirks {
            lost_reply: vec![(Op::Rename, "leaf", Errno::IO)],
            after: vec![(Op::Rename, "leaf", 0, racer)],
            ..Quirks::default()
        },
    );

    let result = glib::MainContext::default().block_on(merge_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        &no_plan,
    ));

    assert!(result.is_err(), "{result:?}");
    let names = names(&target)?;
    let [stage] = names.as_slice() else {
        panic!("only the stage that holds the leaf stays: {names:?}");
    };
    assert_eq!(fs::read(target.join(stage).join("moved"))?, b"incoming");
    Ok(())
}

#[test]
fn a_merge_copies_folders_in_place_where_the_mount_cannot_place_them() -> Result<(), Box<dyn Error>>
{
    use crate::services::RenameCause;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("new"))?;
    fs::create_dir(&target)?;
    for name in ["Makefile", "makefile"] {
        fs::write(source.join("new").join(name), name)?;
    }
    fs::write(target.join("new"), b"taken")?;
    let scope = FsQuirks::install(
        &target,
        Quirks {
            mount_type: Some(("ext4", "rw")),
            case_insensitive: true,
            ..Quirks::without_folder_placement()
        },
    );
    learn_mount_capability(scope.key(), |capabilities| {
        capabilities.no_replace = Some(false);
        capabilities.hard_links = Some(true);
        capabilities.rename_over_empty_directory = Some(false);
    });

    // Without claims, one worker makes the folder's files last first, so
    // `Makefile` is the alias.
    let (merged, report) = merge_reporting(&source, &target, Some(1));

    assert!(merged.is_ok(), "{merged:?}");
    assert_eq!(names(&target)?, ["new", "new (1)"]);
    assert_eq!(fs::read(target.join("new"))?, b"taken");
    assert_eq!(
        names(&target.join("new (1)"))?,
        ["Makefile (1)", "makefile"]
    );
    assert_eq!(fs::read(target.join("new (1)/Makefile (1)"))?, b"Makefile");
    assert!(!scope.used_name(".strata-*"), "{:?}", scope.calls());
    assert_eq!(
        renamed_beneath(&target, &report),
        [
            ("new".into(), "new (1)".into(), RenameCause::Taken),
            (
                "new (1)/Makefile".into(),
                "new (1)/Makefile (1)".into(),
                RenameCause::Alias
            ),
        ]
    );
    Ok(())
}

#[test]
fn a_merged_item_is_invisible_until_complete() -> Result<(), Box<dyn Error>> {
    use super::fs_quirks::Racer;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // Placed by rename, by the rungs that cannot replace, and copied out of
    // the stage where nothing can place it.
    let presets: [Preset; 3] = [Quirks::default, Quirks::vboxsf, Quirks::without_placement];
    for (row, preset) in presets.into_iter().enumerate() {
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let destination = root.path().join("destination");
        let source = sources.join("folder");
        let target = destination.join("folder");
        fs::create_dir_all(source.join("new"))?;
        fs::create_dir_all(&target)?;
        fs::write(source.join("incoming.bin"), vec![0x5a; 256 * 1024])?;
        fs::write(source.join("new/inner.bin"), vec![0xa5; 256 * 1024])?;
        fs::write(target.join("stays.txt"), b"keep me")?;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (live, seen_at_write) = (target.clone(), seen.clone());
        let racer: Racer = Box::new(move || {
            let names = visible_names(&live).expect("list the merged folder");
            seen_at_write.lock().expect("record").extend(names);
        });
        // Separate mounts, so the data is written rather than cloned.
        let _sources = FsQuirks::install(&sources, Quirks::default());
        let _destination = FsQuirks::install(
            &destination,
            Quirks {
                before: vec![(Op::Write, "*", 0, racer)],
                ..preset()
            },
        );

        let result = glib::MainContext::default().block_on(merge_local(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            false,
            gio::Cancellable::new(),
            &no_plan,
        ));

        assert!(result.is_ok(), "row {row}: {result:?}");
        assert_eq!(
            *seen.lock().map_err(|error| error.to_string())?,
            ["stays.txt"],
            "row {row}"
        );
        assert_eq!(
            names(&target)?,
            ["incoming.bin", "new", "stays.txt"],
            "row {row}"
        );
        assert_eq!(
            fs::read(target.join("incoming.bin"))?,
            vec![0x5a; 256 * 1024]
        );
        assert_eq!(
            fs::read(target.join("new/inner.bin"))?,
            vec![0xa5; 256 * 1024]
        );
        assert_eq!(fs::read(target.join("stays.txt"))?, b"keep me");
    }
    Ok(())
}

#[test]
fn merge_does_not_follow_a_target_folder_swapped_for_a_link() -> Result<(), Box<dyn Error>> {
    use super::fs_quirks::Racer;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    let outside = root.path().join("outside");
    fs::create_dir_all(source.join("sub"))?;
    fs::create_dir_all(target.join("sub"))?;
    fs::create_dir(&outside)?;
    fs::write(source.join("sub/new.txt"), b"new")?;
    fs::write(target.join("sub/old.txt"), b"old")?;
    let (live, elsewhere) = (target.clone(), outside.clone());
    let racer: Racer = Box::new(move || {
        fs::rename(live.join("sub"), live.join("moved")).expect("move the folder away");
        std::os::unix::fs::symlink(&elsewhere, live.join("sub")).expect("link elsewhere");
    });
    let _scope = FsQuirks::install(
        &target,
        Quirks {
            before: vec![(Op::OpenDir, "sub", 0, racer)],
            ..Quirks::default()
        },
    );

    let result = glib::MainContext::default().block_on(merge_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        &no_plan,
    ));

    assert!(result.is_err(), "{result:?}");
    assert_eq!(names(&outside)?, Vec::<String>::new());
    assert_eq!(names(&target.join("moved"))?, ["old.txt"]);
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
            copy_into_target: Rc::new(|_, target, _, _| {
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
        let placed = placed_now(&[&target]);
        glib::MainContext::default().block_on(run_merge_undo(
            OperationRequestId(91),
            (Vec::new(), vec![location], placed),
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
    let placed = placed_now(&[&shared]);
    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(90),
        (
            vec![Location::local(&incoming)],
            vec![Location::local(&shared)],
            placed,
        ),
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
fn undoing_a_merge_with_numbered_aliases_removes_only_its_items() -> Result<(), Box<dyn Error>> {
    use crate::services::{RenameCause, RenamedItem};
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let destination = root.path().join("destination");
    let folder = destination.join("folder");
    fs::create_dir_all(sources.join("folder"))?;
    fs::create_dir_all(&folder)?;
    for name in ["Makefile", "makefile"] {
        fs::write(sources.join("folder").join(name), name)?;
    }
    fs::write(folder.join("keep.txt"), b"keep me")?;
    let _scope = FsQuirks::install(&destination, without_trash(Quirks::vfat()));

    let events = run_paste(PasteRequest {
        id: OperationRequestId(94),
        destination: Location::local(&destination),
        items: vec![PasteItem {
            source: Location::local(sources.join("folder")),
            conflict: TransferConflict::Merge,
        }],
        move_sources: false,
    });

    let mut created = events
        .iter()
        .filter_map(|event| match event {
            OperationEvent::Merged { created, .. } => Some(created.clone()),
            _ => None,
        })
        .flatten()
        .filter_map(|location| location.native_path().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    created.sort();
    assert_eq!(created, [folder.join("Makefile"), folder.join("makefile")]);
    let renamed = events
        .iter()
        .filter_map(|event| match event {
            OperationEvent::ItemsReported { report, .. } => Some(report.renamed.clone()),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    let saved_as = Location::local(folder.join("makefile (1)"));
    assert_eq!(
        renamed,
        [RenamedItem {
            requested: Location::local(folder.join("makefile")),
            saved_as: saved_as.clone(),
            cause: RenameCause::Alias,
        }]
    );
    let placed = events
        .iter()
        .filter_map(|event| match event {
            OperationEvent::Merged { placed, .. } => Some(placed.clone()),
            _ => None,
        })
        .flatten()
        .collect::<HashMap<_, _>>();
    assert!(
        placed.contains_key(&saved_as),
        "an alias is recorded as saved"
    );
    let undone = Rc::new(RefCell::new(Vec::new()));
    let emitted = undone.clone();
    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(95),
        (
            vec![Location::local(folder.join("Makefile")), saved_as],
            Vec::new(),
            placed,
        ),
        Rc::new(move |event| emitted.borrow_mut().push(event)),
        gio::Cancellable::new(),
        staged_lookup(HashMap::new()),
    ));

    assert!(
        matches!(
            undone.borrow().last(),
            Some(OperationEvent::Restored { .. })
        ),
        "{:?}",
        undone.borrow()
    );
    assert_eq!(names(&folder)?, ["keep.txt"]);
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
    let placed = placed_now(&[&shared]);
    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(91),
        (Vec::new(), vec![Location::local(&shared)], placed),
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
        (vec![Location::local(&gone)], Vec::new(), HashMap::new()),
        Rc::new(move |event| emitted.borrow_mut().push(event)),
        gio::Cancellable::new(),
        staged_lookup(HashMap::new()),
    ));

    // Reported as removed, so a view that still lists it drops it.
    assert!(matches!(
        events.borrow().last(),
        Some(OperationEvent::Restored { locations, .. }) if locations == &[Location::local(&gone)]
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
        (
            vec![Location::local(&incoming)],
            vec![Location::local(&shared)],
            HashMap::new(),
        ),
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

#[test]
fn a_merge_a_cancel_cuts_off_counts_as_failed_as_it_keeps_what_it_placed()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let destination = root.path().join("destination");
    fs::create_dir_all(sources.join("run"))?;
    fs::create_dir_all(destination.join("run"))?;
    fs::write(sources.join("run/big.bin"), vec![0x5a; 4 * 1024 * 1024])?;
    fs::write(destination.join("run/old.txt"), b"old")?;
    let cancel = CancelFromMain::default();
    let _sources = FsQuirks::install(&sources, Quirks::default());
    let _destination = FsQuirks::install(
        &destination,
        Quirks {
            before: vec![
                (Op::CopyRange, "*", 0, cancel.racer()),
                (Op::Write, "*", 0, cancel.racer()),
            ],
            reflink: super::fs_quirks::Support::Rejected(Errno::OPNOTSUPP),
            ..Quirks::default()
        },
    );
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let mut operation = Some(LocalOperationProvider.paste(
        PasteRequest {
            id: OperationRequestId(1615),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(sources.join("run")),
                conflict: TransferConflict::Merge,
            }],
            move_sources: false,
        },
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    ));

    let context = glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !events.borrow().iter().any(|event| {
        matches!(
            event,
            OperationEvent::Cancelled { .. }
                | OperationEvent::Pasted { .. }
                | OperationEvent::TransferFailed { .. }
        )
    }) {
        assert!(Instant::now() < deadline, "the merge never ended");
        cancel.serve(|| drop(operation.take()));
        context.iteration(false);
        thread::sleep(Duration::from_millis(1));
    }

    assert!(cancel.served());
    let events = events.borrow();
    let Some(OperationEvent::Cancelled { result, .. }) = events.last() else {
        panic!("expected a cancel, got {events:?}");
    };
    assert!(result.completed.is_empty());
    assert_eq!(result.failed, [Location::local(sources.join("run"))]);
    assert!(result.not_attempted.is_empty());
    assert_eq!(fs::read(destination.join("run/old.txt"))?, b"old");
    Ok(())
}

#[test]
fn a_merge_undo_cut_off_counts_an_item_as_not_attempted_only_when_it_left_it_whole()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // Without Trash, the first check is Trash's, the second the comparison's
    // and the third the delete's.
    for (row, check) in [("Trash", None), ("compare", Some(1)), ("delete", Some(2))] {
        let root = tempfile::tempdir()?;
        let created = ["first.txt", "second.txt"].map(|name| root.path().join(name));
        for item in &created {
            fs::write(item, b"merged")?;
        }
        let placed = placed_now(&[&created[0], &created[1]]);
        let cancellable = gio::Cancellable::new();
        let cancel = cancellable.clone();
        let cancel: super::fs_quirks::Racer = Box::new(move || cancel.cancel());
        let quirks = match check {
            None => Quirks {
                before: vec![(Op::Trash, "first.txt", 0, cancel)],
                ..Quirks::default()
            },
            Some(check) => without_trash(Quirks {
                after: vec![(Op::Stat, "first.txt", check, cancel)],
                ..Quirks::default()
            }),
        };
        let _scope = FsQuirks::install(root.path(), quirks);
        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();

        glib::MainContext::default().block_on(run_merge_undo(
            OperationRequestId(1615),
            (
                created.iter().map(Location::local).collect(),
                Vec::new(),
                placed,
            ),
            Rc::new(move |event| emitted.borrow_mut().push(event)),
            cancellable,
            staged_lookup(HashMap::new()),
        ));

        let events = events.borrow();
        let Some(OperationEvent::Cancelled { result, .. }) = events.last() else {
            panic!("{row}: expected a cancel, got {events:?}");
        };
        let [first, second] = &created;
        assert!(result.completed.is_empty(), "{row}");
        if row == "delete" {
            assert!(!first.exists(), "the cancel landed before the delete");
            assert_eq!(result.failed, [Location::local(first)]);
            assert_eq!(result.not_attempted, [Location::local(second)]);
        } else {
            assert!(result.failed.is_empty(), "{row}");
            assert_eq!(
                result.not_attempted,
                [Location::local(first), Location::local(second)],
                "{row}"
            );
            assert_eq!(fs::read(first)?, b"merged", "{row}");
        }
        assert_eq!(fs::read(second)?, b"merged", "{row}");
    }
    Ok(())
}

#[test]
fn a_kept_item_counts_as_failed_when_a_later_item_is_cut_off() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let [kept, cut] = ["kept.txt", "cut.txt"].map(|name| root.path().join(name));
    for item in [&kept, &cut] {
        fs::write(item, b"merged")?;
    }
    let placed = placed_now(&[&cut]);
    let cancellable = gio::Cancellable::new();
    let cancel = cancellable.clone();
    let cancel: Racer = Box::new(move || cancel.cancel());
    let _scope = FsQuirks::install(
        root.path(),
        without_trash(Quirks {
            after: vec![(Op::Stat, "cut.txt", 1, cancel)],
            ..Quirks::default()
        }),
    );
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();

    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(1617),
        (
            vec![Location::local(&kept), Location::local(&cut)],
            Vec::new(),
            placed,
        ),
        Rc::new(move |event| emitted.borrow_mut().push(event)),
        cancellable,
        staged_lookup(HashMap::new()),
    ));

    let events = events.borrow();
    let Some(OperationEvent::Cancelled { result, .. }) = events.last() else {
        panic!("expected a cancel, got {events:?}");
    };
    assert!(result.completed.is_empty(), "{result:?}");
    assert_eq!(result.failed, [Location::local(&kept)]);
    assert_eq!(result.not_attempted, [Location::local(&cut)]);
    assert_eq!(kept_items(&events), [Location::local(&kept)]);
    assert_eq!(fs::read(&kept)?, b"merged");
    assert_eq!(fs::read(&cut)?, b"merged");
    Ok(())
}

#[test]
fn a_merge_undo_never_counts_an_item_it_could_not_check_as_undone() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let [first, second] = ["first.txt", "second.txt"].map(|name| root.path().join(name));
    for item in [&first, &second] {
        fs::write(item, b"merged")?;
    }
    let unreadable = Location::uri("strata-test-no-backend:///odd.txt");
    let placed = placed_now(&[&first, &second]);
    let _scope = FsQuirks::install(root.path(), without_trash(Quirks::default()));
    let cancellable = gio::Cancellable::new();
    let cancel = cancellable.clone();
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let emit = Rc::new(move |event: OperationEvent| {
        if matches!(event, OperationEvent::DeleteProgress { completed: 2, .. }) {
            let cancel = cancel.clone();
            // Runs ahead of the reply to the next item's check, which is then in flight.
            glib::idle_add_local_full(glib::Priority::HIGH, move || {
                cancel.cancel();
                glib::ControlFlow::Break
            });
        }
        emitted.borrow_mut().push(event);
    });

    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(1615),
        (
            vec![
                Location::local(&first),
                unreadable.clone(),
                Location::local(&second),
            ],
            Vec::new(),
            placed,
        ),
        emit,
        cancellable,
        staged_lookup(HashMap::new()),
    ));

    let events = events.borrow();
    let Some(OperationEvent::Cancelled { result, .. }) = events.last() else {
        panic!("expected a cancel, got {events:?}");
    };
    assert_eq!(result.completed, [Location::local(&first)]);
    assert_eq!(result.failed, [unreadable]);
    assert_eq!(result.not_attempted, [Location::local(&second)]);
    assert!(!first.exists());
    assert_eq!(fs::read(&second)?, b"merged");
    Ok(())
}

#[test]
fn a_merge_undo_cancelled_while_it_restores_an_original_ends_as_a_cancel()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // Whether the merge's copy is still at the last path, which the undo
    // deletes before it restores the original, and whether the cut-off copy
    // of the original cannot be removed.
    for (incoming, stuck) in [(true, false), (false, false), (false, true)] {
        let root = tempfile::tempdir()?;
        let [first, second] = ["first.bin", "second.bin"].map(|name| root.path().join(name));
        let original = vec![0x5a; 4 * 1024 * 1024];
        fs::write(&first, b"incoming")?;
        if incoming {
            fs::write(&second, b"incoming")?;
        }
        let staged = HashMap::from([
            (
                first.clone(),
                staged_trash_fixture(root.path(), "first.bin", &first, b"first original")?,
            ),
            (
                second.clone(),
                staged_trash_fixture(root.path(), "second.bin", &second, &original)?,
            ),
        ]);
        let trash = root
            .path()
            .join(format!(".Trash-{}", rustix::process::getuid().as_raw()));
        let cancellable = gio::Cancellable::new();
        let racer = |cancel: gio::Cancellable| -> super::fs_quirks::Racer {
            Box::new(move || cancel.cancel())
        };
        let _scope = FsQuirks::install(
            root.path(),
            Quirks {
                before: vec![
                    (Op::CopyRange, "second.bin", 0, racer(cancellable.clone())),
                    (Op::Write, "second.bin", 0, racer(cancellable.clone())),
                ],
                fail: if stuck {
                    vec![(Op::Unlink, "second.bin", 0, Errno::IO)]
                } else {
                    Vec::new()
                },
                reflink: super::fs_quirks::Support::Rejected(Errno::OPNOTSUPP),
                ..Quirks::without_placement()
            },
        );
        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();

        let placed = placed_now(&[&first, &second]);
        glib::MainContext::default().block_on(run_merge_undo(
            OperationRequestId(1615),
            (
                Vec::new(),
                vec![Location::local(&first), Location::local(&second)],
                placed,
            ),
            Rc::new(move |event| emitted.borrow_mut().push(event)),
            cancellable,
            staged_lookup(staged),
        ));

        let events = events.borrow();
        let Some(OperationEvent::Cancelled { result, .. }) = events.last() else {
            panic!(
                "{incoming}, {stuck}: expected a cancel, got {:?}",
                events.last()
            );
        };
        let cut_off = vec![Location::local(&second)];
        let (failed, not_attempted) = if incoming || stuck {
            (cut_off, Vec::new())
        } else {
            (Vec::new(), cut_off)
        };
        assert_eq!(result.completed, [Location::local(&first)]);
        assert_eq!(result.failed, failed, "{incoming}, {stuck}");
        assert_eq!(result.not_attempted, not_attempted, "{incoming}, {stuck}");
        assert_eq!(fs::read(&first)?, b"first original");
        let left = fs::symlink_metadata(&second).is_ok();
        assert_eq!(left, stuck, "{incoming}, {stuck}");
        assert_eq!(fs::read(trash.join("files/second.bin"))?, original);
        assert!(trash.join("info/second.bin.trashinfo").exists());
    }
    Ok(())
}

#[test]
fn a_merge_undo_restores_an_original_only_over_a_path_it_checked() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unreadable = "strata-test-no-backend:///second.txt";
    // The second path's check fails, a cancel cuts it off, or the merge's
    // copy is already gone from it.
    for case in ["failed", "cancelled", "missing"] {
        let root = tempfile::tempdir()?;
        let [first, second] = ["first.txt", "second.txt"].map(|name| root.path().join(name));
        fs::write(&first, b"merged")?;
        if case == "cancelled" {
            fs::write(&second, b"merged")?;
        }
        let target = match case {
            "failed" => Location::uri(unreadable),
            _ => Location::local(&second),
        };
        let entries = RefCell::new(HashMap::from([
            (
                Location::local(&first),
                staged_trash_fixture(root.path(), "first.txt", &first, b"first")?,
            ),
            (
                target.clone(),
                staged_trash_fixture(root.path(), "second.txt", &second, b"second")?,
            ),
        ]));
        let lookup: StagedOriginalLookup = Rc::new(move |location, _| {
            let entry = entries.borrow_mut().remove(&location);
            Box::pin(async move { entry.ok_or_else(|| io_error("not staged")) })
        });
        let trash = root
            .path()
            .join(format!(".Trash-{}", rustix::process::getuid().as_raw()));
        let cancellable = gio::Cancellable::new();
        let cancel = cancellable.clone();
        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();
        let emit = Rc::new(move |event: OperationEvent| {
            if case == "cancelled"
                && matches!(event, OperationEvent::RestoreProgress { completed: 1, .. })
            {
                let cancel = cancel.clone();
                // Runs ahead of the reply to the second path's check, then in flight.
                glib::idle_add_local_full(glib::Priority::HIGH, move || {
                    cancel.cancel();
                    glib::ControlFlow::Break
                });
            }
            emitted.borrow_mut().push(event);
        });

        let placed = placed_now(&[&first]);
        glib::MainContext::default().block_on(run_merge_undo(
            OperationRequestId(1615),
            (
                Vec::new(),
                vec![Location::local(&first), target.clone()],
                placed,
            ),
            emit,
            cancellable,
            lookup,
        ));

        let events = events.borrow();
        assert_eq!(fs::read(&first)?, b"first", "{case}");
        let staged_second = fs::read(trash.join("files/second.txt"));
        match (case, events.last()) {
            ("failed", Some(OperationEvent::CompletedWithErrors { message, .. })) => {
                let check = gio::File::for_uri(unreadable)
                    .query_info(
                        "standard::type",
                        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                        gio::Cancellable::NONE,
                    )
                    .expect_err("no backend");
                assert!(
                    message.contains(&crate::services::gio_error_detail(&check)),
                    "{message}"
                );
                assert_eq!(staged_second?, b"second");
                assert!(trash.join("info/second.txt.trashinfo").exists());
                assert!(fs::symlink_metadata(&second).is_err());
            }
            ("cancelled", Some(OperationEvent::Cancelled { result, .. })) => {
                assert_eq!(result.completed, [Location::local(&first)]);
                assert!(result.failed.is_empty());
                assert_eq!(result.not_attempted, [Location::local(&second)]);
                assert_eq!(staged_second?, b"second");
                assert_eq!(fs::read(&second)?, b"merged");
            }
            ("missing", Some(OperationEvent::Restored { .. })) => {
                assert_eq!(fs::read(&second)?, b"second");
                assert!(staged_second.is_err());
            }
            (case, last) => panic!("{case}: {last:?}"),
        }
    }
    Ok(())
}

type UndoParts = (
    Vec<Location>,
    Vec<Location>,
    HashMap<Location, PlacedSnapshot>,
);

/// What undo is given of a merge, from what it reported.
fn undo_parts(plans: Vec<MergePlan>) -> UndoParts {
    let mut parts = UndoParts::default();
    for plan in plans {
        parts.0.extend(plan.created);
        parts.1.extend(plan.overwritten);
        parts.2.extend(plan.placed);
    }
    parts
}

fn run_undo(parts: UndoParts, lookup: StagedOriginalLookup) -> Vec<OperationEvent> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(1616),
        parts,
        Rc::new(move |event| emitted.borrow_mut().push(event)),
        gio::Cancellable::new(),
        lookup,
    ));
    events.take()
}

fn kept_in_trash(events: &[OperationEvent]) -> Vec<KeptItem> {
    events
        .iter()
        .filter_map(|event| match event {
            OperationEvent::ItemsReported { report, .. } => Some(report.kept_in_trash.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn kept_items(events: &[OperationEvent]) -> Vec<Location> {
    events
        .iter()
        .filter_map(|event| match event {
            OperationEvent::ItemKept { location, .. } => Some(location.clone()),
            _ => None,
        })
        .collect()
}

fn restored(events: &[OperationEvent]) -> Option<Vec<Location>> {
    match events.last() {
        Some(OperationEvent::Restored { restored, .. }) => Some(restored.clone()),
        _ => None,
    }
}

fn changed_after_the_merge(location: &Path) -> Vec<KeptItem> {
    vec![KeptItem {
        location: Location::local(location),
        cause: KeptCause::ReplacementChanged,
    }]
}

fn trash_in(root: &Path) -> PathBuf {
    root.join(format!(".Trash-{}", rustix::process::getuid().as_raw()))
}

fn staged_trash_folder(
    root: &Path,
    name: &str,
    original: &Path,
    files: &[(&str, &str)],
) -> Result<RestoreEntry, Box<dyn Error>> {
    let entry = staged_trash_fixture(root, name, original, b"")?;
    let staged = entry.physical_path.clone().ok_or("staged path")?;
    fs::remove_file(&staged)?;
    write_files(&staged, files)?;
    Ok(entry)
}

#[test]
fn a_merge_undo_deletes_only_what_the_merge_left_unchanged() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // What happened to the merged a.txt after the merge.
    for change in ["untouched", "linked", "edited", "touched", "replaced"] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        write_files(
            &source,
            &[("docs/a.txt", "incoming a"), ("docs/b.txt", "incoming b")],
        )?;
        if change == "linked" {
            fs::remove_file(source.join("docs/b.txt"))?;
            fs::hard_link(source.join("docs/a.txt"), source.join("docs/b.txt"))?;
        }
        write_files(
            &target,
            &[
                ("docs/a.txt", "old a"),
                ("docs/b.txt", "old b"),
                ("docs/stays.txt", "keep me"),
            ],
        )?;
        let (stage, _) = rename_stage(root.path().join("staging"));
        let (plans, on_merged) = recorded_plans();
        glib::MainContext::default().block_on(merge_local_with(
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
        ))?;
        let [a, b] = ["docs/a.txt", "docs/b.txt"].map(|path| target.join(path));
        match change {
            "linked" => assert_eq!(fs::metadata(&a)?.nlink(), 2, "the merge keeps the link"),
            "edited" => fs::write(&a, b"edited after the merge")?,
            "touched" => set_times(&a, 1_000_000_000, 0)?,
            "replaced" => {
                let merged = fs::metadata(&a)?;
                let other = a.with_file_name("a.new");
                fs::write(&other, fs::read(&a)?)?;
                set_times(
                    &other,
                    u64::try_from(merged.mtime())?,
                    u32::try_from(merged.mtime_nsec())?,
                )?;
                fs::rename(&other, &a)?;
            }
            _ => {}
        }
        let staged = HashMap::from([
            (
                a.clone(),
                staged_trash_fixture(root.path(), "a.txt", &a, b"old a")?,
            ),
            (
                b.clone(),
                staged_trash_fixture(root.path(), "b.txt", &b, b"old b")?,
            ),
        ]);
        let trash = trash_in(root.path());

        let events = run_undo(undo_parts(plans.take()), staged_lookup(staged));

        assert_eq!(fs::read(&b)?, b"old b", "{change}");
        assert_eq!(fs::read(target.join("docs/stays.txt"))?, b"keep me");
        if matches!(change, "untouched" | "linked") {
            assert_eq!(fs::read(&a)?, b"old a", "{change}");
            let mut undone = restored(&events).ok_or("not restored")?;
            undone.sort_by_key(Location::display_name);
            assert_eq!(undone, [Location::local(&a), Location::local(&b)]);
            assert!(kept_in_trash(&events).is_empty());
            continue;
        }
        let expected: &[u8] = match change {
            "edited" => b"edited after the merge",
            _ => b"incoming a",
        };
        assert_eq!(fs::read(&a)?, expected, "{change}");
        assert_eq!(
            restored(&events),
            Some(vec![Location::local(&b)]),
            "{change}"
        );
        assert_eq!(
            kept_in_trash(&events),
            changed_after_the_merge(&a),
            "{change}"
        );
        assert_eq!(fs::read(trash.join("files/a.txt"))?, b"old a", "{change}");
        assert!(trash.join("info/a.txt.trashinfo").exists(), "{change}");
    }
    Ok(())
}

#[test]
fn undoing_a_replace_deletes_what_it_placed_only_if_all_of_it_is_unchanged()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let rows = [
        (false, "untouched"),
        (false, "edited"),
        (true, "untouched"),
        (true, "edited"),
        (true, "added to"),
        (true, "edited during the undo"),
        (true, "undeletable"),
    ];
    for (directory, change) in rows {
        let row = format!("{change} {}", if directory { "folder" } else { "file" });
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        if directory {
            write_files(&source, &[("a.txt", "new a"), ("sub/b.txt", "new b")])?;
            write_files(&target, &[("old.txt", "old")])?;
        } else {
            fs::write(&source, b"new")?;
            fs::write(&target, b"old")?;
        }
        let replaced = RefCell::new(None);
        glib::MainContext::default().block_on(replace_local_with(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            false,
            gio::Cancellable::new(),
            None,
            super::super::Copying::default(),
            &|original, placed| *replaced.borrow_mut() = Some((original, placed)),
        ))?;
        let Some((ReplacedOriginal::Trashed(_), Some(placed))) = replaced.take() else {
            return Err(format!("{row}: the original was not replaced through Trash").into());
        };
        match (directory, change) {
            (false, "edited") => fs::write(&target, b"edited")?,
            (true, "edited") => fs::write(target.join("sub/b.txt"), b"edited b")?,
            (true, "added to") => fs::write(target.join("sub/c.txt"), b"added")?,
            _ => {}
        }
        let original = if directory {
            staged_trash_folder(root.path(), "target", &target, &[("old.txt", "old")])?
        } else {
            staged_trash_fixture(root.path(), "target", &target, b"old")?
        };
        let trash = trash_in(root.path());
        let location = Location::local(&target);
        let edited = target.join("sub/b.txt");
        let _scope = match change {
            "undeletable" => Some(Quirks {
                fail: vec![(Op::Unlink, "a.txt", 0, Errno::ACCESS)],
                ..Quirks::default()
            }),
            // Once the comparison found it unchanged, before its deletion.
            "edited during the undo" => Some(Quirks {
                before: vec![(
                    Op::Stat,
                    "b.txt",
                    1,
                    Box::new(move || drop(fs::write(&edited, b"edited b"))),
                )],
                ..Quirks::default()
            }),
            _ => None,
        }
        .map(|quirks| FsQuirks::install(root.path(), quirks));

        let undo = || {
            run_undo(
                (
                    Vec::new(),
                    vec![location.clone()],
                    HashMap::from([(location.clone(), placed.clone())]),
                ),
                staged_lookup(HashMap::from([(target.clone(), original.clone())])),
            )
        };

        let events = undo();

        if change == "undeletable" {
            let Some(OperationEvent::CompletedWithErrors { message, .. }) = events.last() else {
                panic!("{row}: {events:?}");
            };
            let denied = crate::services::io_error_detail(&io::Error::from_raw_os_error(
                Errno::ACCESS.raw_os_error(),
            ));
            let error = rust_i18n::t!(
                "Could not delete %{name}: %{error}",
                name = "a.txt",
                error = denied
            );
            assert!(message.contains(&*error), "{message}");
            assert_eq!(names(&target)?, ["a.txt"]);
            assert!(kept_in_trash(&events).is_empty());
            assert!(trash.join("info/target.trashinfo").exists());
            assert_eq!(restored(&undo()), Some(vec![location]));
            assert_eq!(names(&target)?, ["old.txt"]);
            continue;
        }
        if change == "untouched" {
            assert_eq!(restored(&events), Some(vec![location]), "{row}");
            let restored = if directory {
                assert_eq!(names(&target)?, ["old.txt"], "{row}");
                fs::read(target.join("old.txt"))?
            } else {
                fs::read(&target)?
            };
            assert_eq!(restored, b"old", "{row}");
            assert!(!trash.join("info/target.trashinfo").exists(), "{row}");
            continue;
        }
        assert_eq!(restored(&events), Some(Vec::new()), "{row}");
        assert_eq!(
            kept_in_trash(&events),
            changed_after_the_merge(&target),
            "{row}"
        );
        assert!(trash.join("info/target.trashinfo").exists(), "{row}");
        match (directory, change) {
            (false, _) => assert_eq!(fs::read(&target)?, b"edited"),
            (true, "edited during the undo") => {
                assert_eq!(fs::read(target.join("sub/b.txt"))?, b"edited b");
            }
            (true, _) => {
                assert_eq!(names(&target)?, ["a.txt", "sub"], "{row}");
                assert_eq!(fs::read(target.join("a.txt"))?, b"new a", "{row}");
                let sub = target.join("sub");
                if change == "edited" {
                    assert_eq!(names(&sub)?, ["b.txt"]);
                    assert_eq!(fs::read(sub.join("b.txt"))?, b"edited b");
                } else {
                    assert_eq!(names(&sub)?, ["b.txt", "c.txt"]);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn a_merge_undo_compares_what_it_placed_as_a_move_compares_its_sources()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let epoch = SystemTime::UNIX_EPOCH;
    let seconds = SystemTime::now().duration_since(epoch)?.as_secs() / 2 * 2;
    let cifs = |options| {
        move || Quirks {
            mount_type: Some(("cifs", options)),
            ..Quirks::default()
        }
    };
    let coarse = || Quirks {
        ctime_is_mtime: true,
        ..Quirks::vfat()
    };
    let unreadable = move || Quirks {
        fail: vec![(Op::OpenSource, "merged.txt", 0, Errno::ACCESS)],
        ..coarse()
    };
    // Inode numbers that change once the kernel evicts an entry, and a
    // rewrite within one tick of coarse times, which leaves the size and
    // times as they were.
    type MakeQuirks = Box<dyn Fn() -> Quirks>;
    let rows: [(&str, MakeQuirks, &str, bool); 6] = [
        (
            "cifs noserverino",
            Box::new(cifs("rw,noserverino")),
            "evicted",
            true,
        ),
        ("cifs", Box::new(cifs("rw,serverino")), "evicted", false),
        ("vfat", Box::new(coarse), "untouched", true),
        ("vfat", Box::new(coarse), "rewritten", false),
        ("vfat", Box::new(coarse), "rewritten in a folder", false),
        ("vfat", Box::new(unreadable), "unread", false),
    ];
    for (mount, quirks, change, undone) in rows {
        let row = format!("{mount}, {change}");
        // Waiting for a tick to pass moves it on.
        let _clock =
            ClockGuard::set(epoch + Duration::from_secs(seconds) + Duration::from_millis(500));
        let root = tempfile::tempdir()?;
        let name = if change == "rewritten in a folder" {
            "folder"
        } else {
            "merged.txt"
        };
        let placed_at = root.path().join(name);
        let merged = if name == "folder" {
            fs::create_dir(&placed_at)?;
            placed_at.join("merged.txt")
        } else {
            placed_at.clone()
        };
        fs::write(&merged, b"merged")?;
        set_times(&merged, seconds, 0)?;
        let _scope = FsQuirks::install(root.path(), quirks());
        let placed = placed_now(&[&placed_at]);
        match change {
            "evicted" => super::fs_quirks::renumbered(&merged),
            "rewritten" | "rewritten in a folder" => {
                fs::write(&merged, b"MERGED")?;
                set_times(&merged, seconds, 0)?;
            }
            _ => {}
        }
        let original = staged_trash_fixture(root.path(), name, &placed_at, b"original")?;

        let events = run_undo(
            (Vec::new(), vec![Location::local(&placed_at)], placed),
            staged_lookup(HashMap::from([(placed_at.clone(), original)])),
        );

        assert!(restored(&events).is_some(), "{row}: {events:?}");
        if undone {
            assert_eq!(fs::read(&merged)?, b"original", "{row}");
            assert!(kept_in_trash(&events).is_empty(), "{row}");
        } else {
            assert_ne!(fs::read(&merged)?, b"original", "{row}");
            assert_eq!(
                kept_in_trash(&events),
                changed_after_the_merge(&placed_at),
                "{row}"
            );
            assert!(
                trash_in(root.path())
                    .join(format!("info/{name}.trashinfo"))
                    .exists(),
                "{row}"
            );
        }
    }
    Ok(())
}

#[test]
fn a_merge_undo_cancelled_while_it_compares_an_item_leaves_it_whole() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let epoch = SystemTime::UNIX_EPOCH;
    let seconds = SystemTime::now().duration_since(epoch)?.as_secs() / 2 * 2;
    // Whether an item it kept as edited came first.
    for after_a_kept_item in [false, true] {
        let _clock =
            ClockGuard::set(epoch + Duration::from_secs(seconds) + Duration::from_millis(500));
        let root = tempfile::tempdir()?;
        let merged = root.path().join("merged.txt");
        let edited = root.path().join("edited.txt");
        for path in [&merged, &edited] {
            fs::write(path, b"merged")?;
            set_times(path, seconds, 0)?;
        }
        let cancellable = gio::Cancellable::new();
        let cancel = cancellable.clone();
        let cancel: Racer = Box::new(move || cancel.cancel());
        // The first open is the record's own, the second the comparison's.
        let _scope = FsQuirks::install(
            root.path(),
            Quirks {
                ctime_is_mtime: true,
                before: vec![(Op::OpenSource, "merged.txt", 1, cancel)],
                ..Quirks::vfat()
            },
        );
        let placed = placed_now(&[&merged, &edited]);
        fs::write(&edited, b"edited after the merge")?;
        let mut overwritten = vec![Location::local(&merged)];
        let mut staged = HashMap::from([(
            merged.clone(),
            staged_trash_fixture(root.path(), "merged.txt", &merged, b"original")?,
        )]);
        if after_a_kept_item {
            overwritten.insert(0, Location::local(&edited));
            staged.insert(
                edited.clone(),
                staged_trash_fixture(root.path(), "edited.txt", &edited, b"original")?,
            );
        }
        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();

        glib::MainContext::default().block_on(run_merge_undo(
            OperationRequestId(1616),
            (Vec::new(), overwritten, placed),
            Rc::new(move |event| emitted.borrow_mut().push(event)),
            cancellable,
            staged_lookup(staged),
        ));

        let events = events.borrow();
        let Some(OperationEvent::Cancelled { result, .. }) = events.last() else {
            panic!("expected a cancel, got {events:?}");
        };
        let kept: &[Location] = if after_a_kept_item {
            &[Location::local(&edited)]
        } else {
            &[]
        };
        assert_eq!(result.completed, kept);
        assert!(result.failed.is_empty());
        assert_eq!(result.not_attempted, [Location::local(&merged)]);
        assert_eq!(fs::read(&merged)?, b"merged");
        assert_eq!(
            fs::read(trash_in(root.path()).join("files/merged.txt"))?,
            b"original"
        );
    }
    Ok(())
}

#[test]
fn what_an_operation_placed_is_read_back_only_until_its_tick_passes() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let epoch = SystemTime::UNIX_EPOCH;
    let seconds = SystemTime::now().duration_since(epoch)?.as_secs() / 2 * 2;
    for change in ["untouched", "rewritten where read"] {
        let _clock =
            ClockGuard::set(epoch + Duration::from_secs(seconds) + Duration::from_millis(500));
        let root = tempfile::tempdir()?;
        let archive = root.path().join("archive.zip");
        fs::write(&archive, b"0123456789abcdef")?;
        set_times(&archive, seconds, 0)?;
        let past_the_tick = epoch + Duration::from_secs(seconds + 3);
        let read_on = Arc::new(AtomicBool::new(false));
        let reading = read_on.clone();
        let _scope = FsQuirks::install(
            root.path(),
            Quirks {
                ctime_is_mtime: true,
                short_io: Some(4),
                before: vec![
                    (
                        Op::Read,
                        "archive.zip",
                        1,
                        Box::new(move || {
                            super::super::copy_engine::set_clock_for_test(Some(past_the_tick));
                        }),
                    ),
                    (
                        Op::Read,
                        "archive.zip",
                        2,
                        Box::new(move || reading.store(true, Ordering::Release)),
                    ),
                ],
                ..Quirks::vfat()
            },
        );

        let placed = placed_now(&[&archive]);

        assert!(
            !read_on.load(Ordering::Acquire),
            "{change}: read past the tick"
        );
        if change == "rewritten where read" {
            fs::write(&archive, b"0123XXXX89abcdef")?;
            set_times(&archive, seconds, 0)?;
        }
        let original = staged_trash_fixture(root.path(), "archive.zip", &archive, b"original")?;
        let events = run_undo(
            (Vec::new(), vec![Location::local(&archive)], placed),
            staged_lookup(HashMap::from([(archive.clone(), original)])),
        );
        if change == "untouched" {
            assert_eq!(fs::read(&archive)?, b"original");
            assert!(kept_in_trash(&events).is_empty(), "{events:?}");
        } else {
            assert_eq!(fs::read(&archive)?, b"0123XXXX89abcdef");
            assert_eq!(kept_in_trash(&events), changed_after_the_merge(&archive));
        }
    }
    Ok(())
}

#[test]
fn an_undo_keeps_what_a_cancel_stopped_the_operation_from_recording() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let epoch = SystemTime::UNIX_EPOCH;
    let seconds = SystemTime::now().duration_since(epoch)?.as_secs() / 2 * 2;
    let _clock = ClockGuard::set(epoch + Duration::from_secs(seconds) + Duration::from_millis(500));
    // While the record reads a file back, or walks a folder.
    for (folder, op, name) in [(false, Op::Read, "placed"), (true, Op::Stat, "sub")] {
        let root = tempfile::tempdir()?;
        let placed = root.path().join("placed");
        if folder {
            write_files(&placed, &[("a.txt", "a"), ("sub/b.txt", "b")])?;
        } else {
            fs::write(&placed, b"placed")?;
            set_times(&placed, seconds, 0)?;
        }
        let cancellable = gio::Cancellable::new();
        let cancel = cancellable.clone();
        let _scope = FsQuirks::install(
            root.path(),
            Quirks {
                ctime_is_mtime: !folder,
                before: vec![(op, name, 0, Box::new(move || cancel.cancel()))],
                ..if folder {
                    Quirks::default()
                } else {
                    Quirks::vfat()
                }
            },
        );

        let recorded = glib::MainContext::default().block_on(super::super::copy_engine::placed(
            placed.clone(),
            super::super::copy_engine::placed_budget(),
            &cancellable,
        ));

        let location = Location::local(&placed);
        let original = staged_trash_fixture(root.path(), "placed", &placed, b"original")?;
        let events = run_undo(
            (
                Vec::new(),
                vec![location.clone()],
                recorded
                    .map(|recorded| HashMap::from([(location, recorded)]))
                    .unwrap_or_default(),
            ),
            staged_lookup(HashMap::from([(placed.clone(), original)])),
        );
        assert_eq!(kept_in_trash(&events), changed_after_the_merge(&placed));
        assert!(trash_in(root.path()).join("info/placed.trashinfo").exists());
        if folder {
            assert_eq!(names(&placed)?, ["a.txt", "sub"]);
        } else {
            assert_eq!(fs::read(&placed)?, b"placed");
        }
    }
    Ok(())
}

#[test]
fn an_undo_keeps_what_was_too_big_to_record() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // A replaced folder of four entries; a merge's two overwritten files,
    // which share the limit.
    for (merged, limit) in [(false, 3), (true, 1)] {
        let _limit = PlacedEntriesGuard::set(limit);
        let root = tempfile::tempdir()?;
        let target = root.path().join("target");
        if !merged {
            write_files(&target, &[("a.txt", "a"), ("sub/b.txt", "b")])?;
            let placed = placed_now(&[&target]);
            let original =
                staged_trash_folder(root.path(), "target", &target, &[("old.txt", "old")])?;
            let events = run_undo(
                (Vec::new(), vec![Location::local(&target)], placed),
                staged_lookup(HashMap::from([(target.clone(), original)])),
            );
            assert_eq!(names(&target)?, ["a.txt", "sub"]);
            assert_eq!(kept_in_trash(&events), changed_after_the_merge(&target));
            assert!(trash_in(root.path()).join("info/target.trashinfo").exists());
            continue;
        }
        let source = root.path().join("source");
        write_files(&source, &[("a.txt", "incoming a"), ("b.txt", "incoming b")])?;
        write_files(&target, &[("a.txt", "old a"), ("b.txt", "old b")])?;
        let (stage, _) = rename_stage(root.path().join("staging"));
        let (plans, on_merged) = recorded_plans();
        glib::MainContext::default().block_on(merge_local_with(
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
        ))?;
        let [a, b] = ["a.txt", "b.txt"].map(|name| target.join(name));
        let staged = HashMap::from([
            (
                a.clone(),
                staged_trash_fixture(root.path(), "a.txt", &a, b"old a")?,
            ),
            (
                b.clone(),
                staged_trash_fixture(root.path(), "b.txt", &b, b"old b")?,
            ),
        ]);
        let events = run_undo(undo_parts(plans.take()), staged_lookup(staged));
        assert_eq!(fs::read(&a)?, b"old a");
        assert_eq!(fs::read(&b)?, b"incoming b");
        assert_eq!(kept_in_trash(&events), changed_after_the_merge(&b));
    }
    Ok(())
}

#[test]
fn a_created_tree_too_big_to_record_leaves_its_entries_to_the_rest() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let _limit = PlacedEntriesGuard::set(3);
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    write_files(
        &source,
        &[
            ("big/1.txt", "1"),
            ("big/2.txt", "2"),
            ("big/3.txt", "3"),
            ("z.txt", "incoming z"),
        ],
    )?;
    write_files(&target, &[("stays.txt", "keep me")])?;
    let _scope = FsQuirks::install(root.path(), without_trash(Quirks::default()));
    let (merged, plans) = merge_through_trash(&source, &target, false, real_copy());
    merged?;
    let [big, z] = ["big", "z.txt"].map(|name| target.join(name));
    let parts = undo_parts(plans);
    assert_eq!(placed_paths(&parts.2), std::slice::from_ref(&z));

    let events = run_undo(parts, staged_lookup(HashMap::new()));

    assert!(!z.exists(), "{events:?}");
    assert_eq!(names(&big)?, ["1.txt", "2.txt", "3.txt"]);
    assert_eq!(kept_items(&events), [Location::local(&big)]);
    Ok(())
}

#[test]
fn a_paste_records_no_more_than_its_limit_across_its_merges() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let _limit = PlacedEntriesGuard::set(1);
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let destination = root.path().join("destination");
    write_files(
        &sources,
        &[("a/first.txt", "first"), ("b/second.txt", "second")],
    )?;
    write_files(
        &destination,
        &[("a/stays.txt", "keep me"), ("b/stays.txt", "keep me")],
    )?;
    let _scope = FsQuirks::install(root.path(), without_trash(Quirks::default()));

    let events = run_paste(PasteRequest {
        id: OperationRequestId(1618),
        destination: Location::local(&destination),
        items: ["a", "b"]
            .map(|name| PasteItem {
                source: Location::local(sources.join(name)),
                conflict: TransferConflict::Merge,
            })
            .into(),
        move_sources: false,
    });

    let placed = events
        .iter()
        .filter_map(|event| match event {
            OperationEvent::Merged { placed, .. } => Some(placed.clone()),
            _ => None,
        })
        .flatten()
        .collect::<HashMap<_, _>>();
    assert_eq!(placed_paths(&placed), [destination.join("a/first.txt")]);
    Ok(())
}

#[test]
fn a_merge_records_what_it_creates_only_where_undo_cannot_trash_it() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // A created folder of four entries sorts ahead of z.txt, with room to
    // record three.
    let _limit = PlacedEntriesGuard::set(3);
    for overwrites in [true, false] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        write_files(
            &source,
            &[
                ("big/1.txt", "1"),
                ("big/2.txt", "2"),
                ("big/3.txt", "3"),
                ("z.txt", "incoming z"),
            ],
        )?;
        write_files(&target, &[("stays.txt", "keep me")])?;
        let z = target.join("z.txt");
        if overwrites {
            fs::write(&z, b"old z")?;
        }
        // The Trash took the original, or is offered where there is none.
        let _scope = (!overwrites).then(|| {
            FsQuirks::install(
                root.path(),
                Quirks {
                    trash: Trash::Refused,
                    ..Quirks::default()
                },
            )
        });
        let (stage, _) = rename_stage(root.path().join("staging"));
        let (plans, on_merged) = recorded_plans();

        glib::MainContext::default().block_on(merge_local_with(
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
        ))?;

        let parts = undo_parts(plans.take());
        if !overwrites {
            assert!(parts.2.is_empty(), "{:?}", placed_paths(&parts.2));
            continue;
        }
        assert_eq!(placed_paths(&parts.2), std::slice::from_ref(&z));
        let staged = HashMap::from([(
            z.clone(),
            staged_trash_fixture(root.path(), "z.txt", &z, b"old z")?,
        )]);
        let events = run_undo((Vec::new(), parts.1, parts.2), staged_lookup(staged));
        assert_eq!(fs::read(&z)?, b"old z", "{events:?}");
    }
    Ok(())
}

#[test]
fn a_merge_undo_without_trash_deletes_only_what_the_merge_created_unchanged()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // What happened to the created new.txt and sub after the merge, whether
    // undo has Trash, and whether the mount places folders or copies them in
    // place.
    let rows = [
        ("untouched", Trash::Unsupported, true),
        ("untouched", Trash::Unsupported, false),
        ("edited", Trash::Unsupported, true),
        ("added to", Trash::Unsupported, true),
        ("unrecorded", Trash::Unsupported, true),
        ("edited", Trash::Supported, true),
    ];
    for (change, trash, places_folders) in rows {
        let row = format!("{change}, {trash:?}, places folders: {places_folders}");
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        write_files(
            &source,
            &[
                ("docs/new.txt", "incoming"),
                ("docs/sub/a.txt", "incoming a"),
                ("docs/sub/b.txt", "incoming b"),
            ],
        )?;
        write_files(&target, &[("docs/stays.txt", "keep me")])?;
        let placement = if places_folders {
            Quirks::default()
        } else {
            Quirks {
                mount_type: Some(("ext4", "rw")),
                ..Quirks::without_folder_placement()
            }
        };
        let scope = FsQuirks::install(root.path(), Quirks { trash, ..placement });
        if !places_folders {
            learn_mount_capability(scope.key(), |capabilities| {
                capabilities.no_replace = Some(false);
                capabilities.hard_links = Some(true);
                capabilities.rename_over_empty_directory = Some(false);
            });
        }
        let (merged, plans) = merge_through_trash(&source, &target, false, real_copy());
        merged?;
        let [file, folder] = ["docs/new.txt", "docs/sub"].map(|path| target.join(path));
        let mut parts = undo_parts(plans);
        match change {
            "edited" => {
                fs::write(&file, b"edited after the merge")?;
                fs::write(folder.join("a.txt"), b"edited after the merge")?;
            }
            "added to" => fs::write(folder.join("c.txt"), b"added after the merge")?,
            "unrecorded" => parts.2.clear(),
            _ => {}
        }

        let events = run_undo(parts, staged_lookup(HashMap::new()));

        assert_eq!(
            fs::read(target.join("docs/stays.txt"))?,
            b"keep me",
            "{row}"
        );
        let mut trashed = trashed_items(&events)
            .into_iter()
            .map(|(location, _)| location)
            .collect::<Vec<_>>();
        trashed.sort_by_key(Location::display_name);
        let kept: &[&Path] = match (change, trash) {
            (_, Trash::Supported) => {
                assert_eq!(
                    trashed,
                    [Location::local(&file), Location::local(&folder)],
                    "{row}: Trash takes them whatever they hold"
                );
                &[]
            }
            ("untouched", _) => &[],
            ("added to", _) => &[&folder],
            _ => &[&file, &folder],
        };
        if !matches!(trash, Trash::Supported) {
            assert!(trashed.is_empty(), "{row}");
        }
        for item in [&file, &folder] {
            assert_eq!(
                item.exists(),
                kept.contains(&item.as_path()),
                "{row}: {item:?}"
            );
        }
        let mut kept_items = kept_items(&events);
        kept_items.sort_by_key(Location::display_name);
        assert_eq!(
            kept_items,
            kept.iter().map(Location::local).collect::<Vec<_>>(),
            "{row}: nothing is left to undo of what was kept"
        );
        if kept.contains(&folder.as_path()) {
            let mut expected = vec!["a.txt", "b.txt"];
            if change == "added to" {
                expected.push("c.txt");
            }
            assert_eq!(names(&folder)?, expected, "{row}: kept whole");
            assert_eq!(fs::read(folder.join("b.txt"))?, b"incoming b", "{row}");
        }
        if kept.is_empty() {
            assert!(
                matches!(events.last(), Some(OperationEvent::Restored { .. })),
                "{row}: {events:?}"
            );
            continue;
        }
        let Some(OperationEvent::CompletedWithErrors {
            deleted_locations,
            message,
            ..
        }) = events.last()
        else {
            panic!("{row}: {events:?}");
        };
        let deleted: Vec<_> = [&file, &folder]
            .into_iter()
            .filter(|item| !kept.contains(&item.as_path()))
            .map(Location::local)
            .collect();
        assert_eq!(*deleted_locations, deleted, "{row}");
        let reason = crate::i18n::tr("It was kept, as it may have changed since it was copied");
        for item in kept {
            let line = super::super::item_error(&Location::local(item).display_name(), &reason);
            assert!(message.contains(&line), "{row}: {message}");
        }
    }
    Ok(())
}

#[test]
fn a_merge_undo_without_trash_deletes_what_its_paste_copied_only_if_unchanged()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for edited in [false, true] {
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let destination = root.path().join("destination");
        write_files(
            &sources,
            &[
                ("notes.txt", "notes"),
                ("docs/new.txt", "incoming"),
                ("keep.txt", "incoming keep"),
                ("plain/a.txt", "a"),
            ],
        )?;
        write_files(
            &destination,
            &[("docs/stays.txt", "keep me"), ("keep.txt", "original keep")],
        )?;
        let _scope = FsQuirks::install(root.path(), without_trash(Quirks::default()));
        let item = |name: &str, conflict| PasteItem {
            source: Location::local(sources.join(name)),
            conflict,
        };

        let events = run_paste(PasteRequest {
            id: OperationRequestId(1619),
            destination: Location::local(&destination),
            items: vec![
                item("notes.txt", TransferConflict::FailIfExists),
                item("docs", TransferConflict::Merge),
                item("keep.txt", TransferConflict::KeepBoth),
                item("plain", TransferConflict::FailIfExists),
            ],
            move_sources: false,
        });
        // As the app gathers the undo of a paste with a merge.
        let mut parts = UndoParts::default();
        for event in events {
            match event {
                OperationEvent::TransferProgress {
                    created_location, ..
                } => parts.0.extend(created_location),
                OperationEvent::Merged {
                    created, placed, ..
                } => {
                    parts.0.extend(created);
                    parts.2.extend(placed);
                }
                _ => {}
            }
        }
        let [notes, plain] = ["notes.txt", "plain"].map(|name| destination.join(name));
        if edited {
            fs::write(&notes, b"edited after the paste")?;
            fs::write(plain.join("a.txt"), b"edited after the paste")?;
        }

        let undone = run_undo(parts, staged_lookup(HashMap::new()));

        let mut left = vec!["docs", "keep.txt"];
        let mut kept = Vec::new();
        if edited {
            left.extend(["notes.txt", "plain"]);
            kept.extend([Location::local(&notes), Location::local(&plain)]);
        }
        assert_eq!(names(&destination)?, left, "edited: {edited}: {undone:?}");
        assert_eq!(names(&destination.join("docs"))?, ["stays.txt"]);
        assert_eq!(fs::read(destination.join("keep.txt"))?, b"original keep");
        let mut kept_items = kept_items(&undone);
        kept_items.sort_by_key(Location::display_name);
        assert_eq!(kept_items, kept, "edited: {edited}");
    }
    Ok(())
}

#[test]
fn a_created_folder_changed_while_undo_deletes_it_fails_as_not_empty() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let folder = root.path().join("sub");
    write_files(&folder, &[("a.txt", "a"), ("b.txt", "b")])?;
    let placed = placed_now(&[&folder]);
    let added = folder.join("c.txt");
    let racer_added = added.clone();
    let racer: Racer = Box::new(move || {
        fs::write(&racer_added, b"added during the undo").expect("added entry");
    });
    let _scope = FsQuirks::install(
        root.path(),
        without_trash(Quirks {
            after: vec![(Op::Unlink, "a.txt", 0, racer)],
            ..Quirks::default()
        }),
    );

    let events = run_undo(
        (vec![Location::local(&folder)], Vec::new(), placed),
        staged_lookup(HashMap::new()),
    );

    assert_eq!(names(&folder)?, ["c.txt"]);
    assert!(
        kept_items(&events).is_empty(),
        "what is left is still to undo"
    );
    let Some(OperationEvent::CompletedWithErrors {
        deleted_locations,
        message,
        ..
    }) = events.last()
    else {
        panic!("{events:?}");
    };
    assert!(deleted_locations.is_empty());
    let not_empty = crate::services::io_error_detail(&io::Error::from_raw_os_error(
        Errno::NOTEMPTY.raw_os_error(),
    ));
    assert!(
        message.contains(&super::super::item_error("sub", &not_empty)),
        "{message}"
    );
    Ok(())
}

fn without_trash(quirks: Quirks) -> Quirks {
    Quirks {
        trash: Trash::Unsupported,
        ..quirks
    }
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
    copy_into_target: CopyStep,
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
        (Trash::Unsupported, Quirks::sshfs(), false),
        (Trash::Unsupported, Quirks::vboxsf(), true),
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
        assert!(quirks.used_name(".strata-merge-*"));
        assert_eq!(fs::read(target.join("shared.txt"))?, b"new");
        assert_eq!(
            fs::read(target.join("nested/deep/shared.txt"))?,
            b"new deep"
        );
        assert_eq!(fs::read(target.join("nested/stays.txt"))?, b"keep me");
        assert_eq!(names(&target)?, ["incoming.txt", "nested", "shared.txt"]);
        assert_eq!(source.exists(), !move_source);
        assert_eq!(
            plans[0].created,
            vec![Location::local(target.join("incoming.txt"))]
        );
        let (_, overwritten, placed) = undo_parts(plans);
        assert!(
            overwritten.is_empty(),
            "originals deleted without Trash cannot be restored by undo"
        );
        let recorded = if move_source {
            Vec::new()
        } else {
            vec![Location::local(target.join("incoming.txt"))]
        };
        assert_eq!(
            placed.into_keys().collect::<Vec<_>>(),
            recorded,
            "{trash:?}: only what undo deletes is recorded"
        );
    }
    Ok(())
}

#[test]
fn without_trash_an_original_the_merge_left_nothing_in_place_of_stays() -> Result<(), Box<dyn Error>>
{
    use super::fs_quirks::Racer;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let rewrite = |path: PathBuf, contents: &'static [u8]| -> Racer {
        Box::new(move || fs::write(&path, contents).expect("rewrite the source"))
    };
    for row in ["left out", "kept at its source", "put back fails"] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source)?;
        write_files(&target, &[("x", "original"), ("y", "kept")])?;
        let moving = row == "kept at its source";
        let mut source_quirks = Quirks::default();
        let _socket = if moving {
            fs::write(source.join("x"), b"incoming")?;
            source_quirks.before = vec![
                (Op::Read, "x", 0, rewrite(source.join("x"), b"second")),
                (Op::Read, "x", 1, rewrite(source.join("x"), b"third")),
            ];
            None
        } else {
            Some(std::os::unix::net::UnixListener::bind(source.join("x"))?)
        };
        let mut target_quirks = without_trash(Quirks::default());
        if row == "put back fails" {
            target_quirks.fail = vec![(Op::Rename, "x", 1, Errno::IO)];
        }
        let _source = FsQuirks::install(&source, source_quirks);
        let _target = FsQuirks::install(&target, target_quirks);

        let (result, _) = merge_through_trash(&source, &target, moving, real_copy());

        if row == "put back fails" {
            let aside = aside_folder(&target)?.ok_or("the original stays aside")?;
            assert!(result.is_err(), "{row}");
            assert_eq!(
                fs::read(target.join(aside).join("x"))?,
                b"original",
                "{row}"
            );
            continue;
        }
        assert!(result.is_ok(), "{row}: {result:?}");
        assert_eq!(names(&target)?, ["x", "y"], "{row}");
        assert_eq!(fs::read(target.join("x"))?, b"original", "{row}");
        if moving {
            assert_eq!(fs::read(source.join("x"))?, b"third", "{row}");
        }
    }
    Ok(())
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
            &[
                ("a.txt", "old a"),
                ("b.txt", "old b"),
                ("stays.txt", "keep me"),
            ],
        )?;
        let _quirks = FsQuirks::install(root.path(), without_trash(Quirks::default()));
        let partial: CopyStep = Rc::new(move |_, target, _, _| {
            Box::pin(async move {
                let path = target
                    .path()
                    .ok_or_else(|| io_error("missing target path"))?;
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
        write_files(
            &source,
            &[
                ("a.txt", "new a"),
                ("b.txt", "new b"),
                ("mount/c.txt", "new c"),
            ],
        )?;
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
        let _nested = (case == "nested mount")
            .then(|| FsQuirks::install(&target.join("mount"), without_trash(Quirks::default())));
        let copied = Rc::new(Cell::new(false));
        let copy_observed = copied.clone();

        let (result, plans) = merge_through_trash(
            &source,
            &target,
            false,
            Rc::new(move |_, _, _, _| {
                copy_observed.set(true);
                Box::pin(async { Ok(MergeCopied::default()) })
            }),
        );

        let error = result.expect_err(case);
        assert!(
            error.message().contains("out of the way"),
            "{case}: {error}"
        );
        assert!(!copied.get(), "{case}");
        assert_eq!(
            quirks.used_name(".strata-merge-*"),
            case != "no safe placement",
            "{case}"
        );
        for (path, contents) in target_files {
            assert_eq!(fs::read(target.join(path))?, contents.as_bytes(), "{case}");
        }
        assert_eq!(names(&target)?, ["a.txt", "b.txt", "mount"], "{case}");
        assert!(plans.iter().all(|plan| plan.overwritten.is_empty()));
    }
    Ok(())
}

#[test]
fn merge_originals_are_deleted_only_once_the_merge_is_flushed() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for flushed in [true, false] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        write_files(&source, &[("shared.txt", "new")])?;
        write_files(&target, &[("shared.txt", "old")])?;
        let aside_during_flush = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (record, watched) = (aside_during_flush.clone(), target.clone());
        let racer: super::fs_quirks::Racer = Box::new(move || {
            let kept = aside_folder(&watched).is_ok_and(|folder| folder.is_some());
            record
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(kept);
        });
        let _quirks = FsQuirks::install(
            root.path(),
            without_trash(Quirks {
                before: vec![(Op::SyncFs, "*", 0, racer)],
                fail: if flushed {
                    Vec::new()
                } else {
                    vec![(Op::SyncFs, "*", 0, Errno::IO)]
                },
                ..Quirks::default()
            }),
        );

        let (result, _) = merge_through_trash(&source, &target, true, real_copy());

        assert_eq!(
            *aside_during_flush
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            [true],
            "one flush before the originals are deleted, shared with the move"
        );
        assert_eq!(fs::read(target.join("shared.txt"))?, b"new");
        if flushed {
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(names(&target)?, ["shared.txt"]);
            assert!(!source.exists());
            continue;
        }
        let aside = aside_folder(&target)?.ok_or("the originals stay aside")?;
        assert_eq!(fs::read(target.join(&aside).join("shared.txt"))?, b"old");
        assert_eq!(fs::read(source.join("shared.txt"))?, b"new");
        let error = result.expect_err("the originals and the source are kept");
        assert!(
            error.message().contains(&aside)
                && error.message().contains("writes may still be pending"),
            "{error}"
        );
    }
    Ok(())
}

/// Lists local folders, watching them only when `watched`, as where no
/// folder monitor is available.
struct Listing {
    watched: bool,
}

impl crate::services::FileSource for Listing {
    fn validate_location(
        &self,
        location: &Location,
    ) -> Result<(), crate::services::LocationValidationError> {
        crate::adapters::LocalFileSource.validate_location(location)
    }

    fn validate_location_async(
        &self,
        location: Location,
        emit: Rc<dyn Fn(Result<(), crate::services::LocationValidationError>)>,
    ) -> crate::services::LoadHandle {
        crate::adapters::LocalFileSource.validate_location_async(location, emit)
    }

    fn enumerate(
        &self,
        request: crate::services::DirectoryRequest,
        emit: Rc<dyn Fn(crate::services::DirectoryEvent)>,
    ) -> crate::services::LoadHandle {
        crate::adapters::LocalFileSource.enumerate(request, emit)
    }

    fn watch(
        &self,
        location: Location,
        include_hidden: bool,
        notify: Rc<dyn Fn(crate::services::DirectoryChange)>,
    ) -> Option<crate::services::LoadHandle> {
        self.watched
            .then(|| crate::adapters::LocalFileSource.watch(location, include_hidden, notify))
            .flatten()
    }
}

#[test]
fn a_merge_undo_drops_from_view_only_what_it_removed() {
    crate::test_support::gtk_test(
        "adapters::local_operations::tests::merge::a_merge_undo_drops_from_view_only_what_it_removed",
        || {
            check_a_merge_undo_drops_from_view_only_what_it_removed().expect("merge undo view");
        },
    );
}

fn check_a_merge_undo_drops_from_view_only_what_it_removed() -> Result<(), Box<dyn Error>> {
    use crate::app::{Browser, BrowserEvent};
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let context = glib::MainContext::default();
    let pump_until = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            if Instant::now() > deadline {
                return Err(format!("timed out waiting until {what}"));
            }
            context.iteration(false);
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    };
    let sandbox = glib::user_data_dir()
        .parent()
        .ok_or("the test's data folder")?
        .to_path_buf();
    let mut failures = Vec::new();
    let rows = [false, true].into_iter().flat_map(|failing| {
        [(false, false), (true, false), (false, true), (true, true)]
            .map(|(background, watched)| (failing, background, watched))
    });
    // Without a monitor, the folder learns only from the undo's outcome.
    for (failing, background, watched) in rows {
        let row = format!(
            "undo fails an item: {failing}, merged in the background: {background}, watched: {watched}"
        );
        let root = tempfile::tempdir_in(glib::home_dir())?;
        let source = root.path().join("source");
        let folder = root.path().join("destination/folder");
        write_files(
            &source,
            &[
                ("folder/a.txt", "incoming a"),
                ("folder/b.txt", "incoming b"),
                ("folder/c.txt", "incoming c"),
                ("folder/d.txt", "incoming d"),
                ("folder/new.txt", "new"),
            ],
        )?;
        write_files(
            &folder,
            &[
                ("a.txt", "original a"),
                ("b.txt", "original b"),
                ("c.txt", "original c"),
                ("d.txt", "original d"),
            ],
        )?;
        // The Trash shares the scope, so it is on the folder's mount.
        let _scope = FsQuirks::install(
            &sandbox,
            Quirks {
                fail: if failing {
                    vec![(Op::Unlink, "c.txt", 0, Errno::ACCESS)]
                } else {
                    Vec::new()
                },
                ..Quirks::default()
            },
        );
        let browser = Browser::new(Rc::new(Listing { watched }));
        browser.set_operation_provider(Rc::new(LocalOperationProvider));
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = events.clone();
        browser.observe(move |event| {
            observed
                .borrow_mut()
                .push(crate::test_support::operations::operation_event(event).clone());
        });
        let names = || {
            let mut names = browser
                .with_column_entries(0, |entries| {
                    entries
                        .iter()
                        .map(|entry| entry.display_name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            names.sort();
            names
        };

        browser.transfer(
            Location::local(root.path().join("destination")),
            vec![PasteItem {
                source: Location::local(source.join("folder")),
                conflict: TransferConflict::Merge,
            }],
            false,
            false,
        );
        if background {
            let id = browser.last_started_operation().ok_or("the merge")?;
            assert!(browser.background_file_operation(id), "{row}");
        }
        pump_until(&format!("{row}: the merge ends"), &|| {
            events
                .borrow()
                .iter()
                .any(|event| matches!(event, BrowserEvent::TransferCompleted))
        })?;
        fs::write(folder.join("b.txt"), "edited after the merge")?;
        fs::remove_file(folder.join("d.txt"))?;
        browser.navigate(Location::local(&folder));
        pump_until(&format!("{row}: the folder lists the merge"), &|| {
            names() == ["a.txt", "b.txt", "c.txt", "new.txt"]
        })?;
        let (generation, created, overwritten) =
            browser.pending_undo_merge().ok_or("the merge's undo")?;
        events.borrow_mut().clear();

        assert!(
            browser.undo_merge(generation, created, overwritten),
            "{row}"
        );
        pump_until(&format!("{row}: the undo ends"), &|| {
            events
                .borrow()
                .iter()
                .any(|event| matches!(event, BrowserEvent::RestorationFinished { .. }))
        })?;

        assert_eq!(
            events
                .borrow()
                .iter()
                .any(|event| matches!(event, BrowserEvent::OperationCompletedWithErrors { .. })),
            failing,
            "{row}"
        );
        assert_eq!(fs::read(folder.join("a.txt"))?, b"original a", "{row}");
        assert_eq!(
            fs::read(folder.join("b.txt"))?,
            b"edited after the merge",
            "{row}"
        );
        assert_eq!(
            fs::read(folder.join("c.txt"))?,
            if failing {
                &b"incoming c"[..]
            } else {
                b"original c"
            },
            "{row}"
        );
        assert_eq!(fs::read(folder.join("d.txt"))?, b"original d", "{row}");
        assert!(!folder.join("new.txt").exists(), "{row}");
        let expected = ["a.txt", "b.txt", "c.txt", "d.txt"];
        // A monitor may still be reporting what the undo changed, and a
        // folder without one reloads.
        if pump_until(&row, &|| names() == expected).is_err() {
            failures.push(format!("{row}: the folder lists {:?}", names()));
        }
        assert_eq!(
            browser
                .pending_undo_merge()
                .map(|(_, created, overwritten)| (created, overwritten)),
            failing.then(|| (Vec::new(), vec![Location::local(folder.join("c.txt"))])),
            "{row}: only what failed is left to undo"
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}
