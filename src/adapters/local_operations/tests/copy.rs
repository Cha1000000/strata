// SPDX-License-Identifier: MIT

use rustix::io::Errno;

use super::super::{
    StageContent, StagedSibling, copy_to_file_stage_blocking, copy_to_new_stage,
    discard_failed_file_stage, publish_staged_without_replace, stage_copier,
};
use super::fs_quirks::{Op, Racer, Support};
use super::*;

#[test]
fn nested_copy_shares_one_worker_budget_across_subdirectories() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    for folder in 0..9 {
        let directory = source.join(folder.to_string());
        fs::create_dir_all(&directory)?;
        for file in 0..9 {
            fs::write(directory.join(file.to_string()), format!("{folder}/{file}"))?;
        }
    }
    COPY_ACTIVITY.with(|activity| activity.set((0, 0)));
    glib::MainContext::default().block_on(super::super::copy_recursively_local(
        Arc::new(open_local_parent_directory(root.path())?),
        OsString::from("source"),
        gio::File::for_path(&target),
        super::super::CopyOptions {
            overwrite_existing: false,
            fat_family: false,
            workers: 2,
        },
        gio::Cancellable::new(),
        None,
        None,
    ))?;
    let (active, peak) = COPY_ACTIVITY.with(Cell::get);
    assert_eq!(active, 0, "all file copies must settle before completion");
    assert!(
        peak <= 2,
        "nested directories exceeded the copy budget: {peak}"
    );
    for folder in 0..9 {
        for file in 0..9 {
            assert_eq!(
                fs::read_to_string(target.join(format!("{folder}/{file}")))?,
                format!("{folder}/{file}")
            );
        }
    }
    Ok(())
}

#[test]
fn recursive_copy_preserves_nested_directory_contents() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-transfer-test-{unique}"));
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir_all(source.join("nested"))?;
    fs::write(source.join("top.txt"), b"top")?;
    fs::write(source.join("nested/child.txt"), b"child")?;

    let result = glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok());
    assert_eq!(fs::read(target.join("top.txt"))?, b"top");
    assert_eq!(fs::read(target.join("nested/child.txt"))?, b"child");

    fs::write(source.join("top.txt"), b"replacement")?;
    let overwrite = glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        true,
        gio::Cancellable::new(),
        None,
    ));
    assert!(overwrite.is_ok());
    assert_eq!(fs::read(target.join("top.txt"))?, b"replacement");

    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn copy_recursively_does_not_follow_a_symlink_nested_inside_the_tree() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-copy-symlink-test-{unique}"));
    let source = root.join("source");
    let outside = root.join("outside");
    let target = root.join("target");
    fs::create_dir_all(source.join("nested"))?;
    fs::create_dir_all(&outside)?;
    fs::write(outside.join("secret.txt"), b"do not copy me")?;
    fs::write(source.join("nested/visible.txt"), b"contents")?;
    std::os::unix::fs::symlink(&outside, source.join("nested/decoy"))?;

    let result = glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok());
    assert_eq!(fs::read(target.join("nested/visible.txt"))?, b"contents");
    let decoy_dest = target.join("nested/decoy");
    let decoy_metadata = fs::symlink_metadata(&decoy_dest)?;
    assert!(
        decoy_metadata.file_type().is_symlink(),
        "the decoy must be copied as a symlink, not followed into a real directory"
    );
    assert_eq!(fs::read_link(&decoy_dest)?, outside);
    // `is_symlink` above already rules out a real directory of copied
    // content existing under this name; confirm the thing it still points
    // at (unavoidably reachable by following the recreated symlink, same as
    // the original) was left untouched rather than overwritten.
    assert_eq!(fs::read(outside.join("secret.txt"))?, b"do not copy me");

    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn copy_recursively_of_a_symlink_creates_a_symlink_not_a_recursive_copy()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-copy-symlink-top-test-{unique}"));
    let outside = root.join("outside");
    let decoy = root.join("decoy");
    let target = root.join("target-link");
    fs::create_dir_all(&outside)?;
    fs::write(outside.join("secret.txt"), b"do not copy me")?;
    std::os::unix::fs::symlink(&outside, &decoy)?;

    let result = glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(&decoy),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok());
    let target_metadata = fs::symlink_metadata(&target)?;
    assert!(
        target_metadata.file_type().is_symlink(),
        "copying a symlink must produce a symlink, not a recursive copy of its target"
    );
    assert_eq!(fs::read_link(&target)?, outside);
    assert_eq!(fs::read(outside.join("secret.txt"))?, b"do not copy me");

    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn copy_accepts_a_symlink_higher_in_the_sources_parent_path() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let actual_root = root.path().join("actual");
    let linked_root = root.path().join("linked");
    fs::create_dir_all(actual_root.join("subdir"))?;
    fs::write(actual_root.join("subdir/source.txt"), b"keep")?;
    std::os::unix::fs::symlink(&actual_root, &linked_root)?;
    let target = root.path().join("target.txt");

    let result = glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(linked_root.join("subdir/source.txt")),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target)?, b"keep");
    assert_eq!(fs::read(actual_root.join("subdir/source.txt"))?, b"keep");
    Ok(())
}

#[test]
fn cancelling_staged_remote_file_copy_removes_only_the_incomplete_stage()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.bin");
    let target = root.path().join("target.bin");
    fs::write(&source, b"source contents")?;

    let result = glib::MainContext::default().block_on(copy_new_remote_file_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        gio::Cancellable::new(),
        Rc::new(|_, stage, _| {
            Box::pin(async move {
                fs::write(stage.path().expect("native stage"), b"partial")
                    .map_err(super::io_error)?;
                Err(glib::Error::new(
                    gio::IOErrorEnum::Cancelled,
                    "injected cancellation",
                ))
            })
        }),
        Rc::new(|_, _, _| Box::pin(async { panic!("cancelled copy must not commit") })),
    ));

    assert!(result.is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)));
    assert!(!target.exists());
    assert_eq!(fs::read(&source)?, b"source contents");
    assert_eq!(fs::read_dir(root.path())?.count(), 1);
    Ok(())
}

#[test]
fn staged_remote_file_copy_preserves_a_racing_destination() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.bin");
    let target = root.path().join("target.bin");
    fs::write(&source, b"source contents")?;

    let result = glib::MainContext::default().block_on(copy_new_remote_file_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        gio::Cancellable::new(),
        Rc::new(|source, stage, _| {
            Box::pin(async move {
                fs::copy(
                    source.path().expect("native source"),
                    stage.path().expect("native stage"),
                )
                .map(|_| ())
                .map_err(super::io_error)
            })
        }),
        Rc::new(|_, target, _| {
            Box::pin(async move {
                fs::write(target.path().expect("native target"), b"racing contents")
                    .map_err(super::io_error)?;
                Err(glib::Error::new(
                    gio::IOErrorEnum::Exists,
                    "injected destination race",
                ))
            })
        }),
    ));

    assert!(result.is_err_and(|error| error.matches(gio::IOErrorEnum::Exists)));
    assert_eq!(fs::read(&target)?, b"racing contents");
    assert_eq!(fs::read(&source)?, b"source contents");
    assert_eq!(fs::read_dir(root.path())?.count(), 2);
    Ok(())
}

#[test]
fn failed_incomplete_copy_cleanup_is_reported_as_a_failure() {
    let error = copy_failure_after_cleanup(
        glib::Error::new(gio::IOErrorEnum::Cancelled, "injected cancellation"),
        Err(glib::Error::new(
            gio::IOErrorEnum::Failed,
            "injected cleanup failure",
        )),
    );

    assert!(!error.matches(gio::IOErrorEnum::Cancelled));
    assert!(
        error
            .to_string()
            .contains("incomplete copy could not be removed")
    );
    assert!(error.to_string().contains("injected cleanup failure"));
}

#[test]
fn cancelling_recursive_copy_removes_only_its_staging_output() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-copy-cancel-test-{unique}"));
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir_all(source.join("nested"))?;
    fs::write(source.join("nested/item.txt"), b"contents")?;
    fs::write(root.join("pre-existing.txt"), b"keep")?;

    let cancellable = gio::Cancellable::new();
    let task = glib::MainContext::default().spawn_local(copy_new_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        cancellable.clone(),
    ));
    let context = glib::MainContext::default();
    loop {
        context.iteration(true);
        if fs::read_dir(&root)?.any(|entry| {
            entry.is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with(".strata-"))
        }) {
            break;
        }
    }
    cancellable.cancel();
    let result = context.block_on(task)?;
    settle_cancelled_io(&context);

    assert!(result.is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)));
    assert!(!target.exists());
    assert_eq!(fs::read(root.join("pre-existing.txt"))?, b"keep");
    assert_eq!(fs::read_dir(&root)?.count(), 2);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn cancelling_a_new_file_copy_removes_the_incomplete_stage() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.bin");
    let target = root.path().join("target.bin");
    fs::File::create(&source)?.set_len(64 * 1024 * 1024)?;
    fs::write(root.path().join("keep.txt"), b"keep")?;
    let cancellable = gio::Cancellable::new();
    let cancel_on_write = cancellable.clone();
    let progress = TransferProgressTracker::new(
        OperationRequestId(88),
        Some(64 * 1024 * 1024),
        Some(1),
        Rc::new(move |event| {
            if matches!(
                event,
                OperationEvent::TransferProgress {
                    transferred_bytes: 1..,
                    ..
                }
            ) {
                cancel_on_write.cancel();
            }
        }),
    );
    let result = glib::MainContext::default().block_on(copy_new_recursively_with_progress(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        cancellable,
        Some(progress),
    ));
    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)),
        "{result:?}"
    );
    assert!(!target.exists());
    assert_eq!(fs::read_dir(root.path())?.count(), 2);
    assert_eq!(fs::read(root.path().join("keep.txt"))?, b"keep");
    Ok(())
}

#[test]
fn a_file_stage_is_created_by_the_copy_at_a_free_name() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let attempts = RefCell::new(Vec::new());

    let staged = copy_to_file_stage_blocking(&root.path().join("target"), |stage| {
        let path = stage.path().ok_or_else(|| io_error("missing stage path"))?;
        attempts.borrow_mut().push(path.clone());
        if attempts.borrow().len() == 1 {
            fs::write(&path, b"theirs").map_err(gio_error)?;
            return Err(glib::Error::new(gio::IOErrorEnum::Exists, "taken"));
        }
        fs::File::create_new(&path)
            .and_then(|mut file| io::Write::write_all(&mut file, b"copied"))
            .map_err(gio_error)
    })
    .map_err(|(error, _)| error)?;

    let attempts = attempts.into_inner();
    assert_eq!(attempts.len(), 2);
    assert_eq!(fs::read(&attempts[0])?, b"theirs");
    assert_eq!(staged.path(), attempts[1]);
    assert_eq!(fs::read(staged.path())?, b"copied");
    Ok(())
}

#[test]
fn a_failed_file_stage_copy_removes_only_its_own_partial_file() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (partial, kept) in [("file", 0), ("hard-linked file", 2), ("folder", 1)] {
        let root = tempfile::tempdir()?;

        let failure = copy_to_file_stage_blocking(&root.path().join("target"), |stage| {
            let path = stage.path().ok_or_else(|| io_error("missing stage path"))?;
            if partial == "folder" {
                fs::create_dir(&path).map_err(gio_error)?;
            } else {
                fs::write(&path, b"partial").map_err(gio_error)?;
            }
            if partial == "hard-linked file" {
                fs::hard_link(&path, root.path().join("other")).map_err(gio_error)?;
            }
            Err(glib::Error::new(gio::IOErrorEnum::Cancelled, "cancelled"))
        })
        .err()
        .ok_or("the copy failed")?;
        let error = glib::MainContext::default().block_on(discard_failed_file_stage(failure));

        assert_eq!(fs::read_dir(root.path())?.count(), kept, "{partial}");
        if partial == "file" {
            assert!(error.matches(gio::IOErrorEnum::Cancelled), "{error}");
        } else {
            assert!(error.message().contains("was retained"), "{error}");
        }
    }
    Ok(())
}

#[test]
fn a_file_stage_turned_folder_is_kept_when_a_name_inside_it_is_taken() -> Result<(), Box<dyn Error>>
{
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    fs::create_dir(&source)?;
    std::os::unix::fs::symlink("elsewhere", source.join("link"))?;
    let stage_root = root.path().to_path_buf();
    let racer: Racer = Box::new(move || {
        fs::write(only_stage(&stage_root).join("link"), b"theirs").expect("take the name")
    });
    let _scope = FsQuirks::install(
        root.path(),
        Quirks {
            before: vec![(Op::Symlink, "link", 0, racer)],
            ..Quirks::default()
        },
    );
    let (copy, cancellable) = (stage_copier(None, false), gio::Cancellable::new());
    let source_file = gio::File::for_path(&source);

    // As if the source was still a file when its type was read.
    let error = glib::MainContext::default()
        .block_on(copy_to_new_stage(
            &source_file,
            gio::FileType::Regular,
            &root.path().join("target"),
            &cancellable,
            &|stage, kind| copy(source_file.clone(), stage, kind, cancellable.clone()),
        ))
        .err()
        .ok_or("the copy failed")?;

    assert!(error.message().contains("was retained"), "{error}");
    assert_eq!(names(root.path())?.len(), 2);
    assert_eq!(fs::read(only_stage(root.path()).join("link"))?, b"theirs");
    Ok(())
}

#[test]
fn copying_symlinks_accepts_an_aliased_destination() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let actual = root.path().join("actual");
    let alias = root.path().join("alias");
    let source = root.path().join("source");
    fs::create_dir(&actual)?;
    fs::write(root.path().join("sentinel.txt"), b"keep")?;
    std::os::unix::fs::symlink("actual", &alias)?;
    std::os::unix::fs::symlink("../sentinel.txt", &source)?;
    let context = glib::MainContext::default();

    context.block_on(copy_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(alias.join("link")),
        false,
        gio::Cancellable::new(),
        None,
    ))?;
    assert_eq!(
        fs::read_link(actual.join("link"))?,
        Path::new("../sentinel.txt")
    );
    context.block_on(copy_new_recursively(
        gio::File::for_path(root.path().join("sentinel.txt")),
        gio::File::for_path(alias.join("new.txt")),
        gio::Cancellable::new(),
    ))?;
    assert_eq!(fs::read(actual.join("new.txt"))?, b"keep");
    assert_eq!(fs::read(root.path().join("sentinel.txt"))?, b"keep");
    Ok(())
}

#[test]
fn copying_a_tree_with_a_named_pipe_fails_instead_of_blocking() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(&source)?;
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        source.join("00-pipe"),
        rustix::fs::Mode::from_bits_truncate(0o600),
    )?;
    for index in 1..16 {
        fs::write(
            source.join(format!("{index:02}-file.txt")),
            index.to_string(),
        )?;
    }

    let result = glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    let error = result.expect_err("a named pipe cannot be copied as a regular file");
    assert!(
        error.to_string().contains("pipe"),
        "the error should name the entry: {error}"
    );
    assert!(!target.join("00-pipe").exists());
    let entries_after_error = fs::read_dir(&target)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<HashSet<_>, _>>()?;
    glib::MainContext::default().block_on(glib::timeout_future(Duration::from_millis(20)));
    let entries_after_settling = fs::read_dir(&target)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<HashSet<_>, _>>()?;
    assert_eq!(entries_after_settling, entries_after_error);
    fs::remove_dir_all(&target)?;
    let staged = glib::MainContext::default().block_on(copy_new_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        gio::Cancellable::new(),
    ));
    assert!(staged.is_err());
    assert!(
        !target.exists(),
        "failed new copies must not expose partial trees"
    );
    assert_eq!(
        fs::read_dir(root.path())?.count(),
        1,
        "failed stages are removed"
    );
    Ok(())
}

#[test]
fn fat_sanitized_name_replaces_invalid_characters_and_trims_trailing_dots_and_spaces() {
    assert_eq!(
        fat_sanitized_name(OsStr::new("sc_macroorganizer?tabid:short=1.png")),
        OsStr::new("sc_macroorganizer_tabid_short=1.png")
    );
    assert_eq!(
        fat_sanitized_name(OsStr::new(r#"a"*/:<>?\|b"#)),
        OsStr::new("a_________b")
    );
    assert_eq!(
        fat_sanitized_name(OsStr::new("trailing dots.. ")),
        OsStr::new("trailing dots")
    );
    assert_eq!(fat_sanitized_name(OsStr::new("...")), OsStr::new("_"));
    assert_eq!(
        fat_sanitized_name(OsStr::new("plain.txt")),
        OsStr::new("plain.txt")
    );
}

#[test]
fn unique_fat_sibling_name_numbers_a_collision_instead_of_overwriting() {
    let mut used = HashSet::new();
    let first = unique_fat_sibling_name(OsString::from("a_b.txt"), &mut used);
    let second = unique_fat_sibling_name(OsString::from("a_b.txt"), &mut used);
    let third = unique_fat_sibling_name(OsString::from("a_b.txt"), &mut used);
    assert_eq!(first, OsString::from("a_b.txt"));
    assert_eq!(second, OsString::from("a_b (1).txt"));
    assert_eq!(third, OsString::from("a_b (2).txt"));
    assert_eq!(
        unique_fat_sibling_name(OsString::from("A_B.txt"), &mut used),
        OsString::from("A_B (3).txt")
    );
    let maximum = OsString::from("a (18446744073709551615).txt");
    assert_eq!(unique_fat_sibling_name(maximum.clone(), &mut used), maximum);
    let almost_maximum = OsString::from("a (18446744073709551614).txt");
    assert_eq!(
        unique_fat_sibling_name(almost_maximum.clone(), &mut used),
        almost_maximum
    );
    assert_eq!(
        unique_fat_sibling_name(almost_maximum, &mut used),
        OsString::from("a (1).txt")
    );
}

#[test]
fn target_is_fat_family_reads_the_reported_filesystem_type() {
    let mount_point = std::env::temp_dir().join("strata-fat-family-mount-probe");
    let mounts = MountTable::parse(format!(
        "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n22 1 8:2 / {} rw - exfat /dev/sdb1 rw\n",
        mount_point.display()
    ));
    assert!(target_is_fat_family(
        &gio::File::for_path(mount_point.join("photo.jpg")),
        &mounts
    ));
    assert!(!target_is_fat_family(
        &gio::File::for_path("/some/other/path"),
        &mounts
    ));
    assert!(!target_is_fat_family(
        &gio::File::for_uri("sftp://example.com/remote"),
        &mounts
    ));
}

#[test]
fn fat_family_copy_sanitizes_an_invalid_name_instead_of_discarding_the_whole_tree()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("nested"))?;
    fs::write(source.join("ok.txt"), b"fine")?;
    fs::write(
        source.join("nested/sc_macroorganizer?tabid:short=1.png"),
        b"cached icon",
    )?;

    let result = glib::MainContext::default().block_on(copy_recursively_fat_family(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("ok.txt"))?, b"fine");
    assert_eq!(
        fs::read(target.join("nested/sc_macroorganizer_tabid_short=1.png"))?,
        b"cached icon"
    );
    assert!(
        !target
            .join("nested/sc_macroorganizer?tabid:short=1.png")
            .exists()
    );
    Ok(())
}

#[test]
fn fat_family_copy_disambiguates_sibling_names_that_collide_after_sanitizing()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(&source)?;
    fs::write(source.join("a?.txt"), b"first")?;
    fs::write(source.join("a:.txt"), b"second")?;
    fs::write(source.join("A*.txt"), b"third")?;

    let result = glib::MainContext::default().block_on(copy_recursively_fat_family(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok(), "{result:?}");
    let mut contents = [
        fs::read(target.join("A_.txt"))?,
        fs::read(target.join("a_ (1).txt"))?,
        fs::read(target.join("a_ (2).txt"))?,
    ];
    contents.sort();
    assert_eq!(
        contents,
        [b"first".to_vec(), b"second".to_vec(), b"third".to_vec()]
    );
    Ok(())
}

#[test]
fn non_fat_copy_leaves_invalid_characters_untouched() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(&source)?;
    fs::write(source.join("a?b:c.txt"), b"unchanged")?;

    let result = glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("a?b:c.txt"))?, b"unchanged");
    Ok(())
}

fn stage(root: &Path, directory: bool) -> Result<StagedSibling, Box<dyn Error>> {
    Ok(if directory {
        StagedSibling::create(root, StageContent::Directory)?
    } else {
        file_stage(root)?
    })
}

fn publish(staged: StagedSibling, target: &Path) -> Result<(), glib::Error> {
    glib::MainContext::default().block_on(publish_staged_without_replace(
        staged,
        target.to_path_buf(),
        gio::Cancellable::new(),
    ))
}

#[test]
fn staged_publication_without_rename_flags_places_the_stage_itself() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::MetadataExt;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for error in [Errno::INVAL, Errno::NOSYS, Errno::OPNOTSUPP] {
        for directory in [false, true] {
            let root = tempfile::tempdir()?;
            let _scope = FsQuirks::install(
                root.path(),
                Quirks {
                    rename_flags: Support::Rejected(error),
                    ..Quirks::default()
                },
            );
            let staged = stage(root.path(), directory)?;
            let staged_path = staged.path();
            let target = root.path().join("target");
            if directory {
                fs::create_dir_all(staged_path.join("nested"))?;
                fs::write(staged_path.join("nested/file.txt"), b"directory data")?;
            } else {
                fs::write(&staged_path, b"file data")?;
            }
            let inode = fs::metadata(&staged_path)?.ino();

            publish(staged, &target)?;

            assert!(!staged_path.exists(), "{error:?}");
            assert_eq!(fs::metadata(&target)?.ino(), inode, "{error:?}");
            if directory {
                assert_eq!(fs::read(target.join("nested/file.txt"))?, b"directory data");
            } else {
                assert_eq!(fs::read(&target)?, b"file data");
            }
        }
    }
    Ok(())
}

#[test]
fn staged_publication_refuses_to_overwrite_an_existing_target() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for quirks in [
        Quirks::default,
        Quirks::without_rename_flags,
        Quirks::without_placement,
    ] {
        for directory in [false, true] {
            let root = tempfile::tempdir()?;
            let _scope = FsQuirks::install(root.path(), quirks());
            let staged = stage(root.path(), directory)?;
            let staged_path = staged.path();
            let target = root.path().join("target");
            if directory {
                fs::create_dir(&target)?;
                fs::write(target.join("existing.txt"), b"target data")?;
            } else {
                fs::write(&staged_path, b"new data")?;
                fs::write(&target, b"target data")?;
            }

            let error = publish(staged, &target).expect_err("must refuse an existing target");

            assert!(error.matches(gio::IOErrorEnum::Exists), "{error}");
            assert!(
                !staged_path.exists(),
                "failed publication must clean its stage"
            );
            let kept = if directory {
                target.join("existing.txt")
            } else {
                target.clone()
            };
            assert_eq!(fs::read(kept)?, b"target data");
        }
    }
    Ok(())
}

#[test]
fn staged_publication_preserves_racing_destinations() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for source_directory in [false, true] {
        for destination_kind in ["file", "directory", "symlink"] {
            let root = tempfile::tempdir()?;
            let target = root.path().join("target");
            let raced = target.clone();
            let racer: Racer = Box::new(move || match destination_kind {
                "file" => fs::write(&raced, b"racing data").expect("create competing file"),
                "directory" => fs::create_dir(&raced).expect("create competing directory"),
                _ => std::os::unix::fs::symlink("protected", &raced)
                    .expect("create competing symlink"),
            });
            // The race lands after the first rung, in front of the fallback rung.
            let fallback = if source_directory {
                Op::Mkdir
            } else {
                Op::Link
            };
            let _scope = FsQuirks::install(
                root.path(),
                Quirks {
                    rename_flags: Support::Rejected(Errno::OPNOTSUPP),
                    before: vec![(fallback, "target", 0, racer)],
                    ..Quirks::default()
                },
            );
            let staged = stage(root.path(), source_directory)?;
            let stage_path = staged.path();
            if !source_directory {
                fs::write(&stage_path, b"new data")?;
            }
            fs::write(root.path().join("protected"), b"protected data")?;

            assert!(publish(staged, &target).is_err());

            assert!(!stage_path.exists());
            match destination_kind {
                "file" => assert_eq!(fs::read(&target)?, b"racing data"),
                "directory" => assert_eq!(fs::read_dir(&target)?.count(), 0),
                _ => assert_eq!(fs::read_link(&target)?, Path::new("protected")),
            }
            assert_eq!(fs::read(root.path().join("protected"))?, b"protected data");
        }
    }
    Ok(())
}

#[test]
fn staged_symlinks_are_recreated_where_links_are_missing() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let _scope = FsQuirks::install(root.path(), Quirks::without_placement());
    let staged = StagedSibling::create(root.path(), StageContent::Symlink("missing".into()))?;
    let staged_path = staged.path();
    let target = root.path().join("target");

    publish(staged, &target)?;

    assert_eq!(fs::read_link(target)?, Path::new("missing"));
    assert!(fs::symlink_metadata(staged_path).is_err());
    Ok(())
}

#[test]
fn staged_fallback_removes_reserved_directory_on_copy_error() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let _scope = FsQuirks::install(root.path(), Quirks::without_placement());
    let staged = stage(root.path(), true)?;
    let staged_path = staged.path();
    let _socket = std::os::unix::net::UnixListener::bind(staged_path.join("unsupported.sock"))?;
    let target = root.path().join("target");

    assert!(publish(staged, &target).is_err());

    assert!(!target.exists());
    assert!(!staged_path.exists());
    Ok(())
}

fn only_stage(root: &Path) -> PathBuf {
    fs::read_dir(root)
        .expect("list the stage's folder")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".strata-replacement-"))
        })
        .expect("a stage")
}

/// A folder put at a file stage's name gets the stage's inode number back.
fn reusing_stage_inodes(before: (Op, &'static str, usize, Racer)) -> Quirks {
    Quirks {
        reused_inodes: Some(".strata-replacement-*"),
        no_birth_time: true,
        before: vec![before],
        ..Quirks::without_placement()
    }
}

#[test]
fn a_stage_left_after_the_exclusive_copy_is_reported() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let stage_root = root.path().to_path_buf();
    let racer: Racer = Box::new(move || {
        let stage = only_stage(&stage_root);
        fs::remove_file(&stage).expect("remove file stage");
        fs::create_dir(&stage).expect("replace stage with directory");
    });
    // One stat records the stage and two decide publication; the fourth is the discard.
    let _scope = FsQuirks::install(
        root.path(),
        reusing_stage_inodes((Op::Stat, ".strata-replacement-*", 3, racer)),
    );
    let staged = stage(root.path(), false)?;
    let staged_path = staged.path();
    fs::write(&staged_path, b"copied")?;
    let target = root.path().join("target");

    let error = publish(staged, &target).expect_err("the stage cannot be removed");

    assert!(
        error
            .message()
            .contains("staging copy could not be removed"),
        "{error}"
    );
    assert_eq!(fs::read(&target)?, b"copied");
    assert!(staged_path.is_dir());
    Ok(())
}

#[test]
fn a_stage_swapped_before_publication_is_neither_published_nor_deleted()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let stage_root = root.path().to_path_buf();
    let racer: Racer = Box::new(move || {
        let stage = only_stage(&stage_root);
        fs::remove_file(&stage).expect("remove file stage");
        fs::create_dir(&stage).expect("replace stage with directory");
        fs::write(stage.join("copied.txt"), b"foreign data").expect("populate");
    });
    let _scope = FsQuirks::install(
        root.path(),
        reusing_stage_inodes((Op::Rename, ".strata-replacement-*", 0, racer)),
    );
    let staged = stage(root.path(), false)?;
    let staged_path = staged.path();
    let target = root.path().join("target");

    let error = publish(staged, &target).expect_err("a swapped stage is refused");

    assert!(
        error
            .message()
            .contains("incomplete copy could not be removed"),
        "{error}"
    );
    assert!(fs::symlink_metadata(&target).is_err());
    assert_eq!(fs::read(staged_path.join("copied.txt"))?, b"foreign data");
    Ok(())
}

#[test]
fn cancelling_staged_fallback_removes_its_reserved_directory() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let _scope = FsQuirks::install(root.path(), Quirks::without_placement());
    let staged = stage(root.path(), true)?;
    let staged_path = staged.path();
    fs::File::create(staged_path.join("large.bin"))?.set_len(64 * 1024 * 1024)?;
    let target = root.path().join("target");
    let cancellable = gio::Cancellable::new();
    let finished = Rc::new(Cell::new(false));
    let completed = finished.clone();
    let publish_target = target.clone();
    let publish_cancellable = cancellable.clone();
    let context = glib::MainContext::default();
    let task = context.spawn_local(async move {
        let result =
            publish_staged_without_replace(staged, publish_target, publish_cancellable).await;
        completed.set(true);
        result
    });
    while !target.exists() && !finished.get() {
        context.iteration(true);
    }
    assert!(
        target.exists(),
        "fallback must reserve the destination before cancellation"
    );
    cancellable.cancel();
    let result = context.block_on(task)?;
    settle_cancelled_io(&context);
    assert!(result.is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)));
    assert!(!target.exists());
    assert!(!staged_path.exists());
    Ok(())
}

#[test]
fn a_symlink_child_never_overwrites_an_existing_entry() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir(&source)?;
    fs::create_dir(&target)?;
    std::os::unix::fs::symlink("elsewhere", source.join("Link"))?;
    fs::write(target.join("Link"), b"existing")?;

    let error = glib::MainContext::default()
        .block_on(copy_recursively(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            true,
            gio::Cancellable::new(),
            None,
        ))
        .expect_err("an existing entry blocks the link");

    assert!(error.matches(gio::IOErrorEnum::Exists), "{error}");
    assert_eq!(fs::read(target.join("Link"))?, b"existing");
    Ok(())
}
