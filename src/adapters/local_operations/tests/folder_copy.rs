// SPDX-License-Identifier: MIT

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use rustix::io::Errno;

use super::fs_quirks::{Op, Racer, Support};
use super::*;
use crate::adapters::volume::{MountCapabilities, learn_mount_capability};

/// A source folder and a destination folder, each with its own quirks, so
/// they act as separate mounts.
struct Mounts {
    _root: tempfile::TempDir,
    sources: PathBuf,
    destination: PathBuf,
    source_scope: FsQuirks,
    destination_scope: FsQuirks,
}

fn mounts(destination: Quirks) -> Result<Mounts, Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let sources = root.path().join("sources");
    let target = root.path().join("destination");
    fs::create_dir(&sources)?;
    fs::create_dir(&target)?;
    Ok(Mounts {
        source_scope: FsQuirks::install(&sources, Quirks::default()),
        destination_scope: FsQuirks::install(&target, destination),
        _root: root,
        sources,
        destination: target,
    })
}

fn copy(source: &Path, target: &Path) -> Result<(), glib::Error> {
    glib::MainContext::default().block_on(copy_new_recursively(
        gio::File::for_path(source),
        gio::File::for_path(target),
        gio::Cancellable::new(),
    ))
}

fn mode(path: &Path) -> io::Result<u32> {
    Ok(fs::symlink_metadata(path)?.mode() & 0o7777)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Gives every folder under `root` its owner's access back, so it can be removed.
fn unlock(root: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(root)?.is_dir() {
        return Ok(());
    }
    set_mode(root, 0o755)?;
    for entry in fs::read_dir(root)? {
        unlock(&entry?.path())?;
    }
    Ok(())
}

fn set_mtime(path: &Path, seconds: i64) -> io::Result<()> {
    let time = rustix::fs::Timespec {
        tv_sec: seconds,
        tv_nsec: 0,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &rustix::fs::Timestamps {
            last_access: time,
            last_modification: time,
        },
        rustix::fs::AtFlags::empty(),
    )?;
    Ok(())
}

fn calls_naming(scope: &FsQuirks, op: Op, name: &str) -> usize {
    scope
        .calls()
        .into_iter()
        .filter(|call| call.op == op && call.names.iter().any(|called| called == name))
        .count()
}

#[test]
fn copying_a_folder_keeps_folder_permissions_and_times() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir_all(source.join("private"))?;
    fs::write(source.join("private/file.txt"), b"contents")?;
    let folders = [
        ("private", 0o700, 1_000_000_000),
        ("", 0o750, 1_100_000_000),
    ];
    for (relative, mode, seconds) in folders {
        let folder = source.join(relative);
        rustix::fs::setxattr(
            &folder,
            "user.strata-test",
            relative.as_bytes(),
            rustix::fs::XattrFlags::empty(),
        )?;
        set_mode(&folder, mode)?;
        rustix::fs::utimensat(
            rustix::fs::CWD,
            &folder,
            &rustix::fs::Timestamps {
                last_access: rustix::fs::Timespec {
                    tv_sec: seconds - 1_000,
                    tv_nsec: 0,
                },
                last_modification: rustix::fs::Timespec {
                    tv_sec: seconds,
                    tv_nsec: 0,
                },
            },
            rustix::fs::AtFlags::empty(),
        )?;
    }

    copy(&source, &target)?;

    for (relative, expected, seconds) in folders {
        let folder = target.join(relative);
        let metadata = fs::symlink_metadata(&folder)?;
        assert_eq!(metadata.mode() & 0o7777, expected, "{relative:?}");
        assert_eq!(metadata.mtime(), seconds, "{relative:?}");
        assert_eq!(metadata.atime(), seconds - 1_000, "{relative:?}");
        let mut value = [0u8; 32];
        let length = rustix::fs::getxattr(&folder, "user.strata-test", &mut value[..])?;
        assert_eq!(&value[..length], relative.as_bytes(), "{relative:?}");
    }
    assert_eq!(fs::read(target.join("private/file.txt"))?, b"contents");
    Ok(())
}

const DESCRIPTOR_LIMIT: &str = "STRATA_TEST_DESCRIPTOR_LIMIT";

#[test]
fn copying_many_read_only_folders_needs_no_descriptor_per_folder() -> Result<(), Box<dyn Error>> {
    let inner = "adapters::local_operations::tests::folder_copy::read_only_folders_under_a_descriptor_limit";
    let output = std::process::Command::new(std::env::current_exe()?)
        .args([inner, "--exact", "--ignored", "--test-threads=1"])
        .env(DESCRIPTOR_LIMIT, "256")
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
#[ignore = "run by copying_many_read_only_folders_needs_no_descriptor_per_folder"]
fn read_only_folders_under_a_descriptor_limit() -> Result<(), Box<dyn Error>> {
    let Some(limit) = std::env::var(DESCRIPTOR_LIMIT)
        .ok()
        .and_then(|limit| limit.parse().ok())
    else {
        return Ok(());
    };
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    let groups = (0..40).map(|group| source.join(group.to_string()));
    for group in groups.clone() {
        for folder in 0..50 {
            fs::create_dir_all(group.join(folder.to_string()))?;
        }
    }
    for group in groups.clone() {
        for folder in 0..50 {
            set_mode(&group.join(folder.to_string()), 0o555)?;
        }
        set_mode(&group, 0o555)?;
    }
    rustix::process::setrlimit(
        rustix::process::Resource::Nofile,
        rustix::process::Rlimit {
            current: Some(limit),
            maximum: Some(limit),
        },
    )?;

    let copied = copy(&source, &target);

    let modes = (0..40)
        .flat_map(|group| (0..50).map(move |folder| format!("{group}/{folder}")))
        .chain((0..40).map(|group| group.to_string()))
        .map(|folder| mode(&target.join(folder)))
        .collect::<io::Result<HashSet<_>>>();
    unlock(&source)?;
    if target.exists() {
        unlock(&target)?;
    }
    copied?;
    assert_eq!(modes?, HashSet::from([0o555]));
    Ok(())
}

#[test]
fn copying_into_a_setgid_folder_keeps_it_shared() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let (sources, shared) = (root.path().join("sources"), root.path().join("shared"));
    fs::create_dir_all(sources.join("project/sub"))?;
    fs::write(sources.join("project/file.txt"), b"contents")?;
    set_mode(&sources.join("project/file.txt"), 0o644)?;
    set_mode(&sources.join("project/sub"), 0o750)?;
    set_mode(&sources.join("project"), 0o755)?;
    fs::create_dir(&shared)?;
    set_mode(&shared, 0o2775)?;
    // The source reports a group of its own, which the shared folder's replaces.
    let _sources = FsQuirks::install(
        &sources,
        Quirks {
            foreign_group: true,
            ..Quirks::default()
        },
    );
    let destination = FsQuirks::install(&shared, Quirks::default());

    copy(&sources.join("project"), &shared.join("project"))?;

    let group = fs::metadata(&shared)?.gid();
    for (relative, expected) in [
        ("project", 0o2755),
        ("project/sub", 0o2750),
        ("project/file.txt", 0o644),
    ] {
        let metadata = fs::symlink_metadata(shared.join(relative))?;
        assert_eq!(metadata.mode() & 0o7777, expected, "{relative}");
        assert_eq!(metadata.gid(), group, "{relative}");
    }
    assert_eq!(destination.count(Op::ChownFd), 0);
    Ok(())
}

#[test]
fn a_lost_create_reply_inside_a_stage_is_adopted() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    for (op, name) in [
        (Op::Mkdir, "dir"),
        (Op::CreateNode, "file.txt"),
        (Op::Symlink, "link"),
    ] {
        let mounts = mounts(Quirks {
            lost_reply: vec![(op, name, Errno::EXIST)],
            ..Quirks::nfs3()
        })?;
        let source = mounts.sources.join("source");
        fs::create_dir_all(source.join("dir"))?;
        fs::write(source.join("dir/inner.txt"), b"inner")?;
        fs::write(source.join("file.txt"), b"contents")?;
        std::os::unix::fs::symlink("file.txt", source.join("link"))?;
        let target = mounts.destination.join("target");

        copy(&source, &target)?;

        assert_eq!(names(&target)?, ["dir", "file.txt", "link"], "{op:?}");
        assert_eq!(fs::read(target.join("dir/inner.txt"))?, b"inner", "{op:?}");
        assert_eq!(fs::read(target.join("file.txt"))?, b"contents", "{op:?}");
        assert_eq!(fs::read_link(target.join("link"))?, Path::new("file.txt"));
        assert_eq!(
            calls_naming(&mounts.destination_scope, op, name),
            1,
            "{op:?}"
        );
    }
    Ok(())
}

#[test]
fn files_are_claimed_where_nodes_are_refused() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let mounts = mounts(Quirks {
        node_create: Support::Rejected(Errno::PERM),
        ..Quirks::nfs3()
    })?;
    let source = mounts.sources.join("source");
    fs::create_dir(&source)?;
    for index in 0..3 {
        fs::write(source.join(format!("{index}.txt")), index.to_string())?;
    }
    let target = mounts.destination.join("target");

    copy(&source, &target)?;

    for index in 0..3 {
        assert_eq!(
            fs::read_to_string(target.join(format!("{index}.txt")))?,
            index.to_string()
        );
    }
    let scope = &mounts.destination_scope;
    assert_eq!(scope.count(Op::CreateNode), 1, "learned once per copy");
    assert_eq!(scope.count(Op::CreateReadonly), 3);
    Ok(())
}

#[test]
fn a_claimed_name_swapped_for_a_pipe_fails_without_waiting() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let (sources, destination) = (root.path().join("sources"), root.path().join("destination"));
    fs::create_dir_all(sources.join("source"))?;
    fs::create_dir(&destination)?;
    fs::write(sources.join("source/file.bin"), b"contents")?;
    let pipe = Arc::new(std::sync::Mutex::new(None));
    let racer: Racer = {
        let (destination, pipe) = (destination.clone(), pipe.clone());
        Box::new(move || {
            let stage = fs::read_dir(&destination)
                .and_then(|mut entries| entries.next().expect("the stage"))
                .expect("list the destination")
                .path();
            let claimed = stage.join("file.bin");
            fs::remove_file(&claimed).expect("remove the claim");
            rustix::fs::mknodat(
                rustix::fs::CWD,
                &claimed,
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::from_raw_mode(0o600),
                0,
            )
            .expect("make a pipe");
            *pipe.lock().expect("pipe") = Some(claimed);
        })
    };
    let _sources = FsQuirks::install(&sources, Quirks::default());
    let _destination = FsQuirks::install(
        &destination,
        Quirks {
            before: vec![(Op::OpenWrite, "file.bin", 0, racer)],
            ..Quirks::nfs3()
        },
    );
    // Without a reader a blocking open waits for ever, so a reader comes
    // after a while to let the test end.
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let rescue = thread::spawn(move || {
        if finished.recv_timeout(Duration::from_secs(5)).is_ok() {
            return false;
        }
        if let Some(pipe) = pipe.lock().expect("pipe").clone() {
            let _reader = rustix::fs::open(
                &pipe,
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::empty(),
            );
        }
        true
    });

    let result = copy(&sources.join("source"), &destination.join("target"));
    let _ = done.send(());

    assert!(!rescue.join().expect("rescue"), "the copy waited on the pipe");
    assert!(result.is_err(), "{result:?}");
    assert_eq!(names(&destination)?, Vec::<String>::new());
    Ok(())
}

#[test]
fn a_cancelled_copy_on_nfs_leaves_no_stage() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let cancellable = gio::Cancellable::new();
    let cancel = cancellable.clone();
    let mounts = mounts(Quirks {
        silly_rename: true,
        before: vec![(Op::Write, "*", 0, Box::new(move || cancel.cancel()))],
        ..Quirks::nfs3()
    })?;
    let source = mounts.sources.join("source");
    fs::create_dir_all(source.join("nested"))?;
    for index in 0..8 {
        fs::write(source.join(format!("{index}.bin")), vec![0x5a; 256 * 1024])?;
        fs::write(
            source.join(format!("nested/{index}.bin")),
            vec![0xa5; 256 * 1024],
        )?;
    }

    let result = glib::MainContext::default().block_on(copy_new_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(mounts.destination.join("target")),
        cancellable,
    ));

    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)),
        "{result:?}"
    );
    assert_eq!(names(&mounts.destination)?, Vec::<String>::new());
    Ok(())
}

/// The files beneath `root`, by path relative to it.
fn files_beneath(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let path = relative.join(entry?.file_name());
        if fs::symlink_metadata(root.join(&path))?.is_dir() {
            files_beneath(root, &path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[test]
fn a_cancelled_merge_on_nfs_leaves_no_partial_file_at_a_real_name() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let cancellable = gio::Cancellable::new();
    let cancel = cancellable.clone();
    let mounts = mounts(Quirks {
        silly_rename: true,
        before: vec![(Op::Write, "*", 0, Box::new(move || cancel.cancel()))],
        ..Quirks::nfs3()
    })?;
    let source = mounts.sources.join("source");
    fs::create_dir_all(source.join("new"))?;
    for index in 0..8 {
        fs::write(source.join(format!("{index}.bin")), vec![0x5a; 256 * 1024])?;
        fs::write(
            source.join(format!("new/{index}.bin")),
            vec![0xa5; 256 * 1024],
        )?;
    }
    let live = mounts.destination.join("live");
    fs::create_dir(&live)?;
    fs::write(live.join("stays.txt"), b"keep me")?;

    let result = glib::MainContext::default().block_on(copy_recursively(
        gio::File::for_path(&source),
        gio::File::for_path(&live),
        true,
        cancellable,
    ));

    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.matches(gio::IOErrorEnum::Cancelled)),
        "{result:?}"
    );
    assert_eq!(fs::read(live.join("stays.txt"))?, b"keep me");
    let mut merged = Vec::new();
    files_beneath(&live, Path::new(""), &mut merged)?;
    for path in merged.iter().filter(|path| *path != Path::new("stays.txt")) {
        let (copied, original) = (fs::read(live.join(path))?, fs::read(source.join(path))?);
        assert!(
            copied == original,
            "{path:?} has {} of {} bytes",
            copied.len(),
            original.len()
        );
    }
    Ok(())
}

#[test]
fn a_folder_is_copied_in_place_where_nothing_can_place_it() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    fs::create_dir_all(source.join("nested"))?;
    fs::write(source.join("nested/file.txt"), b"contents")?;
    let destination = root.path().join("destination");
    fs::create_dir(&destination)?;
    let scope = FsQuirks::install(&destination, Quirks::without_folder_placement());
    learn_mount_capability(scope.key(), |capabilities| {
        capabilities.no_replace = Some(false);
        capabilities.hard_links = Some(true);
        capabilities.rename_over_empty_directory = Some(false);
    });

    copy(&source, &destination.join("target"))?;

    assert_eq!(names(&destination)?, ["target"]);
    assert_eq!(
        fs::read(destination.join("target/nested/file.txt"))?,
        b"contents"
    );
    assert!(!scope.used_name(".strata-*"), "{:?}", scope.calls());
    Ok(())
}

#[test]
fn a_failed_copy_in_place_removes_only_what_it_made() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    fs::create_dir_all(source.join("nested"))?;
    fs::write(source.join("a.txt"), b"a")?;
    fs::write(source.join("nested/b.txt"), b"b")?;
    std::os::unix::fs::symlink("a.txt", source.join("link"))?;
    let destination = root.path().join("destination");
    fs::create_dir(&destination)?;
    let target = destination.join("target");
    let user = target.join("user.txt");
    let racer: Racer = Box::new(move || fs::write(&user, b"user data").expect("write"));
    let scope = FsQuirks::install(
        &destination,
        Quirks {
            before: vec![(Op::Close, "*", 0, racer)],
            fail: vec![(Op::Close, "*", 0, Errno::IO)],
            ..Quirks::without_folder_placement()
        },
    );
    learn_mount_capability(scope.key(), |capabilities| {
        capabilities.no_replace = Some(false);
        capabilities.hard_links = Some(true);
        capabilities.rename_over_empty_directory = Some(false);
    });

    let result = copy(&source, &target);

    assert!(result.is_err(), "{result:?}");
    assert_eq!(names(&target)?, ["user.txt"]);
    assert_eq!(fs::read(target.join("user.txt"))?, b"user data");
    Ok(())
}

#[test]
fn a_stage_with_read_only_folders_is_removed_when_it_cannot_be_published()
-> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    fs::create_dir_all(source.join("sub"))?;
    fs::write(source.join("sub/file.txt"), b"contents")?;
    set_mode(&source.join("sub"), 0o555)?;
    set_mode(&source, 0o555)?;
    let destination = root.path().join("destination");
    fs::create_dir(&destination)?;
    let taken = destination.join("target");
    let racer: Racer = Box::new(move || fs::create_dir(&taken).expect("take the name"));
    let _scope = FsQuirks::install(
        &destination,
        Quirks {
            before: vec![(Op::Rename, ".strata-copy-*", 0, racer)],
            ..Quirks::default()
        },
    );

    let result = copy(&source, &destination.join("target"));

    unlock(&source)?;
    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.matches(gio::IOErrorEnum::Exists)),
        "{result:?}"
    );
    assert_eq!(names(&destination)?, ["target"]);
    assert_eq!(names(&destination.join("target"))?, Vec::<String>::new());
    Ok(())
}

#[test]
fn a_copied_folder_keeps_its_hard_links_where_the_destination_can() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let in_place: fn(&mut MountCapabilities) = |capabilities| {
        capabilities.no_replace = Some(false);
        capabilities.hard_links = Some(true);
        capabilities.rename_over_empty_directory = Some(false);
    };
    let rows: [(&str, Preset, Option<fn(&mut MountCapabilities)>, bool); 6] = [
        ("tmpfs", Quirks::default, None, true),
        ("ext4", Quirks::ext4, None, true),
        ("claimed names", Quirks::nfs3, None, true),
        (
            "in place",
            Quirks::without_folder_placement,
            Some(in_place),
            true,
        ),
        ("vfat", Quirks::vfat, None, false),
        ("sshfs", Quirks::sshfs, None, false),
    ];
    for (row, preset, learned, linked) in rows {
        let mounts = mounts(preset())?;
        if let Some(learned) = learned {
            learn_mount_capability(mounts.destination_scope.key(), learned);
        }
        let source = mounts.sources.join("source");
        fs::create_dir_all(source.join("sub"))?;
        fs::write(source.join("a"), b"linked")?;
        fs::hard_link(source.join("a"), source.join("sub/b"))?;
        fs::write(source.join("c"), b"single")?;
        fs::hard_link(source.join("c"), mounts.sources.join("outside"))?;
        let folders = [("sub", 1_000_000_000), ("", 1_100_000_000)];
        for (folder, seconds) in folders {
            set_mtime(&source.join(folder), seconds)?;
        }
        let target = mounts.destination.join("target");

        copy(&source, &target)?;

        let [a, b, c] = ["a", "sub/b", "c"].map(|name| fs::metadata(target.join(name)));
        let (a, b, c) = (a?, b?, c?);
        assert_eq!(a.ino() == b.ino(), linked, "{row}");
        let links = if linked { 2 } else { 1 };
        assert_eq!(
            (a.nlink(), b.nlink(), c.nlink()),
            (links, links, 1),
            "{row}"
        );
        assert_ne!(c.ino(), a.ino(), "{row}");
        assert_eq!(fs::read(target.join("a"))?, b"linked", "{row}");
        assert_eq!(fs::read(target.join("sub/b"))?, b"linked", "{row}");
        assert_eq!(fs::read(target.join("c"))?, b"single", "{row}");
        for (folder, seconds) in folders {
            let mtime = fs::metadata(target.join(folder))?.mtime();
            assert_eq!(mtime, seconds, "{row}: {folder:?}");
        }
    }
    Ok(())
}

#[test]
fn probing_for_hard_links_leaves_nothing_in_the_copy() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    // The probe's link is made, but its reply is lost, so it tries another name.
    let mounts = mounts(Quirks {
        lost_reply: vec![(Op::Link, ".strata-probe-*", Errno::EXIST)],
        ..Quirks::nfs3()
    })?;
    let source = mounts.sources.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("a"), b"linked")?;
    fs::hard_link(source.join("a"), source.join("b"))?;
    let target = mounts.destination.join("target");

    copy(&source, &target)?;

    assert_eq!(names(&target)?, ["a", "b"]);
    assert_eq!(
        fs::metadata(target.join("a"))?.ino(),
        fs::metadata(target.join("b"))?.ino()
    );
    Ok(())
}

#[test]
fn a_refused_link_is_copied_from_the_first_copy_and_ends_linking() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let mounts = mounts(Quirks {
        fail: vec![(Op::Link, "*", 0, Errno::MLINK)],
        ..Quirks::default()
    })?;
    learn_mount_capability(mounts.destination_scope.key(), |capabilities| {
        capabilities.hard_links = Some(true);
    });
    let source = mounts.sources.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("a"), b"linked")?;
    for name in ["b", "d"] {
        fs::hard_link(source.join("a"), source.join(name))?;
    }
    let target = mounts.destination.join("target");

    copy(&source, &target)?;

    let mut inodes = HashSet::new();
    for name in ["a", "b", "d"] {
        let metadata = fs::metadata(target.join(name))?;
        assert_eq!(metadata.nlink(), 1, "{name}");
        inodes.insert(metadata.ino());
        assert_eq!(fs::read(target.join(name))?, b"linked", "{name}");
    }
    assert_eq!(inodes.len(), 3);
    assert_eq!(mounts.destination_scope.count(Op::Link), 1);
    let read = mounts
        .source_scope
        .calls()
        .into_iter()
        .filter(|call| call.op == Op::Read)
        .flat_map(|call| call.names)
        .collect::<HashSet<_>>();
    assert_eq!(read.len(), 1, "the source is read once: {read:?}");
    Ok(())
}
