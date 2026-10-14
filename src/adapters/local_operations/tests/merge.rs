// SPDX-License-Identifier: MIT

use rustix::io::Errno;

use super::fs_quirks::{Op, Trash};
use super::*;
use crate::adapters::volume::learn_mount_capability;

fn real_copy() -> CopyStep {
    super::super::merge_copy(None, false)
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
            copy_into_target: Rc::new(|source, target, _, cancellable| {
                let copied = copy_recursively_fat_family(source, target, true, cancellable);
                Box::pin(async move { copied.await.map(|()| MergeCopied::default()) })
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
    let undone = Rc::new(RefCell::new(Vec::new()));
    let emitted = undone.clone();
    glib::MainContext::default().block_on(run_merge_undo(
        OperationRequestId(95),
        vec![Location::local(folder.join("Makefile")), saved_as],
        Vec::new(),
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
    for has_trash in [true, false] {
        let root = tempfile::tempdir()?;
        let created = ["first.txt", "second.txt"].map(|name| root.path().join(name));
        for item in &created {
            fs::write(item, b"merged")?;
        }
        let cancellable = gio::Cancellable::new();
        let cancel = cancellable.clone();
        let cancel: super::fs_quirks::Racer = Box::new(move || cancel.cancel());
        let quirks = if has_trash {
            Quirks {
                before: vec![(Op::Trash, "first.txt", 0, cancel)],
                ..Quirks::default()
            }
        } else {
            // The first check is Trash's, the second the permanent delete's.
            without_trash(Quirks {
                after: vec![(Op::Stat, "first.txt", 1, cancel)],
                ..Quirks::default()
            })
        };
        let _scope = FsQuirks::install(root.path(), quirks);
        let events = Rc::new(RefCell::new(Vec::new()));
        let emitted = events.clone();

        glib::MainContext::default().block_on(run_merge_undo(
            OperationRequestId(1615),
            created.iter().map(Location::local).collect(),
            Vec::new(),
            Rc::new(move |event| emitted.borrow_mut().push(event)),
            cancellable,
            staged_lookup(HashMap::new()),
        ));

        let events = events.borrow();
        let Some(OperationEvent::Cancelled { result, .. }) = events.last() else {
            panic!("Trash {has_trash}: expected a cancel, got {events:?}");
        };
        let [first, second] = &created;
        assert!(result.completed.is_empty(), "Trash {has_trash}");
        if has_trash {
            assert!(result.failed.is_empty());
            assert_eq!(
                result.not_attempted,
                [Location::local(first), Location::local(second)]
            );
            assert_eq!(fs::read(first)?, b"merged");
        } else {
            assert!(!first.exists(), "the cancel landed before the delete");
            assert_eq!(result.failed, [Location::local(first)]);
            assert_eq!(result.not_attempted, [Location::local(second)]);
        }
        assert_eq!(fs::read(second)?, b"merged", "Trash {has_trash}");
    }
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
        vec![
            Location::local(&first),
            unreadable.clone(),
            Location::local(&second),
        ],
        Vec::new(),
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
