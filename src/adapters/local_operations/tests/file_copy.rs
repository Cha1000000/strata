// SPDX-License-Identifier: MIT

use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    sync::Mutex,
};

use rustix::io::Errno;

use super::fs_quirks::{Call, Op, Racer, Support, TmpfileLink};
use super::*;
use crate::adapters::volume::{XattrNamespace, mount_capabilities};

/// A source folder and a destination folder, each with its own quirks, so
/// they act as separate mounts.
struct Mounts {
    _root: tempfile::TempDir,
    sources: PathBuf,
    destination: PathBuf,
    _source_scope: FsQuirks,
    destination_scope: FsQuirks,
}

fn mounts(
    source: Quirks,
    destination: impl FnOnce(&Path) -> Quirks,
) -> Result<Mounts, Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let target = root.path().join("destination");
    fs::create_dir(&sources)?;
    fs::create_dir(&target)?;
    Ok(Mounts {
        _source_scope: FsQuirks::install(&sources, source),
        destination_scope: FsQuirks::install(&target, destination(&target)),
        _root: root,
        sources,
        destination: target,
    })
}

impl Mounts {
    fn source(&self, contents: &[u8]) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.sources.join("source.bin");
        fs::write(&path, contents)?;
        Ok(path)
    }

    fn target(&self) -> PathBuf {
        self.destination.join("target.bin")
    }
}

fn copy(source: &Path, target: &Path) -> Result<(), glib::Error> {
    glib::MainContext::default().block_on(copy_new_recursively(
        gio::File::for_path(source),
        gio::File::for_path(target),
        gio::Cancellable::new(),
    ))
}

fn patterned(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

/// Lists `folder` into `seen` when it runs.
fn list_into(folder: &Path, seen: &Arc<Mutex<Option<Vec<String>>>>) -> Racer {
    let (folder, seen) = (folder.to_path_buf(), seen.clone());
    Box::new(move || {
        *seen.lock().expect("listing") = Some(names(&folder).expect("list the destination"));
    })
}

fn ops(calls: &[Call]) -> Vec<Op> {
    calls.iter().map(|call| call.op).collect()
}

fn xattrs_set(calls: &[Call]) -> Vec<OsString> {
    calls
        .iter()
        .filter(|call| call.op == Op::SetXattr)
        .filter_map(|call| call.names.last().cloned())
        .collect()
}

#[test]
fn a_file_being_copied_is_invisible_until_complete() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let seen = Arc::new(Mutex::new(None));
    let mounts = mounts(Quirks::default(), |destination| Quirks {
        before: vec![(Op::Write, "*", 0, list_into(destination, &seen))],
        ..Quirks::default()
    })?;
    let contents = patterned(2 * 1024 * 1024);
    let source = mounts.source(&contents)?;

    copy(&source, &mounts.target())?;

    let seen = seen
        .lock()
        .map_err(|error| error.to_string())?
        .take()
        .ok_or("the copy wrote nothing")?;
    assert!(seen.iter().all(|name| name.starts_with('.')), "{seen:?}");
    if mount_capabilities(mounts.destination_scope.key()).tmpfile == Some(true) {
        assert!(seen.is_empty(), "{seen:?}");
    }
    assert_eq!(fs::read(mounts.target())?, contents);
    Ok(())
}

#[test]
fn files_use_a_hidden_stage_where_tmpfiles_are_unavailable() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    for (case, preset, probed) in [
        (
            "tmpfiles refused",
            (|| Quirks {
                tmpfile: Support::Rejected(Errno::OPNOTSUPP),
                ..Quirks::ext4()
            }) as Preset,
            true,
        ),
        ("NFS", Quirks::nfs42, false),
    ] {
        let seen = Arc::new(Mutex::new(None));
        let mounts = mounts(Quirks::default(), |destination| Quirks {
            before: vec![(Op::Write, "*", 0, list_into(destination, &seen))],
            ..preset()
        })?;
        let source = mounts.source(b"contents")?;

        copy(&source, &mounts.target())?;

        let seen = seen
            .lock()
            .map_err(|error| error.to_string())?
            .take()
            .ok_or("the copy wrote nothing")?;
        assert!(
            matches!(seen.as_slice(), [stage] if stage.starts_with(".strata-copy-")),
            "{case}: {seen:?}"
        );
        let scope = &mounts.destination_scope;
        assert_eq!(scope.count(Op::OpenTmpfile), usize::from(probed), "{case}");
        if probed {
            assert_eq!(mount_capabilities(scope.key()).tmpfile, Some(false));
        }
        assert_eq!(names(&mounts.destination)?, ["target.bin"], "{case}");
        assert_eq!(fs::read(mounts.target())?, b"contents");
    }
    Ok(())
}

#[test]
fn a_tmpfile_that_cannot_be_linked_is_published_without_rereading_the_source()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    for (case, destination, staged) in [
        (
            "AT_EMPTY_PATH refused",
            (|| Quirks {
                tmpfile_link: TmpfileLink::ProcOnly,
                ..Quirks::default()
            }) as Preset,
            false,
        ),
        (
            "no form of link",
            || Quirks {
                tmpfile_link: TmpfileLink::Unavailable,
                ..Quirks::default()
            },
            true,
        ),
        (
            "links refused",
            || Quirks {
                links: Support::Rejected(Errno::MLINK),
                ..Quirks::default()
            },
            true,
        ),
    ] {
        // One pass reads the data, then the end; a third read would fail.
        let mounts = mounts(
            Quirks {
                fail: vec![(Op::Read, "source.bin", 2, Errno::IO)],
                ..Quirks::default()
            },
            |_| destination(),
        )?;
        let source = mounts.source(b"contents")?;

        copy(&source, &mounts.target())?;

        let scope = &mounts.destination_scope;
        assert_eq!(scope.used_name(".strata-copy-*"), staged, "{case}");
        assert_eq!(names(&mounts.destination)?, ["target.bin"], "{case}");
        assert_eq!(fs::read(mounts.target())?, b"contents", "{case}");
    }
    Ok(())
}

#[test]
fn a_name_taken_just_before_the_link_is_left_alone() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.txt");
    fs::write(&source, b"ours")?;
    let target = root.path().join("target.txt");
    let taken = target.clone();
    let scope = FsQuirks::install(
        root.path(),
        Quirks {
            before: vec![(
                Op::LinkFd,
                "*",
                0,
                Box::new(move || fs::write(&taken, b"theirs").expect("take the name")),
            )],
            ..Quirks::default()
        },
    );

    let error = copy(&source, &target).expect_err("the name is taken");

    assert!(error.matches(gio::IOErrorEnum::Exists), "{error}");
    assert_eq!(scope.count(Op::LinkFd), 1);
    assert_eq!(fs::read(&target)?, b"theirs");
    assert_eq!(names(root.path())?, ["source.txt", "target.txt"]);
    Ok(())
}

#[test]
fn a_folder_removed_before_the_link_leaves_tmpfiles_in_use() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.txt");
    fs::write(&source, b"contents")?;
    let removed = root.path().join("removed");
    fs::create_dir(&removed)?;
    let folder = removed.clone();
    let scope = FsQuirks::install(
        root.path(),
        Quirks {
            before: vec![(
                Op::LinkFd,
                "*",
                0,
                Box::new(move || fs::remove_dir(&folder).expect("remove the folder")),
            )],
            ..Quirks::default()
        },
    );
    copy(&source, &removed.join("target.txt")).expect_err("the folder is gone");
    scope.clear_calls();

    copy(&source, &root.path().join("target.txt"))?;

    assert_eq!(scope.count(Op::OpenTmpfile), 1, "{:?}", scope.calls());
    assert!(!scope.used_name(".strata-copy-*"));
    assert_eq!(fs::read(root.path().join("target.txt"))?, b"contents");
    Ok(())
}

#[test]
fn running_out_of_space_leaves_nothing_behind() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    for (case, preset) in [
        ("unnamed file", Quirks::default as Preset),
        ("named stage", || Quirks {
            tmpfile: Support::Rejected(Errno::OPNOTSUPP),
            ..Quirks::ext4()
        }),
    ] {
        let mounts = mounts(Quirks::default(), |_| Quirks {
            fail: vec![(Op::Write, "*", 1, Errno::NOSPC)],
            ..preset()
        })?;
        let source = mounts.source(&patterned(3 * 1024 * 1024))?;

        let error = copy(&source, &mounts.target()).expect_err("the second write fails");

        assert!(error.matches(gio::IOErrorEnum::NoSpace), "{case}: {error}");
        assert!(names(&mounts.destination)?.is_empty(), "{case}");
    }
    Ok(())
}

#[test]
fn short_reads_and_writes_still_copy_every_byte() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let short = || Quirks {
        short_io: Some(4096),
        ..Quirks::default()
    };
    let mounts = mounts(short(), |_| short())?;
    let contents = patterned(100 * 1024);
    let source = mounts.source(&contents)?;

    copy(&source, &mounts.target())?;

    assert_eq!(fs::read(mounts.target())?, contents);
    assert!(mounts.destination_scope.count(Op::Write) >= contents.len() / 4096);
    Ok(())
}

#[test]
fn a_write_error_reported_at_close_fails_the_copy() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    // An unnamed file is closed once linked, under its inode's name.
    for (case, closed, preset) in [
        ("named stage", ".strata-copy-*", Quirks::nfs3 as Preset),
        ("unnamed file", "#*", Quirks::default),
    ] {
        let mounts = mounts(Quirks::default(), |_| Quirks {
            fail: vec![(Op::Close, closed, 0, Errno::IO)],
            ..preset()
        })?;
        let source = mounts.source(b"contents")?;

        copy(&source, &mounts.target()).expect_err("the close fails");

        assert!(mounts.destination_scope.used_name(closed), "{case}");
        assert!(names(&mounts.destination)?.is_empty(), "{case}");
    }
    Ok(())
}

#[test]
fn connection_loss_while_copying_metadata_fails_the_copy() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    for op in [Op::XattrNames, Op::GetXattr, Op::SetXattr] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source.txt");
        fs::write(&source, b"contents")?;
        let _scope = FsQuirks::install(
            root.path(),
            Quirks {
                extra_xattrs: vec![("user.tag", b"value")],
                fail: vec![(op, "*", 0, Errno::NOTCONN)],
                ..Quirks::default()
            },
        );

        copy(&source, &root.path().join("target.txt")).expect_err("the connection is lost");

        assert_eq!(names(root.path())?, ["source.txt"], "{op:?}");
    }
    Ok(())
}

#[test]
fn a_link_whose_times_fail_with_eio_is_not_left_behind() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    std::os::unix::fs::symlink("elsewhere", &source)?;
    let _scope = FsQuirks::install(
        root.path(),
        Quirks {
            fail: vec![(Op::SetLinkTimes, "*", 0, Errno::IO)],
            ..Quirks::default()
        },
    );

    copy(&source, &root.path().join("target")).expect_err("the device fails");

    assert_eq!(names(root.path())?, ["source"]);
    Ok(())
}

#[test]
fn a_clone_failing_with_eio_is_not_retried() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.txt");
    fs::write(&source, b"contents")?;
    let scope = FsQuirks::install(
        root.path(),
        Quirks {
            fail: vec![(Op::Clone, "*", 0, Errno::IO)],
            ..Quirks::default()
        },
    );

    copy(&source, &root.path().join("target.txt")).expect_err("the clone fails");

    assert_eq!(scope.count(Op::Clone), 1);
    assert_eq!(scope.count(Op::CopyRange) + scope.count(Op::Write), 0);
    assert_eq!(names(root.path())?, ["source.txt"]);
    Ok(())
}

#[test]
fn a_file_that_reports_no_size_is_copied_whole() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("cpuinfo");
    fs::write(&source, b"processor : 0\n")?;
    let _scope = FsQuirks::install(
        root.path(),
        Quirks {
            mount_type: Some("fuse.lxcfs"),
            virtual_sizes: true,
            ..Quirks::default()
        },
    );
    let target = root.path().join("copy");

    copy(&source, &target)?;

    assert_eq!(fs::read(&target)?, b"processor : 0\n");
    Ok(())
}

#[test]
fn copy_range_refused_across_mounts_continues_with_reads_and_writes() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let mounts = mounts(Quirks::default(), |_| Quirks::default())?;
    let contents = patterned(3 * 1024 * 1024);
    let source = mounts.source(&contents)?;

    copy(&source, &mounts.target())?;

    let calls = ops(&mounts.destination_scope.calls());
    let range = calls
        .iter()
        .position(|op| *op == Op::CopyRange)
        .ok_or("copy_file_range was not tried")?;
    assert!(calls[range..].contains(&Op::Write), "{calls:?}");
    assert_eq!(fs::read(mounts.target())?, contents);
    Ok(())
}

#[test]
fn times_survive_filesystems_that_reset_them() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    for (case, preset, (before, after)) in [
        ("NFSv4.2", Quirks::nfs42 as Preset, (false, true)),
        ("NFSv3", Quirks::nfs3, (true, false)),
        (
            "FUSE writeback cache",
            || Quirks {
                times_reset_at_close: true,
                ..Quirks::sshfs()
            },
            (true, true),
        ),
    ] {
        let mounts = mounts(Quirks::default(), |_| preset())?;
        let source = mounts.source(b"contents")?;
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        fs::File::open(&source)?.set_modified(modified)?;

        copy(&source, &mounts.target())?;

        let calls = ops(&mounts.destination_scope.calls());
        let closed = calls
            .iter()
            .position(|op| *op == Op::Close)
            .ok_or("the stage was not closed")?;
        assert_eq!(
            (
                calls[..closed].contains(&Op::SetTimes),
                calls[closed..].contains(&Op::SetTimes)
            ),
            (before, after),
            "{case}: {calls:?}"
        );
        assert_eq!(
            fs::metadata(mounts.target())?.modified()?,
            modified,
            "{case}"
        );
    }
    Ok(())
}

#[test]
fn copies_their_owner_cannot_read_keep_their_times_on_nfs4() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    // The owner may only write the first copy, and neither read nor write the second.
    for mode in [0o244, 0o044] {
        let mounts = mounts(
            Quirks {
                reported_mode: Some(("source.bin", mode)),
                ..Quirks::default()
            },
            |_| Quirks::nfs42(),
        )?;
        let source = mounts.source(b"contents")?;
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        fs::File::open(&source)?.set_modified(modified)?;

        copy(&source, &mounts.target())?;

        let copied = fs::metadata(mounts.target())?;
        assert_eq!(copied.mode() & 0o777, mode, "{mode:o}");
        assert_eq!(copied.modified()?, modified, "{mode:o}");
    }
    Ok(())
}

#[test]
fn special_bits_are_dropped_when_the_owner_cannot_be_kept() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    for (case, mode, shared_destination) in [
        ("group refused", 0o4755, false),
        ("setgid destination", 0o2755, true),
    ] {
        let mounts = mounts(
            Quirks {
                foreign_group: true,
                ..Quirks::default()
            },
            |_| Quirks {
                chown: Support::Rejected(Errno::PERM),
                ..Quirks::default()
            },
        )?;
        if shared_destination {
            fs::set_permissions(&mounts.destination, fs::Permissions::from_mode(0o2775))?;
        }
        let source = mounts.source(b"#!/bin/sh\n")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(mode))?;

        copy(&source, &mounts.target())?;

        let copied = fs::metadata(mounts.target())?;
        assert_eq!(copied.mode() & 0o7777, 0o755, "{case}");
        // A setgid folder's group is kept, so the owner is never changed there.
        assert_eq!(
            mounts.destination_scope.count(Op::ChownFd),
            usize::from(!shared_destination),
            "{case}"
        );
        assert_eq!(
            copied.gid(),
            fs::metadata(&mounts.destination)?.gid(),
            "{case}"
        );
    }
    Ok(())
}

#[test]
fn xattr_namespaces_are_learned_separately() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let first = root.path().join("first.txt");
    let second = root.path().join("second.txt");
    fs::write(&first, b"first")?;
    fs::write(&second, b"second")?;
    let target = root.path().join("copies");
    fs::create_dir(&target)?;

    let scope = FsQuirks::install(
        root.path(),
        Quirks {
            xattrs_refused: vec![("user.", Errno::OPNOTSUPP)],
            extra_xattrs: vec![("user.tag", b"1"), ("security.label", b"2")],
            ..Quirks::default()
        },
    );
    copy(&first, &target.join("first.txt"))?;
    scope.clear_calls();
    copy(&second, &target.join("second.txt"))?;
    assert_eq!(
        xattrs_set(&scope.calls()),
        [OsString::from("security.label")]
    );
    let refused = mount_capabilities(scope.key()).refused_xattrs;
    assert!(refused.contains(XattrNamespace::User));
    assert!(!refused.contains(XattrNamespace::Security));
    drop(scope);

    let scope = FsQuirks::install(
        root.path(),
        Quirks {
            extra_xattrs: vec![
                ("security.evm", b"stale"),
                ("$LXMOD", b"\x00\x00"),
                ("user.tag", b"kept"),
            ],
            ..Quirks::default()
        },
    );
    copy(&first, &target.join("third.txt"))?;
    assert_eq!(xattrs_set(&scope.calls()), [OsString::from("user.tag")]);
    Ok(())
}
