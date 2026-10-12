// SPDX-License-Identifier: MIT

use std::os::unix::fs::MetadataExt;

use rustix::{fs::FsWord, io::Errno};

use super::super::{StageContent, StagedSibling, publish_staged_replacement, stage_copier};
use super::fs_quirks::{Exchange, Op, Trash};
use super::*;
use crate::{adapters::volume::mount_capabilities, services::ReplacedOriginal};

const NFS_SUPER_MAGIC: FsWord = 0x6969;
const FUSE_SUPER_MAGIC: FsWord = 0x6573_5546;
const TMPFS_MAGIC: FsWord = 0x0102_1994;

fn without_trash(fs_type: FsWord, quirks: Quirks) -> Quirks {
    Quirks {
        trash: Trash::Unsupported,
        fs_type: Some(fs_type),
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

fn inode(path: &Path) -> Result<u64, io::Error> {
    Ok(fs::symlink_metadata(path)?.ino())
}

fn used_name(quirks: &FsQuirks, prefix: &str) -> bool {
    quirks.calls().iter().any(|call| {
        call.names
            .iter()
            .any(|name| name.to_string_lossy().starts_with(prefix))
    })
}

/// Replaces `target` with `source`, returning what became of the original.
fn replace_reporting(
    source: &Path,
    target: &Path,
    move_source: bool,
) -> (Result<(), glib::Error>, Vec<ReplacedOriginal>) {
    let replaced = RefCell::new(Vec::new());
    let result = glib::MainContext::default().block_on(replace_local_with(
        gio::File::for_path(source),
        gio::File::for_path(target),
        move_source,
        gio::Cancellable::new(),
        None,
        stage_copier(None, false),
        &|original| replaced.borrow_mut().push(original),
    ));
    (result, replaced.into_inner())
}

fn write_item(path: &Path, directory: bool, contents: &[u8]) -> Result<(), io::Error> {
    if directory {
        fs::create_dir(path)?;
        fs::write(path.join("item"), contents)
    } else {
        fs::write(path, contents)
    }
}

fn read_item(path: &Path, directory: bool) -> Result<Vec<u8>, io::Error> {
    fs::read(if directory {
        path.join("item")
    } else {
        path.to_path_buf()
    })
}

#[test]
fn replacement_publication_preserves_concurrent_arrivals() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for directory in [false, true] {
        let root = tempfile::tempdir()?;
        let target = root.path().join("target");
        let staged = StagedSibling::create(
            root.path(),
            if directory {
                StageContent::Directory
            } else {
                StageContent::File
            },
        )?;
        let staged_path = staged.path().to_owned();
        if directory {
            fs::write(staged_path.join("incoming"), b"replacement")?;
            fs::create_dir(&target)?;
        } else {
            fs::write(&staged_path, b"replacement")?;
            fs::write(&target, b"new arrival")?;
        }
        let result = glib::MainContext::default()
            .block_on(publish_staged_replacement(staged, target.clone()));

        assert!(
            result.is_err(),
            "publication must not replace a concurrent arrival"
        );
        if directory {
            assert!(target.is_dir());
            assert_eq!(fs::read_dir(&target)?.count(), 0);
        } else {
            assert_eq!(fs::read(&target)?, b"new arrival");
        }
        assert!(!staged_path.exists());
    }
    Ok(())
}

#[test]
fn replacement_publication_reports_a_stage_it_cannot_remove() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let target = root.path().join("target");
    fs::write(&target, b"existing")?;
    let staged = StagedSibling::create(root.path(), StageContent::File)?;
    let staged_path = staged.path().to_owned();
    fs::remove_file(&staged_path)?;
    fs::create_dir(&staged_path)?;
    let error = glib::MainContext::default()
        .block_on(publish_staged_replacement(staged, target.clone()))
        .expect_err("concurrent destination prevents publication");
    assert!(
        error
            .message()
            .contains("incomplete copy could not be removed")
    );
    assert!(staged_path.is_dir());
    assert_eq!(fs::read(target)?, b"existing");
    fs::remove_dir(staged_path)?;
    Ok(())
}

#[test]
fn staged_file_replacement_preserves_the_destination_on_disk_full() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-replacement-failure-test-{unique}"));
    let source = root.join("source.txt");
    let target = root.join("target.txt");
    fs::create_dir_all(&root)?;
    fs::write(&source, b"replacement")?;
    fs::write(&target, b"original")?;

    let result = glib::MainContext::default().block_on(replace_local_with(
        gio::File::for_path(source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
        Rc::new(|_, staged, _, _| {
            Box::pin(async move {
                fs::write(
                    staged
                        .path()
                        .ok_or_else(|| super::io_error("missing stage"))?,
                    b"partial",
                )
                .map_err(super::io_error)?;
                Err(glib::Error::new(
                    gio::IOErrorEnum::NoSpace,
                    "injected disk-full failure",
                ))
            })
        }),
        &|_| {},
    ));

    assert!(result.is_err());
    assert_eq!(fs::read(&target)?, b"original");
    assert_eq!(fs::read_dir(&root)?.count(), 2);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn cancelling_staging_preserves_the_destination_and_cleans_the_partial_copy()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-replacement-cancel-test-{unique}"));
    let source = root.join("source.txt");
    let target = root.join("target.txt");
    fs::create_dir_all(&root)?;
    fs::write(&source, b"replacement")?;
    fs::write(&target, b"original")?;
    let staging = Rc::new(Cell::new(false));
    let staging_for_copy = staging.clone();
    let cancellable = gio::Cancellable::new();

    let task = glib::MainContext::default().spawn_local(replace_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        cancellable.clone(),
        None,
        Rc::new(move |_, staged, _, cancellable| {
            let staging = staging_for_copy.clone();
            Box::pin(async move {
                fs::write(
                    staged
                        .path()
                        .ok_or_else(|| super::io_error("missing stage"))?,
                    b"partial",
                )
                .map_err(super::io_error)?;
                staging.set(true);
                cancellable.future().await;
                Err(glib::Error::new(
                    gio::IOErrorEnum::Cancelled,
                    "injected cancellation",
                ))
            })
        }),
        &|_| {},
    ));
    let context = glib::MainContext::default();
    while !staging.get() {
        context.iteration(true);
    }
    cancellable.cancel();
    let result = context.block_on(task)?;
    settle_cancelled_io(&context);

    assert!(result.is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)));
    assert_eq!(fs::read(&target)?, b"original");
    assert_eq!(fs::read(&source)?, b"replacement");
    assert_eq!(fs::read_dir(&root)?.count(), 2);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn staged_file_replacement_commits_then_removes_a_moved_source() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-replacement-success-test-{unique}"));
    let source = root.join("source.txt");
    let target = root.join("target.txt");
    fs::create_dir_all(&root)?;
    fs::write(&source, b"replacement")?;
    fs::write(&target, b"original")?;

    let result = glib::MainContext::default().block_on(replace_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        true,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(&target)?, b"replacement");
    assert!(!source.exists());
    assert_eq!(fs::read_dir(&root)?.count(), 1);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn replacing_a_symlink_preserves_link_semantics() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source-link");
    let target = root.path().join("target-link");
    std::os::unix::fs::symlink("new-target", &source)?;
    std::os::unix::fs::symlink("old-target", &target)?;

    let result = glib::MainContext::default().block_on(replace_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read_link(&target)?, Path::new("new-target"));
    assert_eq!(fs::read_link(&source)?, Path::new("new-target"));
    Ok(())
}

#[test]
fn replacement_move_does_not_delete_a_substituted_source() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let destination = root.path().join("destination");
    fs::create_dir(&sources)?;
    fs::create_dir(&destination)?;
    let _volumes = separate_volumes(&sources, &destination);
    let source = sources.join("source.txt");
    let original_source = sources.join("original-source.txt");
    let target = destination.join("target.txt");
    fs::write(&source, b"replacement")?;
    fs::write(&target, b"original target")?;

    let replaced_source = source.clone();
    let new_source = source.clone();
    let preserved_source = original_source.clone();
    let result = glib::MainContext::default().block_on(replace_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        true,
        gio::Cancellable::new(),
        None,
        Rc::new(move |_, staged, _, _| {
            let replaced_source = replaced_source.clone();
            let new_source = new_source.clone();
            let preserved_source = preserved_source.clone();
            Box::pin(async move {
                fs::write(staged.path().unwrap_or_default(), b"replacement").map_err(io_error)?;
                fs::rename(replaced_source, preserved_source).map_err(io_error)?;
                fs::write(new_source, b"new arrival").map_err(io_error)
            })
        }),
        &|_| {},
    ));

    let error = result.expect_err("a substituted source must fail identity validation");
    assert!(error.to_string().contains("changed"), "{error}");
    assert_eq!(fs::read(target)?, b"replacement");
    assert_eq!(fs::read(source)?, b"new arrival");
    assert_eq!(fs::read(original_source)?, b"replacement");
    Ok(())
}

#[test]
fn replace_accepts_a_symlink_in_the_sources_parent_path() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let actual_parent = root.path().join("actual");
    let linked_parent = root.path().join("linked");
    fs::create_dir(&actual_parent)?;
    fs::write(actual_parent.join("source.txt"), b"new")?;
    std::os::unix::fs::symlink(&actual_parent, &linked_parent)?;
    let target = root.path().join("target.txt");
    fs::write(&target, b"old")?;

    let mut affected_locations = HashSet::new();
    let result = glib::MainContext::default().block_on(replace_local(
        gio::File::for_path(linked_parent.join("source.txt")),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        Some(&mut affected_locations),
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(&target)?, b"new");
    assert_eq!(fs::read(actual_parent.join("source.txt"))?, b"new");
    Ok(())
}

#[test]
fn replacement_stops_before_exchanging_a_substituted_target() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.txt");
    let target = root.path().join("target.txt");
    let original_target = root.path().join("original-target.txt");
    fs::write(&source, b"replacement")?;
    fs::write(&target, b"original target")?;
    let staged_path = Rc::new(RefCell::new(None));
    let recorded_staged_path = staged_path.clone();
    let replaced_target = target.clone();
    let new_target = target.clone();
    let preserved_target = original_target.clone();

    let result = glib::MainContext::default().block_on(replace_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
        Rc::new(move |_, staged, _, _| {
            let staged = staged.path().unwrap_or_default();
            *recorded_staged_path.borrow_mut() = Some(staged.clone());
            let replaced_target = replaced_target.clone();
            let new_target = new_target.clone();
            let preserved_target = preserved_target.clone();
            Box::pin(async move {
                fs::write(&staged, b"replacement").map_err(io_error)?;
                fs::rename(replaced_target, preserved_target).map_err(io_error)?;
                fs::write(new_target, b"new arrival").map_err(io_error)
            })
        }),
        &|_| {},
    ));

    let error = result.expect_err("a substituted target must fail identity validation");
    assert!(error.to_string().contains("changed"), "{error}");
    assert_eq!(fs::read(target)?, b"new arrival");
    assert_eq!(fs::read(original_target)?, b"original target");
    let staged_path = staged_path
        .borrow()
        .clone()
        .ok_or("the staging path was not recorded")?;
    assert!(!staged_path.exists());
    Ok(())
}

#[test]
fn cancelled_replacement_move_tracks_the_modified_source_and_target_roots()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-replacement-move-cancel-test-{unique}"));
    let source = root.join("sources/source");
    let target = root.join("destination/target");
    fs::create_dir_all(source.join("new"))?;
    fs::create_dir_all(target.join("old"))?;
    let _volumes = separate_volumes(&root.join("sources"), &root.join("destination"));
    fs::write(source.join("new/item.txt"), b"replacement")?;
    for index in 0..16 {
        fs::write(target.join(format!("old/item-{index}.txt")), b"old")?;
    }

    let cancellable = gio::Cancellable::new();
    let cancel_after_commit = cancellable.clone();
    let committed_marker = target.join("new/item.txt");
    let context = glib::MainContext::default();
    let watcher = context.spawn_local(async move {
        while !committed_marker.exists() {
            glib::timeout_future(Duration::ZERO).await;
        }
        cancel_after_commit.cancel();
    });
    let mut affected_locations = HashSet::new();
    let result = context.block_on(replace_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        true,
        cancellable,
        Some(&mut affected_locations),
    ));
    context.block_on(watcher)?;
    settle_cancelled_io(&context);

    assert!(result.is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)));
    assert!(affected_locations.contains(&Location::local(&source)));
    assert!(affected_locations.contains(&Location::local(&target)));
    assert_eq!(fs::read(target.join("new/item.txt"))?, b"replacement");
    assert!(source.exists());

    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn staged_directory_replacement_does_not_merge_old_contents() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-directory-replacement-test-{unique}"));
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir_all(source.join("new"))?;
    fs::create_dir_all(target.join("old"))?;
    fs::write(source.join("new/item.txt"), b"new")?;
    fs::write(target.join("old/item.txt"), b"old")?;

    let result = glib::MainContext::default().block_on(replace_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("new/item.txt"))?, b"new");
    assert!(!target.join("old").exists());
    assert!(source.exists());
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn replacing_a_directory_cleans_up_a_symlink_in_the_old_contents_without_following_it()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let root = std::env::temp_dir().join(format!("strata-replace-symlink-cleanup-test-{unique}"));
    let source = root.join("source");
    let target = root.join("target");
    let outside = root.join("outside");
    fs::create_dir_all(&source)?;
    fs::write(source.join("item.txt"), b"new")?;
    fs::create_dir_all(&target)?;
    fs::write(target.join("keep.txt"), b"old")?;
    fs::create_dir_all(&outside)?;
    fs::write(outside.join("secret.txt"), b"do not delete me")?;
    std::os::unix::fs::symlink(&outside, target.join("decoy"))?;

    let result = glib::MainContext::default().block_on(replace_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("item.txt"))?, b"new");
    assert!(
        !target.join("keep.txt").exists(),
        "the replaced directory's old contents must be gone"
    );
    assert!(
        !target.join("decoy").exists() && !target.join("decoy").is_symlink(),
        "the old decoy symlink itself must be gone from the replaced directory"
    );
    assert_eq!(
        fs::read(outside.join("secret.txt"))?,
        b"do not delete me",
        "cleaning up the old target must never follow a symlink it contained"
    );

    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn replace_without_trash_or_exchange_sets_the_original_aside() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for directory in [false, true] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        write_item(&source, directory, b"new")?;
        write_item(&target, directory, b"old")?;
        let quirks = FsQuirks::install(
            root.path(),
            without_trash(NFS_SUPER_MAGIC, Quirks::without_rename_flags()),
        );

        let (result, replaced) = replace_reporting(&source, &target, false);

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(replaced, [ReplacedOriginal::Deleted]);
        assert_eq!(read_item(&target, directory)?, b"new");
        assert_eq!(read_item(&source, directory)?, b"new");
        assert_eq!(names(root.path())?, ["source", "target"]);
        assert!(used_name(&quirks, ".strata-replaced-"));
        assert_eq!(quirks.count(Op::Exchange), 0);
    }
    Ok(())
}

#[test]
fn replace_falls_back_to_aside_when_trash_lies() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (trash, move_source) in [
        (Trash::FalseWithoutError, false),
        (Trash::SucceedsWithoutMoving, true),
    ] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::write(&source, b"new")?;
        fs::write(&target, b"old")?;
        let quirks = FsQuirks::install(
            root.path(),
            Quirks {
                trash,
                fs_type: Some(NFS_SUPER_MAGIC),
                ..Quirks::default()
            },
        );

        let (result, replaced) = replace_reporting(&source, &target, move_source);

        assert!(result.is_ok(), "{trash:?}: {result:?}");
        assert_eq!(quirks.count(Op::Trash), 1);
        assert_eq!(replaced, [ReplacedOriginal::Deleted]);
        assert_eq!(fs::read(&target)?, b"new");
        assert!(used_name(&quirks, ".strata-replaced-"));
        assert_eq!(
            names(root.path())?,
            if move_source {
                vec!["target"]
            } else {
                vec!["source", "target"]
            }
        );
    }
    Ok(())
}

#[test]
fn replace_never_uses_exchange_on_fuse() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"new")?;
    fs::write(&target, b"old")?;
    let quirks = FsQuirks::install(
        root.path(),
        without_trash(
            FUSE_SUPER_MAGIC,
            Quirks {
                exchange: Exchange::IgnoredOverwrites,
                ..Quirks::default()
            },
        ),
    );

    let (result, _) = replace_reporting(&source, &target, false);

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(quirks.count(Op::Exchange), 0);
    assert_eq!(fs::read(&target)?, b"new");
    assert_eq!(names(root.path())?, ["source", "target"]);
    Ok(())
}

#[test]
fn rejected_exchange_falls_back_to_aside_and_is_remembered() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"new")?;
    let quirks = FsQuirks::install(
        root.path(),
        without_trash(
            TMPFS_MAGIC,
            Quirks {
                exchange: Exchange::Rejected,
                ..Quirks::default()
            },
        ),
    );

    for exchanges in [1, 0] {
        fs::write(&target, b"old")?;
        quirks.clear_calls();

        let (result, replaced) = replace_reporting(&source, &target, false);

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(replaced, [ReplacedOriginal::Deleted]);
        assert_eq!(quirks.count(Op::Exchange), exchanges);
        assert_eq!(fs::read(&target)?, b"new");
        assert_eq!(names(root.path())?, ["source", "target"]);
    }
    assert_eq!(mount_capabilities(quirks.key()).exchange, Some(false));
    Ok(())
}

#[test]
fn a_failed_publication_puts_the_original_back() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (directory, quirks, op) in [
        (false, Quirks::default(), Op::Rename),
        (true, Quirks::default(), Op::Rename),
        (false, Quirks::without_rename_flags(), Op::Link),
    ] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        write_item(&source, directory, b"new")?;
        write_item(&target, directory, b"old")?;
        let original = inode(&target)?;
        // The first match sets the original aside; the second publishes.
        let quirks = FsQuirks::install(
            root.path(),
            without_trash(
                NFS_SUPER_MAGIC,
                Quirks {
                    fail: vec![(op, "target", 1, Errno::IO)],
                    ..quirks
                },
            ),
        );

        let (result, replaced) = replace_reporting(&source, &target, false);

        assert!(result.is_err(), "{directory} {op:?}");
        assert!(replaced.is_empty());
        assert!(used_name(&quirks, ".strata-replaced-"));
        assert_eq!(inode(&target)?, original);
        assert_eq!(read_item(&target, directory)?, b"old");
        assert_eq!(names(root.path())?, ["source", "target"]);
    }
    Ok(())
}

#[test]
fn a_taken_name_keeps_the_original_aside_and_names_it() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"new")?;
    fs::write(&target, b"old")?;
    let racer_target = target.clone();
    let racer: Box<dyn FnMut() + Send> = Box::new(move || {
        let _ = fs::write(&racer_target, b"racer");
    });
    let _quirks = FsQuirks::install(
        root.path(),
        without_trash(
            NFS_SUPER_MAGIC,
            Quirks {
                before: vec![(Op::Rename, "target", 1, racer)],
                ..Quirks::default()
            },
        ),
    );

    let (result, _) = replace_reporting(&source, &target, false);

    let error = result.expect_err("the racer keeps the name");
    let kept = names(root.path())?
        .into_iter()
        .find(|name| name.starts_with(".strata-replaced-"))
        .ok_or("the original was not kept")?;
    assert!(error.message().contains(&kept), "{error}");
    assert_eq!(fs::read(root.path().join(&kept))?, b"old");
    assert_eq!(fs::read(&target)?, b"racer");
    assert_eq!(names(root.path())?, [kept.as_str(), "source", "target"]);
    Ok(())
}

#[test]
fn replace_fails_before_staging_when_no_strategy_is_safe() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"new")?;
    fs::write(&target, b"old")?;
    let _quirks = FsQuirks::install(
        root.path(),
        without_trash(FUSE_SUPER_MAGIC, Quirks::without_placement()),
    );
    let copied = Rc::new(Cell::new(false));
    let copy_observed = copied.clone();

    let result = glib::MainContext::default().block_on(replace_local_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
        Rc::new(move |_, _, _, _| {
            copy_observed.set(true);
            Box::pin(async { Ok(()) })
        }),
        &|_| {},
    ));

    let error = result.expect_err("no safe way to replace");
    assert!(error.message().contains("replaced safely"), "{error}");
    assert!(!copied.get());
    assert_eq!(fs::read(&target)?, b"old");
    assert_eq!(names(root.path())?, ["source", "target"]);
    Ok(())
}

#[test]
fn the_original_is_deleted_only_after_the_removable_flush() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"new")?;
    fs::write(&target, b"old")?;
    let _guard = RemovableFlushGuard::install(root.path(), None);
    let aside_during_flush = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = aside_during_flush.clone();
    let watched = root.path().to_path_buf();
    set_sync_observer(Some(Arc::new(move |_| {
        let kept = names(&watched).is_ok_and(|names| {
            names
                .iter()
                .any(|name| name.starts_with(".strata-replaced-"))
        });
        record
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(kept);
    })));
    let _quirks = FsQuirks::install(
        root.path(),
        without_trash(NFS_SUPER_MAGIC, Quirks::default()),
    );

    let (result, _) = replace_reporting(&source, &target, false);

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        aside_during_flush
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .first(),
        Some(&true)
    );
    assert_eq!(fs::read(&target)?, b"new");
    assert_eq!(names(root.path())?, ["source", "target"]);
    Ok(())
}

#[test]
fn same_filesystem_replacing_move_places_the_source_without_a_stage() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for directory in [false, true] {
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let destination = root.path().join("destination");
        fs::create_dir(&sources)?;
        fs::create_dir(&destination)?;
        let source = sources.join("item");
        let target = destination.join("item");
        write_item(&source, directory, b"new")?;
        write_item(&target, directory, b"old")?;
        let moved = inode(&source)?;
        let quirks = FsQuirks::install(
            root.path(),
            without_trash(NFS_SUPER_MAGIC, Quirks::default()),
        );

        let (result, replaced) = replace_reporting(&source, &target, true);

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(replaced, [ReplacedOriginal::Deleted]);
        assert_eq!(inode(&target)?, moved);
        assert_eq!(read_item(&target, directory)?, b"new");
        assert!(!source.exists());
        assert_eq!(names(&destination)?, ["item"]);
        assert!(!used_name(&quirks, ".strata-replacement-"));
    }
    Ok(())
}

#[test]
fn same_filesystem_replacing_move_refuses_a_substituted_source() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let preserved = root.path().join("preserved");
    let target = root.path().join("target");
    fs::write(&source, b"incoming")?;
    fs::write(&target, b"old")?;
    let original = inode(&target)?;
    let (racer_source, racer_preserved) = (source.clone(), preserved.clone());
    let racer: Box<dyn FnMut() + Send> = Box::new(move || {
        let _ = fs::rename(&racer_source, &racer_preserved);
        let _ = fs::write(&racer_source, b"new arrival");
    });
    let _quirks = FsQuirks::install(
        root.path(),
        without_trash(
            NFS_SUPER_MAGIC,
            Quirks {
                before: vec![(Op::Rename, "source", 0, racer)],
                ..Quirks::without_rename_flags()
            },
        ),
    );

    let (result, replaced) = replace_reporting(&source, &target, true);

    let error = result.expect_err("a substituted source is refused");
    assert!(error.message().contains("changed"), "{error}");
    assert!(replaced.is_empty());
    assert_eq!(inode(&target)?, original);
    assert_eq!(fs::read(&target)?, b"old");
    assert_eq!(fs::read(&source)?, b"new arrival");
    assert_eq!(fs::read(&preserved)?, b"incoming");
    assert_eq!(names(root.path())?, ["preserved", "source", "target"]);
    Ok(())
}

#[test]
fn same_filesystem_replacing_move_restores_the_original_when_placement_fails()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"incoming")?;
    fs::write(&target, b"old")?;
    let original = inode(&target)?;
    let quirks = FsQuirks::install(
        root.path(),
        without_trash(
            NFS_SUPER_MAGIC,
            Quirks {
                fail: vec![(Op::Rename, "source", 0, Errno::IO)],
                ..Quirks::default()
            },
        ),
    );

    let (result, replaced) = replace_reporting(&source, &target, true);

    assert!(result.is_err());
    assert!(replaced.is_empty());
    assert!(used_name(&quirks, ".strata-replaced-"));
    assert_eq!(inode(&target)?, original);
    assert_eq!(fs::read(&target)?, b"old");
    assert_eq!(fs::read(&source)?, b"incoming");
    assert_eq!(names(root.path())?, ["source", "target"]);
    Ok(())
}

#[test]
fn a_true_alias_is_a_no_op_but_a_hard_link_elsewhere_is_replaced() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let photo = root.path().join("Photo.txt");
    let link = root.path().join("link.txt");
    fs::write(&photo, b"photo")?;
    fs::hard_link(&photo, &link)?;
    let original = inode(&photo)?;
    let _quirks = FsQuirks::install(
        root.path(),
        without_trash(
            NFS_SUPER_MAGIC,
            Quirks {
                case_insensitive: true,
                ..Quirks::default()
            },
        ),
    );

    let (aliased, aliased_replaced) =
        replace_reporting(&photo, &root.path().join("photo.txt"), false);
    let (linked, link_replaced) = replace_reporting(&photo, &link, false);

    assert!(aliased.is_ok(), "{aliased:?}");
    assert!(aliased_replaced.is_empty());
    assert!(linked.is_ok(), "{linked:?}");
    assert_eq!(link_replaced, [ReplacedOriginal::Deleted]);
    assert_eq!(inode(&photo)?, original);
    assert_eq!(fs::symlink_metadata(&photo)?.nlink(), 1);
    assert_ne!(inode(&link)?, original);
    assert_eq!(fs::read(&link)?, b"photo");
    assert_eq!(names(root.path())?, ["Photo.txt", "link.txt"]);
    Ok(())
}
