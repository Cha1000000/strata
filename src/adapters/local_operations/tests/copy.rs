// SPDX-License-Identifier: MIT

use rustix::io::Errno;

use super::super::{
    StageContent, StagedSibling,
    copy_engine::{Destination, Options, copy_local},
    publish_staged_without_replace,
};
use super::fs_quirks::{Op, Racer, Support};
use super::*;
use crate::adapters::volume::learn_mount_capability;

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
    *copy_activity() = (0, 0);
    glib::MainContext::default().block_on(copy_local(
        source,
        Destination::New(target.clone(), Taken::Fail),
        Options {
            workers: Some(2),
            ..Options::default()
        },
        None,
        &gio::Cancellable::new(),
    ))?;
    let (active, peak) = *copy_activity();
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
    ));

    assert!(result.is_ok());
    assert_eq!(fs::read(target.join("top.txt"))?, b"top");
    assert_eq!(fs::read(target.join("nested/child.txt"))?, b"child");

    fs::write(source.join("top.txt"), b"replacement")?;
    let merged = glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        true,
        gio::Cancellable::new(),
    ));
    assert!(merged.is_ok(), "{merged:?}");
    // A merge fills only names it moved out of the way, and numbers the rest.
    assert_eq!(fs::read(target.join("top.txt"))?, b"top");
    assert_eq!(fs::read(target.join("top (1).txt"))?, b"replacement");
    assert_eq!(fs::read(target.join("nested/child.txt"))?, b"child");
    assert_eq!(fs::read(target.join("nested/child (1).txt"))?, b"child");

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
        Taken::Fail,
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
        Taken::Fail,
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
fn a_remote_keep_both_copy_takes_the_next_name_when_one_is_taken_meanwhile()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.bin");
    let target = root.path().join("target.bin");
    fs::write(&source, b"source contents")?;
    fs::write(&target, b"existing")?;

    let copied = glib::MainContext::default().block_on(copy_new_remote_file_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        Taken::Number,
        gio::Cancellable::new(),
        Rc::new(|source, stage, _| {
            Box::pin(async move {
                fs::copy(
                    source.path().expect("native source"),
                    stage.path().expect("native stage"),
                )
                .map(drop)
                .map_err(super::io_error)
            })
        }),
        Rc::new(|stage, target, _| {
            Box::pin(async move {
                let target = target.path().expect("native target");
                if target.ends_with("target (1).bin") {
                    fs::write(&target, b"racing contents").map_err(super::io_error)?;
                    return Err(glib::Error::new(
                        gio::IOErrorEnum::Exists,
                        "injected destination race",
                    ));
                }
                fs::rename(stage.path().expect("native stage"), target).map_err(super::io_error)
            })
        }),
    ))?;

    assert_eq!(copied.path(), Some(root.path().join("target (2).bin")));
    assert_eq!(
        fs::read(root.path().join("target (2).bin"))?,
        b"source contents"
    );
    assert_eq!(
        fs::read(root.path().join("target (1).bin"))?,
        b"racing contents"
    );
    assert_eq!(fs::read(&target)?, b"existing");
    assert_eq!(
        names(root.path())?,
        [
            "source.bin",
            "target (1).bin",
            "target (2).bin",
            "target.bin"
        ]
    );
    Ok(())
}

#[test]
fn a_remote_keep_both_copy_numbers_a_long_name_within_the_limit() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.bin");
    let target = root.path().join(format!("{}.bin", "t".repeat(251)));
    let numbered = root.path().join(format!("{} (1).bin", "t".repeat(247)));
    fs::write(&source, b"source contents")?;
    fs::write(&target, b"existing")?;

    let copied = glib::MainContext::default().block_on(copy_new_remote_file_with(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        Taken::Number,
        gio::Cancellable::new(),
        Rc::new(|source, stage, _| {
            Box::pin(async move {
                fs::copy(
                    source.path().expect("native source"),
                    stage.path().expect("native stage"),
                )
                .map(drop)
                .map_err(super::io_error)
            })
        }),
        Rc::new(|stage, target, _| {
            Box::pin(async move {
                fs::rename(
                    stage.path().expect("native stage"),
                    target.path().expect("native target"),
                )
                .map_err(super::io_error)
            })
        }),
    ))?;

    assert_eq!(copied.path(), Some(numbered.clone()));
    assert_eq!(fs::read(numbered)?, b"source contents");
    assert_eq!(fs::read(&target)?, b"existing");
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
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("nested"))?;
    fs::write(source.join("nested/item.txt"), b"contents")?;
    fs::write(root.path().join("pre-existing.txt"), b"keep")?;
    let cancellable = gio::Cancellable::new();
    let cancel = cancellable.clone();
    let _scope = FsQuirks::install(
        root.path(),
        Quirks {
            before: vec![(
                Op::OpenSource,
                "item.txt",
                0,
                Box::new(move || cancel.cancel()),
            )],
            ..Quirks::default()
        },
    );

    let result = glib::MainContext::default().block_on(copy_new_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        cancellable,
    ));

    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)),
        "{result:?}"
    );
    assert_eq!(names(root.path())?, ["pre-existing.txt", "source"]);
    assert_eq!(fs::read(root.path().join("pre-existing.txt"))?, b"keep");
    Ok(())
}

#[test]
fn cancelling_a_new_file_copy_removes_the_incomplete_stage() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (case, preset) in [
        ("unnamed file", Quirks::default as Preset),
        ("named stage", || Quirks {
            tmpfile: Support::Rejected(Errno::OPNOTSUPP),
            ..Quirks::ext4()
        }),
    ] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source.bin");
        let target = root.path().join("target.bin");
        fs::write(&source, vec![0x5a; 4 * 1024 * 1024])?;
        fs::write(root.path().join("keep.txt"), b"keep")?;
        let cancellable = gio::Cancellable::new();
        let cancel =
            |cancellable: gio::Cancellable| -> Racer { Box::new(move || cancellable.cancel()) };
        let _scope = FsQuirks::install(
            root.path(),
            Quirks {
                before: vec![
                    (Op::CopyRange, "*", 0, cancel(cancellable.clone())),
                    (Op::Write, "*", 0, cancel(cancellable.clone())),
                ],
                reflink: Support::Rejected(Errno::OPNOTSUPP),
                ..preset()
            },
        );

        let result = glib::MainContext::default().block_on(copy_new_recursively(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            cancellable,
        ));

        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)),
            "{case}: {result:?}"
        );
        assert_eq!(names(root.path())?, ["keep.txt", "source.bin"], "{case}");
    }
    Ok(())
}

#[test]
fn a_stage_name_left_by_a_lost_reply_is_adopted_and_a_taken_one_is_not()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let is_stage = |name: &str| name.starts_with(".strata-copy-");
    for (op, folder) in [(Op::Create, false), (Op::Mkdir, true)] {
        for taken in [false, true] {
            let row = format!("{op:?}, taken: {taken}");
            let root = tempfile::tempdir()?;
            let (source, target) = (root.path().join("source"), root.path().join("target"));
            let copied = if folder {
                fs::create_dir(&source)?;
                source.join("inner.txt")
            } else {
                source.clone()
            };
            fs::write(&copied, b"copied")?;
            let parent = root.path().to_path_buf();
            let racer: Racer = Box::new(move || {
                let stage = names(&parent)
                    .expect("list")
                    .into_iter()
                    .find(|name| is_stage(name))
                    .map(|name| parent.join(name))
                    .expect("the stage");
                let theirs = if folder { stage.join("theirs") } else { stage };
                fs::write(theirs, b"theirs").expect("fill the stage");
            });
            let scope = FsQuirks::install(
                root.path(),
                Quirks {
                    tmpfile: Support::Rejected(Errno::OPNOTSUPP),
                    lost_reply: vec![(op, ".strata-copy-*", Errno::EXIST)],
                    before: if taken {
                        vec![(Op::Stat, ".strata-copy-*", 0, racer)]
                    } else {
                        Vec::new()
                    },
                    ..Quirks::ext4()
                },
            );

            glib::MainContext::default().block_on(copy_new_recursively(
                gio::File::for_path(&source),
                gio::File::for_path(&target),
                gio::Cancellable::new(),
            ))?;

            let stages = scope
                .calls()
                .into_iter()
                .filter(|call| call.op == op)
                .flat_map(|call| call.names)
                .filter(|name| is_stage(&name.to_string_lossy()))
                .collect::<HashSet<_>>();
            assert_eq!(stages.len(), if taken { 2 } else { 1 }, "{row}");
            let copy = if folder {
                target.join("inner.txt")
            } else {
                target
            };
            assert_eq!(fs::read(copy)?, b"copied", "{row}");
            let left = names(root.path())?
                .into_iter()
                .filter(|name| is_stage(name))
                .map(|name| root.path().join(name))
                .collect::<Vec<_>>();
            match left.as_slice() {
                [] => assert!(!taken, "{row}"),
                [theirs] => {
                    let theirs = if folder {
                        theirs.join("theirs")
                    } else {
                        theirs.clone()
                    };
                    assert!(taken, "{row}");
                    assert_eq!(fs::read(theirs)?, b"theirs", "{row}");
                }
                _ => panic!("{row}: {left:?}"),
            }
        }
    }
    Ok(())
}

#[test]
fn a_stage_replaced_during_the_copy_is_kept() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let (sources, destination) = (root.path().join("sources"), root.path().join("destination"));
    fs::create_dir(&sources)?;
    fs::create_dir(&destination)?;
    fs::write(sources.join("source.bin"), vec![1; 64 * 1024])?;
    let stage_folder = destination.clone();
    let racer: Racer = Box::new(move || {
        let stage = only_entry(&stage_folder, ".strata-copy-");
        fs::remove_file(&stage).expect("remove the stage");
        fs::write(&stage, b"theirs").expect("take the stage name");
    });
    let _source = FsQuirks::install(&sources, Quirks::default());
    let _destination = FsQuirks::install(
        &destination,
        Quirks {
            tmpfile: Support::Rejected(Errno::OPNOTSUPP),
            before: vec![(Op::Write, "*", 0, racer)],
            fail: vec![(Op::Write, "*", 0, Errno::IO)],
            ..Quirks::ext4()
        },
    );

    let error = glib::MainContext::default()
        .block_on(copy_new_recursively(
            gio::File::for_path(sources.join("source.bin")),
            gio::File::for_path(destination.join("target.bin")),
            gio::Cancellable::new(),
        ))
        .expect_err("the write fails");

    assert!(
        error
            .message()
            .contains("incomplete copy could not be removed"),
        "{error}"
    );
    assert_eq!(
        fs::read(only_entry(&destination, ".strata-copy-"))?,
        b"theirs"
    );
    assert_eq!(names(&destination)?.len(), 1);
    Ok(())
}

#[test]
fn a_folder_swapped_for_a_link_during_the_copy_is_not_followed() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let outside = root.path().join("outside");
    fs::create_dir_all(source.join("nested"))?;
    fs::write(source.join("nested/inside.txt"), b"inside")?;
    fs::create_dir(&outside)?;
    fs::write(outside.join("secret.txt"), b"do not copy me")?;
    let (nested, moved) = (source.join("nested"), source.join("moved"));
    let racer: Racer = Box::new(move || {
        fs::rename(&nested, &moved).expect("move the folder away");
        std::os::unix::fs::symlink("../outside", &nested).expect("put a link in its place");
    });
    let _scope = FsQuirks::install(
        root.path(),
        Quirks {
            before: vec![(Op::OpenDir, "nested", 0, racer)],
            ..Quirks::default()
        },
    );

    let error = glib::MainContext::default()
        .block_on(copy_new_recursively(
            gio::File::for_path(&source),
            gio::File::for_path(root.path().join("target")),
            gio::Cancellable::new(),
        ))
        .expect_err("the swapped folder is refused");

    assert!(error.message().contains("changed"), "{error}");
    assert_eq!(names(root.path())?, ["outside", "source"]);
    assert_eq!(names(&outside)?, ["secret.txt"]);
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

fn make_fifo(path: &Path, mode: u32) -> io::Result<()> {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(mode),
        0,
    )?;
    Ok(())
}

#[test]
fn special_files_inside_a_copied_folder_are_recreated_or_left_out_and_reported()
-> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(&source)?;
    fs::create_dir(&destination)?;
    make_fifo(&source.join("00-pipe"), 0o600)?;
    fs::set_permissions(source.join("00-pipe"), fs::Permissions::from_mode(0o640))?;
    let _socket = std::os::unix::net::UnixListener::bind(source.join("socket"))?;
    for index in 1..4 {
        fs::write(
            source.join(format!("{index:02}-file.txt")),
            index.to_string(),
        )?;
    }
    let modified = rustix::fs::Timespec {
        tv_sec: 1_000_000_000,
        tv_nsec: 0,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        source.join("00-pipe"),
        &rustix::fs::Timestamps {
            last_access: modified,
            last_modification: modified,
        },
        rustix::fs::AtFlags::empty(),
    )?;
    let _sources = FsQuirks::install(&source, Quirks::default());
    let _destination = FsQuirks::install(&destination, Quirks::default());
    let target = destination.join("copy");

    let report = copy_reporting(&source, &target, None)?;

    let pipe = fs::symlink_metadata(target.join("00-pipe"))?;
    assert!(pipe.file_type().is_fifo());
    assert_eq!(pipe.mode() & 0o7777, 0o640);
    assert_eq!(pipe.mtime(), 1_000_000_000);
    assert_eq!(
        report
            .skipped
            .into_iter()
            .map(|item| (item.location, item.kind))
            .collect::<Vec<_>>(),
        [(
            Location::local(target.join("socket")),
            crate::services::SpecialKind::Socket,
        )]
    );
    assert!(!target.join("socket").exists());
    for index in 1..4 {
        assert_eq!(
            fs::read_to_string(target.join(format!("{index:02}-file.txt")))?,
            index.to_string()
        );
    }
    Ok(())
}

#[test]
fn a_special_file_left_out_of_fat_is_reported_under_its_own_name() -> Result<(), Box<dyn Error>> {
    use crate::services::SpecialKind;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let stick = root.path().join("stick");
    fs::create_dir_all(&source)?;
    fs::create_dir(&stick)?;
    make_fifo(&source.join("p:ipe"), 0o600)?;
    let _socket = std::os::unix::net::UnixListener::bind(source.join("s:ock"))?;
    let _stick = FsQuirks::install(&stick, Quirks::vfat());
    let target = stick.join("copy");

    let (copied, report) = reporting(
        &source,
        super::super::copy_engine::Destination::New(target.clone(), Taken::Fail),
        super::super::copy_engine::Options {
            names: fixing(),
            ..super::super::copy_engine::Options::default()
        },
    );

    copied?;
    let mut skipped = report
        .skipped
        .into_iter()
        .map(|item| (item.location, item.kind))
        .collect::<Vec<_>>();
    skipped.sort_by(|left, right| left.0.compare(&right.0));
    assert_eq!(
        skipped,
        [
            (Location::local(target.join("p:ipe")), SpecialKind::Pipe),
            (Location::local(target.join("s:ock")), SpecialKind::Socket),
        ]
    );
    assert_eq!(report.renamed, []);
    assert_eq!(names(&target)?, Vec::<String>::new());
    Ok(())
}

#[test]
fn named_pipes_are_left_out_only_where_the_destination_refuses_them() -> Result<(), Box<dyn Error>>
{
    use rustix::io::Errno;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // How vfat, exfat-fuse, SMB and others refuse a pipe, and errors that
    // stop the copy instead.
    let rows = [
        (Errno::PERM, true),
        (Errno::IO, true),
        (Errno::OPNOTSUPP, true),
        (Errno::NOSYS, true),
        (Errno::INVAL, true),
        (Errno::NOSPC, false),
        (Errno::ACCESS, false),
    ];
    for (errno, refused) in rows {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir_all(&source)?;
        fs::create_dir(&destination)?;
        make_fifo(&source.join("pipe"), 0o600)?;
        fs::write(source.join("file.txt"), b"file")?;
        let _destination = FsQuirks::install(
            &destination,
            Quirks {
                fifos: fs_quirks::Support::Rejected(errno),
                ..Quirks::default()
            },
        );
        let target = destination.join("copy");

        let copied = copy_reporting(&source, &target, None);

        if refused {
            let report = copied.map_err(|error| format!("{errno:?}: {error}"))?;
            let skipped = report
                .skipped
                .into_iter()
                .map(|item| (item.location, item.kind))
                .collect::<Vec<_>>();
            assert_eq!(
                skipped,
                [(
                    Location::local(target.join("pipe")),
                    crate::services::SpecialKind::Pipe
                )],
                "{errno:?}"
            );
            assert_eq!(names(&target)?, ["file.txt"], "{errno:?}");
        } else {
            assert!(copied.is_err(), "{errno:?}");
            assert_eq!(names(&destination)?, Vec::<String>::new(), "{errno:?}");
        }
    }
    Ok(())
}

#[test]
fn a_named_pipe_whose_copy_fails_is_not_left_behind() -> Result<(), Box<dyn Error>> {
    use crate::adapters::volume::learn_mount_capability;
    use rustix::io::Errno;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // A selected pipe made at its final name, and a pipe in a folder made in
    // place, where the mount cannot place a stage.
    for selected in [true, false] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir_all(&source)?;
        fs::create_dir(&destination)?;
        make_fifo(&source.join("pipe"), 0o640)?;
        fs::write(source.join("a.txt"), b"a")?;
        let scope = FsQuirks::install(
            &destination,
            Quirks {
                fail: vec![(Op::SetTimes, "pipe", 0, Errno::IO)],
                ..Quirks::without_folder_placement()
            },
        );
        learn_mount_capability(scope.key(), |capabilities| {
            capabilities.no_replace = Some(false);
            capabilities.hard_links = Some(true);
            capabilities.rename_over_empty_directory = Some(false);
        });
        let (from, to) = if selected {
            (source.join("pipe"), destination.join("pipe"))
        } else {
            (source.clone(), destination.join("copy"))
        };

        let copied = glib::MainContext::default().block_on(copy_new_recursively(
            gio::File::for_path(from),
            gio::File::for_path(to),
            gio::Cancellable::new(),
        ));

        assert!(copied.is_err(), "selected: {selected}");
        assert_eq!(
            names(&destination)?,
            Vec::<String>::new(),
            "selected: {selected}"
        );
    }
    Ok(())
}

#[test]
fn a_selected_named_pipe_is_recreated_where_the_destination_can() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::FileTypeExt;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (row, preset, pipes) in [
        ("tmpfs", Quirks::default as Preset, true),
        ("vfat", Quirks::vfat, false),
    ] {
        let root = tempfile::tempdir()?;
        let destination = root.path().join("destination");
        fs::create_dir(&destination)?;
        let source = root.path().join("pipe");
        make_fifo(&source, 0o600)?;
        let _destination = FsQuirks::install(&destination, preset());
        let target = destination.join("pipe");

        let copied = glib::MainContext::default().block_on(copy_new_recursively(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            gio::Cancellable::new(),
        ));

        if pipes {
            copied?;
            assert!(
                fs::symlink_metadata(&target)?.file_type().is_fifo(),
                "{row}"
            );
        } else {
            let error = copied.expect_err("vfat has no named pipes");
            assert_eq!(
                error.message(),
                rust_i18n::t!(
                    "Cannot copy %{name}: it is not a regular file, directory, or symbolic link",
                    name = "pipe"
                ),
                "{row}"
            );
            assert_eq!(names(&destination)?, Vec::<String>::new(), "{row}");
        }
    }
    Ok(())
}

#[test]
fn a_paste_through_a_symbolic_link_takes_the_rules_of_the_mount_behind_it()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let stick = root.path().join("stick");
    let link = root.path().join("link");
    fs::create_dir_all(source.join("photos?"))?;
    fs::create_dir(&stick)?;
    std::os::unix::fs::symlink(&stick, &link)?;
    fs::write(source.join("a:b"), b"colon")?;
    fs::write(source.join("photos?/x*y.jpg"), b"star")?;
    fs::File::create(source.join("disk.img"))?.set_len(FAT32_MAX_FILE_SIZE + 1)?;
    let _stick = FsQuirks::install(&stick, Quirks::vfat());
    let paste = |names: &[&str]| PasteRequest {
        id: OperationRequestId(1530),
        destination: Location::local(&link),
        items: names
            .iter()
            .map(|name| PasteItem {
                source: Location::local(source.join(name)),
                conflict: TransferConflict::FailIfExists,
            })
            .collect(),
        move_sources: false,
    };

    run_paste(paste(&["a:b", "photos?"]));

    assert_eq!(names(&stick)?, ["a_b", "photos_"]);
    assert_eq!(fs::read(stick.join("a_b"))?, b"colon");
    assert_eq!(fs::read(stick.join("photos_/x_y.jpg"))?, b"star");
    let events = Rc::new(RefCell::new(Vec::new()));
    let emitted = events.clone();
    let _operation = LocalOperationProvider.paste(
        paste(&["disk.img"]),
        Rc::new(move |event| emitted.borrow_mut().push(event)),
    );
    drive_until_transfer_settles(&events);

    assert!(
        matches!(
            events.borrow().last(),
            Some(OperationEvent::TransferFailed { .. })
        ),
        "{:?}",
        events.borrow()
    );
    assert_eq!(names(&stick)?, ["a_b", "photos_"]);
    Ok(())
}

#[test]
fn a_paste_fixes_the_names_it_brings_onto_fat_and_keeps_those_already_there()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let (one, two, stick) = (
        root.path().join("one"),
        root.path().join("two"),
        root.path().join("stick"),
    );
    let earlier = stick.join("earlier");
    for folder in [&one, &two, &earlier.join("kept")] {
        fs::create_dir_all(folder)?;
    }
    for (path, contents) in [
        (one.join("a:b"), "colon"),
        (two.join("a_b"), "kept"),
        (one.join("x"), "lower"),
        (two.join("X"), "upper"),
        (earlier.join("CON"), "device"),
        (earlier.join("kept/CON"), "nested device"),
    ] {
        fs::write(path, contents)?;
    }
    // More than eight files are copied in parallel.
    let bulk = root.path().join("bulk");
    fs::create_dir_all(stick.join("bulk"))?;
    let mut bulk_items = vec![bulk.join("b?c")];
    bulk_items.extend((0..8).map(|index| bulk.join(format!("file {index}"))));
    fs::create_dir(&bulk)?;
    for item in &bulk_items {
        fs::write(item, "bulk")?;
    }
    let _stick = FsQuirks::install(&stick, Quirks::vfat());
    let paste = |destination: &Path, items: &[PathBuf]| PasteRequest {
        id: OperationRequestId(1590),
        destination: Location::local(destination),
        items: items
            .iter()
            .map(|source| PasteItem {
                source: Location::local(source),
                conflict: TransferConflict::FailIfExists,
            })
            .collect(),
        move_sources: false,
    };

    let events = run_paste(paste(
        &stick,
        &[
            one.join("a:b"),
            two.join("a_b"),
            one.join("x"),
            two.join("X"),
            earlier.join("CON"),
            earlier.join("kept"),
        ],
    ));
    let bulk_events = run_paste(paste(&stick.join("bulk"), &bulk_items));

    for (name, contents) in [
        ("a_b (1)", "colon"),
        ("a_b", "kept"),
        ("x (1)", "lower"),
        ("X", "upper"),
        ("CON", "device"),
        ("kept/CON", "nested device"),
        ("bulk/b_c", "bulk"),
    ] {
        assert_eq!(fs::read(stick.join(name))?, contents.as_bytes(), "{name}");
    }
    assert_eq!(names(&stick)?.len(), 8);
    let renamed = |folder: &Path, events: &[OperationEvent]| {
        renamed_beneath(folder, &followed_report(events))
    };
    use crate::services::RenameCause::{Alias, Invalid};
    assert_eq!(
        renamed(&stick, &events),
        [("a:b", "a_b (1)", Invalid), ("x", "x (1)", Alias)].map(|(name, saved_as, cause)| (
            name.to_owned(),
            saved_as.to_owned(),
            cause
        ))
    );
    assert_eq!(
        renamed(&stick.join("bulk"), &bulk_events),
        [("b?c".to_owned(), "b_c".to_owned(), Invalid)]
    );
    Ok(())
}

#[test]
fn a_fat_copy_fixes_an_invalid_name_instead_of_discarding_the_whole_tree()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let stick = root.path().join("stick");
    let target = stick.join("target");
    fs::create_dir_all(source.join("nested"))?;
    fs::create_dir(&stick)?;
    fs::write(source.join("ok.txt"), b"fine")?;
    fs::write(
        source.join("nested/sc_macroorganizer?tabid:short=1.png"),
        b"cached icon",
    )?;
    let _stick = FsQuirks::install(&stick, Quirks::vfat());

    let result = glib::MainContext::default().block_on(copy_recursively_fixing(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("ok.txt"))?, b"fine");
    assert_eq!(
        fs::read(target.join("nested/sc_macroorganizer_tabid_short=1.png"))?,
        b"cached icon"
    );
    assert_eq!(names(&target.join("nested"))?.len(), 1);
    Ok(())
}

#[test]
fn fat_family_copies_number_fixed_names_past_the_names_their_siblings_keep()
-> Result<(), Box<dyn Error>> {
    use crate::services::RenameCause::{Alias, Invalid};
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    type CopyTree = fn(
        gio::File,
        gio::File,
        gio::Cancellable,
        Rc<TransferProgressTracker>,
    ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>>;
    let copies: [(&str, CopyTree); 2] = [
        ("engine", |source, target, cancellable, progress| {
            Box::pin(async move {
                copy_new_recursively_on_filesystem(
                    source,
                    target,
                    Taken::Fail,
                    cancellable,
                    Some(progress),
                    fixing(),
                )
                .await
                .map(drop)
            })
        }),
        ("GIO", |source, target, cancellable, progress| {
            super::super::copy_recursively_with_progress(
                source,
                target,
                super::super::CopyInto::New,
                cancellable,
                super::super::CopyRecord::Nothing,
                Some(progress),
                super::super::GioNames::new(fixing()),
            )
        }),
    ];
    for (row, copy) in copies {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        let target = destination.join("target");
        fs::create_dir_all(&source)?;
        fs::create_dir(&destination)?;
        fs::write(source.join("a?.txt"), b"question")?;
        // Enough siblings that GIO lists them in several batches.
        for index in 0..130 {
            fs::write(source.join(format!("pad {index:03}")), b"")?;
        }
        fs::write(source.join("a:.txt"), b"colon")?;
        fs::write(source.join("A*.txt"), b"star")?;
        fs::write(source.join("a_.txt"), b"kept")?;
        fs::write(source.join("README.txt"), b"upper")?;
        fs::write(source.join("Readme.txt"), b"mixed")?;
        fs::write(source.join("CON"), b"device")?;
        for folder in ["Photos.2024", "photos.2024"] {
            fs::create_dir(source.join(folder))?;
            fs::write(source.join(folder).join("photo"), folder)?;
        }
        let _destination = FsQuirks::install(&destination, Quirks::vfat());
        let (progress, events) = recording_progress();

        let result = glib::MainContext::default().block_on(copy(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            gio::Cancellable::new(),
            progress,
        ));

        assert!(result.is_ok(), "{row}: {result:?}");
        assert_eq!(
            renamed_beneath(&target, &followed_report(&events.borrow())),
            [
                ("A*.txt", "A_ (1).txt", Invalid),
                ("CON", "CON_", Invalid),
                ("Readme.txt", "Readme (1).txt", Alias),
                ("a:.txt", "a_ (2).txt", Invalid),
                ("a?.txt", "a_ (3).txt", Invalid),
                ("photos.2024", "photos.2024 (1)", Alias),
            ]
            .map(|(name, saved_as, cause)| (
                name.to_owned(),
                saved_as.to_owned(),
                cause
            )),
            "{row}"
        );
        for (name, contents) in [
            ("a_.txt", "kept"),
            ("A_ (1).txt", "star"),
            ("a_ (2).txt", "colon"),
            ("a_ (3).txt", "question"),
            ("README.txt", "upper"),
            ("Readme (1).txt", "mixed"),
            ("CON_", "device"),
            ("Photos.2024/photo", "Photos.2024"),
            ("photos.2024 (1)/photo", "photos.2024"),
        ] {
            assert_eq!(
                fs::read(target.join(name))?,
                contents.as_bytes(),
                "{row}: {name}"
            );
        }
        assert_eq!(names(&target)?.len(), 139, "{row}");
    }
    Ok(())
}

#[test]
fn a_copy_into_a_case_sensitive_folder_keeps_a_cut_name_apart_by_case_only()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let disk = root.path().join("disk");
    let target = disk.join("target");
    fs::create_dir_all(&source)?;
    fs::create_dir(&disk)?;
    let (long, short) = (
        format!("A{}", "b".repeat(70)),
        format!("a{}", "b".repeat(63)),
    );
    for (name, contents) in [
        (long.as_str(), "long"),
        (short.as_str(), "short"),
        ("README", "upper"),
        ("Readme", "mixed"),
    ] {
        fs::write(source.join(name), contents)?;
    }
    let _disk = FsQuirks::install(
        &disk,
        Quirks {
            name_max: Some(64),
            ..Quirks::ext4()
        },
    );
    let (progress, events) = recording_progress();

    glib::MainContext::default().block_on(copy_new_recursively_on_filesystem(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        Taken::Fail,
        gio::Cancellable::new(),
        Some(progress),
        fixing(),
    ))?;

    assert_eq!(
        renamed_beneath(&target, &followed_report(&events.borrow())),
        [(
            long.clone(),
            format!("A{}", "b".repeat(63)),
            crate::services::RenameCause::TooLong
        )]
    );
    for (name, contents) in [
        (format!("A{}", "b".repeat(63)), "long"),
        (short, "short"),
        ("README".to_owned(), "upper"),
        ("Readme".to_owned(), "mixed"),
    ] {
        assert_eq!(fs::read(target.join(&name))?, contents.as_bytes(), "{name}");
    }
    assert_eq!(names(&target)?.len(), 4);
    Ok(())
}

#[test]
fn a_copy_onto_a_posix_mount_leaves_windows_characters_untouched() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let disk = root.path().join("disk");
    let target = disk.join("target");
    fs::create_dir_all(&source)?;
    fs::create_dir(&disk)?;
    fs::write(source.join("a?b:c.txt"), b"unchanged")?;
    let _disk = FsQuirks::install(&disk, Quirks::ext4());

    let result = glib::MainContext::default().block_on(copy_recursively_fixing(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("a?b:c.txt"))?, b"unchanged");
    Ok(())
}

fn refuses_colons_as_bad_messages(name: &OsStr) -> Option<Errno> {
    name.as_encoded_bytes()
        .contains(&b':')
        .then_some(Errno::BADMSG)
}

fn refuses_colons_as_missing(name: &OsStr) -> Option<Errno> {
    name.as_encoded_bytes()
        .contains(&b':')
        .then_some(Errno::NOENT)
}

/// Pastes `items` into `destination`, which they are not on.
fn paste_into(destination: &Path, items: &[PathBuf]) -> Vec<OperationEvent> {
    run_paste(PasteRequest {
        id: OperationRequestId(1590),
        destination: Location::local(destination),
        items: items
            .iter()
            .map(|source| PasteItem {
                source: Location::local(source),
                conflict: TransferConflict::FailIfExists,
            })
            .collect(),
        move_sources: false,
    })
}

/// What a paste brings onto a mount that refuses some names.
struct Refusing {
    label: &'static str,
    quirks: Preset,
    /// A refused name, and a sibling that has the name stricter rules give it.
    first: [&'static str; 2],
    /// A refused folder.
    second: &'static str,
    /// A refused item at the top, a folder or a file, and an item already
    /// there with the name stricter rules give it.
    top: (&'static str, bool, &'static str),
    /// Pasted later: a refused name and one ending in a dot in a folder,
    /// and a refused name at the top.
    later: [&'static str; 3],
    /// What `first`, `second`, `top` and `later` are saved as.
    saved: [&'static str; 6],
    /// The names the mount is asked for and refuses, in each paste.
    refused: [&'static [&'static str]; 2],
}

#[test]
fn a_refused_name_takes_the_first_stricter_rules_it_passes_and_teaches_only_a_mount_that_learns()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let unknown_daemon = || Quirks {
        mount_type: Some(("fuse.strictfs", "rw,user_id=0,group_id=0")),
        invalid_name: Some(refuses_colons_as_bad_messages),
        ..Quirks::default()
    };
    let ntfs_3g_without_udev = || Quirks {
        device_format: None,
        ..Quirks::ntfs_3g()
    };
    // As a casefolded folder refuses names its filesystem takes elsewhere.
    let strict_ext4 = || Quirks {
        invalid_name: Some(has_colon),
        ..Quirks::ext4()
    };
    // Each mount places the top item its own way: a staged folder, a
    // staged file, and a file linked from an unnamed one.
    let rows = [
        Refusing {
            label: "unknown FUSE daemon",
            quirks: unknown_daemon,
            first: ["a:b", "a_b"],
            second: "c:d",
            top: ("x:y", true, "x_y"),
            later: ["e:f", "dot.", "g:h"],
            saved: ["a_b (1)", "c_d", "x_y (1)", "e_f", "dot.", "g_h"],
            refused: [&["a:b", "x:y"], &[]],
        },
        // The top name is fixed for Windows first, and then refused.
        Refusing {
            label: "ntfs-3g that udev cannot name",
            quirks: ntfs_3g_without_udev,
            first: ["aux.c", "aux_.c"],
            second: "con.h",
            top: ("prn.txt?", false, "prn_.txt_"),
            later: ["com1.log", "dot.", "lpt1.ini"],
            saved: [
                "aux_ (1).c",
                "con_.h",
                "prn_ (1).txt_",
                "com1_.log",
                "dot",
                "lpt1_.ini",
            ],
            refused: [&["aux.c", "prn.txt_"], &[]],
        },
        Refusing {
            label: "ext4",
            quirks: strict_ext4,
            first: ["a:b", "a_b"],
            second: "c:d",
            top: ("x:y", false, "x_y"),
            later: ["e:f", "dot.", "g:h"],
            saved: ["a_b (1)", "c_d", "x_y (1)", "e_f", "dot.", "g_h"],
            refused: [&["a:b", "c:d", "x:y"], &["e:f", "g:h"]],
        },
    ];
    for row in rows {
        let label = row.label;
        let [first, kept] = row.first;
        let (top, top_folder, existing) = row.top;
        let [later, dotted, later_top] = row.later;
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let disk = root.path().join("disk");
        for folder in [
            Path::new("one"),
            &Path::new("two").join(row.second),
            Path::new("three"),
        ] {
            fs::create_dir_all(source.join(folder))?;
        }
        let top_file = if top_folder {
            fs::create_dir(source.join(top))?;
            Path::new(top).join("top")
        } else {
            PathBuf::from(top)
        };
        fs::create_dir(&disk)?;
        fs::write(disk.join(existing), "earlier")?;
        for (path, contents) in [
            (Path::new("one").join(first), "first"),
            (Path::new("one").join(kept), "kept"),
            (Path::new("two").join(row.second).join("inside"), "second"),
            (top_file, "top"),
            (Path::new("three").join(later), "later"),
            (Path::new("three").join(dotted), "dotted"),
            (PathBuf::from(later_top), "later top"),
        ] {
            fs::write(source.join(path), contents)?;
        }
        let quirks = (row.quirks)();
        let refuses = quirks.invalid_name.ok_or("no refusal")?;
        let scope = FsQuirks::install(&disk, quirks);
        let refused_calls = || {
            scope
                .calls()
                .iter()
                .flat_map(|call| call.names.clone())
                .filter(|name| refuses(name).is_some())
                .map(|name| name.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };

        let events = paste_into(
            &disk,
            &[source.join("one"), source.join("two"), source.join(top)],
        );

        let invalid = crate::services::RenameCause::Invalid;
        let [
            first_saved,
            second_saved,
            top_saved,
            later_saved,
            dotted_saved,
            later_top_saved,
        ] = row.saved;
        let second = row.second;
        let mut renamed = vec![
            (
                format!("one/{first}"),
                format!("one/{first_saved}"),
                invalid,
            ),
            (
                format!("two/{second}"),
                format!("two/{second_saved}"),
                invalid,
            ),
            (top.to_owned(), top_saved.to_owned(), invalid),
        ];
        renamed.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            renamed_beneath(&disk, &followed_report(&events)),
            renamed,
            "{label}"
        );
        let top_saved = if top_folder {
            Path::new(top_saved).join("top")
        } else {
            PathBuf::from(top_saved)
        };
        for (name, contents) in [
            (Path::new("one").join(kept), "kept"),
            (Path::new("one").join(first_saved), "first"),
            (Path::new("two").join(second_saved).join("inside"), "second"),
            (top_saved, "top"),
            (PathBuf::from(existing), "earlier"),
        ] {
            let read = fs::read(disk.join(&name))?;
            assert_eq!(read, contents.as_bytes(), "{label}: {}", name.display());
        }
        // Names planned before the mount learned are refused once each.
        assert_eq!(refused_calls(), row.refused[0], "{label}");
        scope.clear_calls();

        paste_into(&disk, &[source.join("three"), source.join(later_top)]);

        for (name, contents) in [
            (Path::new("three").join(later_saved), "later"),
            (Path::new("three").join(dotted_saved), "dotted"),
            (PathBuf::from(later_top_saved), "later top"),
        ] {
            let read = fs::read(disk.join(&name))?;
            assert_eq!(read, contents.as_bytes(), "{label}: {}", name.display());
        }
        assert_eq!(refused_calls(), row.refused[1], "{label}");
    }
    Ok(())
}

#[test]
fn a_name_refused_as_missing_neither_turns_unnamed_files_off_nor_writes_twice()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    // Names given back are refused as they are.
    for (names, saved) in [(fixing(), Some("a_b")), (NamePolicy::default(), None)] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let disk = root.path().join("disk");
        fs::create_dir_all(&source)?;
        fs::create_dir(&disk)?;
        fs::write(source.join("a:b"), b"colon")?;
        fs::write(source.join("plain"), b"plain")?;
        let scope = FsQuirks::install(
            &disk,
            Quirks {
                invalid_name: Some(refuses_colons_as_missing),
                ..Quirks::ext4()
            },
        );
        let copy = |name: &str| {
            glib::MainContext::default().block_on(copy_new_recursively_on_filesystem(
                gio::File::for_path(source.join(name)),
                gio::File::for_path(disk.join(name)),
                Taken::Fail,
                gio::Cancellable::new(),
                None,
                names.clone(),
            ))
        };

        let copied = copy("a:b");

        match saved {
            Some(saved) => {
                assert_eq!(copied?.path(), Some(disk.join(saved)));
                assert_eq!(fs::read(disk.join(saved))?, b"colon");
                assert!(!scope.used_name(".strata-copy-*"), "{:?}", scope.calls());
            }
            None => assert!(copied.is_err()),
        }
        scope.clear_calls();

        copy("plain")?;

        assert_eq!(fs::read(disk.join("plain"))?, b"plain");
        assert_eq!(
            scope.count(Op::OpenTmpfile),
            1,
            "{saved:?}: {:?}",
            scope.calls()
        );
    }
    Ok(())
}

fn refused_as_invalid(name: &OsStr) -> Option<Errno> {
    (name == "refused").then_some(Errno::INVAL)
}

fn refused_as_illegal(name: &OsStr) -> Option<Errno> {
    (name == "refused").then_some(Errno::ILSEQ)
}

fn refused_as_bad_message(name: &OsStr) -> Option<Errno> {
    (name == "refused").then_some(Errno::BADMSG)
}

fn refused_as_missing(name: &OsStr) -> Option<Errno> {
    (name == "refused").then_some(Errno::NOENT)
}

fn refused_as_too_long(name: &OsStr) -> Option<Errno> {
    (name == "refused").then_some(Errno::NAMETOOLONG)
}

#[test]
fn a_name_no_stricter_rules_change_fails_naming_the_item_and_the_reason()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    type Refusal = fn(&OsStr) -> Option<Errno>;
    let invalid = crate::services::INVALID_NAME_REASON;
    let too_long = "The name is too long";
    let rows: [(Refusal, bool, &str); 7] = [
        (refused_as_invalid, true, invalid),
        (refused_as_illegal, true, invalid),
        (refused_as_bad_message, true, invalid),
        (refused_as_missing, true, invalid),
        (refused_as_too_long, true, too_long),
        (refused_as_invalid, false, invalid),
        (refused_as_bad_message, false, invalid),
    ];
    for (refusal, nested, reason) in rows {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let disk = root.path().join("disk");
        fs::create_dir_all(&source)?;
        fs::create_dir(&disk)?;
        fs::write(source.join("refused"), b"refused")?;
        let _disk = FsQuirks::install(
            &disk,
            Quirks {
                invalid_name: Some(refusal),
                ..Quirks::default()
            },
        );
        let item = if nested {
            source.clone()
        } else {
            source.join("refused")
        };
        let target = disk.join(item.file_name().ok_or("no name")?);

        let error = glib::MainContext::default()
            .block_on(copy_new_recursively_on_filesystem(
                gio::File::for_path(&item),
                gio::File::for_path(&target),
                Taken::Fail,
                gio::Cancellable::new(),
                None,
                fixing(),
            ))
            .expect_err("refused");

        assert_eq!(
            error.message(),
            rust_i18n::t!(
                "Could not copy %{name}: %{error}",
                name = "refused",
                error = crate::services::error_detail(crate::i18n::tr(reason))
            ),
            "{reason}, nested: {nested}"
        );
    }
    Ok(())
}

fn stage(root: &Path, directory: bool) -> Result<StagedSibling, Box<dyn Error>> {
    if directory {
        staged(root, StageContent::Directory)
    } else {
        file_stage(root)
    }
}

fn publish(staged: StagedSibling, target: &Path) -> Result<(), glib::Error> {
    glib::MainContext::default().block_on(publish_staged_without_replace(
        staged,
        target.to_path_buf(),
        false,
        gio::Cancellable::new(),
    ))
}

#[test]
fn staged_publication_without_rename_flags_places_the_stage_itself() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::MetadataExt;
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    let presets: [(&str, Preset); 5] = [
        ("EINVAL", Quirks::without_rename_flags),
        ("ENOSYS", || Quirks {
            rename_flags: Support::Rejected(Errno::NOSYS),
            ..Quirks::default()
        }),
        ("EOPNOTSUPP", || Quirks {
            rename_flags: Support::Rejected(Errno::OPNOTSUPP),
            ..Quirks::default()
        }),
        ("sshfs", Quirks::sshfs),
        ("vboxsf", Quirks::vboxsf),
    ];
    for (case, quirks) in presets {
        for directory in [false, true] {
            let root = tempfile::tempdir()?;
            let _scope = FsQuirks::install(root.path(), quirks());
            let staged = stage(root.path(), directory)?;
            let staged_path = root.path().join(&staged.name);
            let target = root.path().join("target");
            if directory {
                fs::create_dir_all(staged_path.join("nested"))?;
                fs::write(staged_path.join("nested/file.txt"), b"directory data")?;
            } else {
                fs::write(&staged_path, b"file data")?;
            }
            let inode = fs::metadata(&staged_path)?.ino();

            publish(staged, &target)?;

            assert_eq!(names(root.path())?, ["target"], "{case}");
            assert_eq!(fs::metadata(&target)?.ino(), inode, "{case}");
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
        Quirks::vboxsf,
    ] {
        for directory in [false, true] {
            let root = tempfile::tempdir()?;
            let _scope = FsQuirks::install(root.path(), quirks());
            let staged = stage(root.path(), directory)?;
            let staged_path = root.path().join(&staged.name);
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
            let stage_path = root.path().join(&staged.name);
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
    let _scope = FsQuirks::install(root.path(), Quirks::vboxsf());
    let staged = staged(root.path(), StageContent::Symlink("missing".into()))?;
    let staged_path = root.path().join(&staged.name);
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
    let staged_path = root.path().join(&staged.name);
    let _socket = std::os::unix::net::UnixListener::bind(staged_path.join("unsupported.sock"))?;
    let target = root.path().join("target");

    assert!(publish(staged, &target).is_err());

    assert!(!target.exists());
    assert!(!staged_path.exists());
    Ok(())
}

fn only_entry(folder: &Path, prefix: &str) -> PathBuf {
    fs::read_dir(folder)
        .expect("list the stage's folder")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
        })
        .expect("a stage")
}

fn only_stage(root: &Path) -> PathBuf {
    only_entry(root, ".strata-replacement-")
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
    // Two stats decide publication; the third is the discard.
    let _scope = FsQuirks::install(
        root.path(),
        reusing_stage_inodes((Op::Stat, ".strata-replacement-*", 2, racer)),
    );
    let staged = stage(root.path(), false)?;
    let staged_path = root.path().join(&staged.name);
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
    let staged_path = root.path().join(&staged.name);
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
    let target = root.path().join("target");
    let cancellable = gio::Cancellable::new();
    let reserved = Arc::new(AtomicBool::new(false));
    let (seen, made, cancel) = (reserved.clone(), target.clone(), cancellable.clone());
    let racer: Racer = Box::new(move || {
        seen.store(made.is_dir(), std::sync::atomic::Ordering::Relaxed);
        cancel.cancel();
    });
    let scope = FsQuirks::install(
        root.path(),
        Quirks {
            before: vec![(Op::OpenSource, "large.bin", 0, racer)],
            ..Quirks::without_placement()
        },
    );
    // No placeholder, so the target appears only when the copy makes it.
    learn_mount_capability(scope.key(), |capabilities| {
        capabilities.rename_over_empty_directory = Some(false);
    });
    let staged = stage(root.path(), true)?;
    let staged_path = root.path().join(&staged.name);
    fs::File::create(staged_path.join("large.bin"))?.set_len(64 * 1024 * 1024)?;

    let result = glib::MainContext::default().block_on(publish_staged_without_replace(
        staged,
        target.clone(),
        false,
        cancellable,
    ));

    assert!(
        reserved.load(std::sync::atomic::Ordering::Relaxed),
        "fallback must reserve the destination before cancellation"
    );
    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)),
        "{result:?}"
    );
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

    glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        true,
        gio::Cancellable::new(),
    ))?;

    assert_eq!(fs::read(target.join("Link"))?, b"existing");
    assert_eq!(
        fs::read_link(target.join("Link (1)"))?,
        Path::new("elsewhere")
    );
    Ok(())
}

#[test]
fn read_only_items_keep_their_xattrs() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .map_err(|error| error.to_string())?;
    for (directory, mode) in [(false, 0o444), (true, 0o555)] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        if directory {
            fs::create_dir(&source)?;
            fs::write(source.join("inside.txt"), b"contents")?;
        } else {
            fs::write(&source, b"contents")?;
        }
        rustix::fs::setxattr(
            &source,
            "user.strata-test",
            b"metadata",
            rustix::fs::XattrFlags::empty(),
        )?;
        fs::set_permissions(&source, fs::Permissions::from_mode(mode))?;

        let copied = glib::MainContext::default().block_on(copy_new_recursively(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            gio::Cancellable::new(),
        ));

        let copied_mode = fs::metadata(&target).map(|metadata| metadata.mode() & 0o7777);
        let mut value = [0u8; 32];
        let xattr = rustix::fs::getxattr(&target, "user.strata-test", &mut value[..]);
        for path in [&source, &target] {
            if path.is_dir() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
            }
        }
        copied?;
        assert_eq!(copied_mode?, mode, "{directory}");
        assert_eq!(&value[..xattr?], b"metadata", "{directory}");
    }
    Ok(())
}
