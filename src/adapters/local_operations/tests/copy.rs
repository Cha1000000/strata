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
    assert!(
        merged
            .as_ref()
            .is_err_and(|error| error.matches(gio::IOErrorEnum::Exists)),
        "a merge only fills names it moved out of the way: {merged:?}"
    );
    assert_eq!(fs::read(target.join("top.txt"))?, b"top");

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
            // Another program fills the name the lost reply left.
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
    for (row, preset, pipes) in [
        ("tmpfs", Quirks::default as Preset, true),
        ("ext4", Quirks::ext4, true),
        ("vfat", Quirks::vfat, false),
    ] {
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
        let _destination = FsQuirks::install(&destination, preset());
        let target = destination.join("copy");

        let report = copy_reporting(&source, &target, None)?;

        let mut expected = vec![(
            Location::local(target.join("socket")),
            crate::services::SpecialKind::Socket,
        )];
        if pipes {
            let pipe = fs::symlink_metadata(target.join("00-pipe"))?;
            assert!(pipe.file_type().is_fifo(), "{row}");
            assert_eq!(pipe.mode() & 0o7777, 0o640, "{row}");
            assert_eq!(pipe.mtime(), 1_000_000_000, "{row}");
        } else {
            assert!(!target.join("00-pipe").exists(), "{row}");
            expected.insert(
                0,
                (
                    Location::local(target.join("00-pipe")),
                    crate::services::SpecialKind::Pipe,
                ),
            );
        }
        assert_eq!(
            report
                .skipped
                .into_iter()
                .map(|item| (item.location, item.kind))
                .collect::<Vec<_>>(),
            expected,
            "{row}"
        );
        assert!(!target.join("socket").exists(), "{row}");
        for index in 1..4 {
            assert_eq!(
                fs::read_to_string(target.join(format!("{index:02}-file.txt")))?,
                index.to_string(),
                "{row}"
            );
        }
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
            assert!(
                error.message().contains("not a regular file"),
                "{row}: {error}"
            );
            assert_eq!(names(&destination)?, Vec::<String>::new(), "{row}");
        }
    }
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
    ));

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(fs::read(target.join("a?b:c.txt"))?, b"unchanged");
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

    let error = glib::MainContext::default()
        .block_on(copy_recursively(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            true,
            gio::Cancellable::new(),
        ))
        .expect_err("an existing entry blocks the link");

    assert!(error.matches(gio::IOErrorEnum::Exists), "{error}");
    assert_eq!(fs::read(target.join("Link"))?, b"existing");
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
