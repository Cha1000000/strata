// SPDX-License-Identifier: MIT

use rustix::io::Errno;

use super::super::fs_ops::{Entry, fs};
use super::super::placement::{PlaceError, RenameFailure, rename_in_place};
use super::fs_quirks::{Op, Support, Trash};
use super::*;
use crate::model::hidden_names::{Leftover, classify};

fn originals_in(folder: &Path) -> Result<(String, Vec<String>), Box<dyn Error>> {
    let found: Vec<_> = names(folder)?
        .into_iter()
        .filter(|name| classify(OsStr::new(name)) == Some(Leftover::Originals))
        .collect();
    let [originals] = found.as_slice() else {
        return Err(format!("one originals folder in {:?}", names(folder)?).into());
    };
    let inside = names(&folder.join(originals))?;
    Ok((originals.clone(), inside))
}

#[test]
fn a_copy_cut_off_by_an_unplug_leaves_only_its_stage() -> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.bin");
    fs::write(&source, b"copied")?;
    let destination = root.path().join("destination");
    fs::create_dir(&destination)?;
    let target = destination.join("copy.bin");
    let _source = FsQuirks::install(root.path(), Quirks::ext4());
    let scope = FsQuirks::install(
        &destination,
        Quirks {
            tmpfile: Support::Rejected(Errno::OPNOTSUPP),
            short_io: Some(2),
            unplugged_after: Some((Op::Write, ".strata-copy-*", 1)),
            ..Quirks::ext4()
        },
    );
    let copy = || {
        glib::MainContext::default().block_on(copy_new_recursively(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            gio::Cancellable::new(),
        ))
    };
    let stat = |dir: &Path, name: &str| -> Result<Result<(), Errno>, Box<dyn Error>> {
        let dir = open_local_parent_directory(dir)?;
        Ok(fs()
            .stat(Entry::new(&dir, OsStr::new(name)), false)
            .map(drop))
    };

    assert!(copy().is_err());
    let left = names(&destination)?;
    let [stage] = left.as_slice() else {
        panic!("only the stage stays: {left:?}");
    };
    assert_eq!(classify(OsStr::new(stage)), Some(Leftover::IncompleteCopy));
    assert_eq!(fs::read(destination.join(stage))?, b"copi");
    let calls = scope.calls();
    let unplugged = calls
        .iter()
        .enumerate()
        .filter(|(_, call)| call.op == Op::Write)
        .nth(1)
        .map(|(index, _)| index)
        .ok_or("the stage's second write")?;
    assert!(
        calls[unplugged + 1..]
            .iter()
            .any(|call| call.names.iter().any(|name| name == stage.as_str())),
        "{calls:?}"
    );
    assert_eq!(stat(&destination, stage)?, Err(Errno::IO));
    assert_eq!(stat(root.path(), "source.bin")?, Ok(()));
    assert!(
        fs().trash(&destination.join(stage), &gio::Cancellable::new())
            .is_err()
    );

    scope.clear_calls();
    assert!(copy().is_err());
    assert!(!scope.calls().is_empty());
    assert_eq!(names(&destination)?, left);

    scope.replug();
    assert_eq!(stat(&destination, stage)?, Ok(()));
    copy()?;
    assert_eq!(fs::read(&target)?, b"copied");
    assert_eq!(names(&destination)?, [stage.as_str(), "copy.bin"]);
    Ok(())
}

#[test]
fn an_unplug_after_setting_aside_keeps_the_original_under_its_name() -> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    // A copy publishes a stage after setting the original aside; a move on its
    // own filesystem renames the source over the name.
    for (quirks, moving) in [(Quirks::nfs3(), false), (Quirks::ext4(), true)] {
        let row = format!("moving: {moving}");
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::write(&source, b"new")?;
        fs::write(&target, b"old")?;
        let scope = FsQuirks::install(
            root.path(),
            Quirks {
                trash: Trash::Unsupported,
                unplugged_after: Some((Op::Rename, "target", 0)),
                ..quirks
            },
        );

        let result = glib::MainContext::default().block_on(replace_local(
            gio::File::for_path(&source),
            gio::File::for_path(&target),
            moving,
            gio::Cancellable::new(),
            None,
        ));

        assert!(result.is_err(), "{row}");
        scope.replug();
        let (originals, inside) = originals_in(root.path())?;
        assert_eq!(inside, ["target"], "{row}");
        assert_eq!(
            fs::read(root.path().join(&originals).join("target"))?,
            b"old"
        );
        assert_eq!(fs::read(&source)?, b"new", "{row}");
        for name in names(root.path())? {
            if name != originals && name != "source" {
                let class = classify(OsStr::new(&name));
                assert_eq!(class, Some(Leftover::IncompleteCopy), "{row}: {name}");
                assert_eq!(fs::read(root.path().join(&name))?, b"new", "{row}: {name}");
            }
        }
    }
    Ok(())
}

#[test]
fn an_unplug_mid_case_only_rename_keeps_the_old_spelling() -> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let root = tempfile::tempdir()?;
    let item = root.path().join("report");
    fs::write(&item, b"data")?;
    // Skips the direct rename, which the old spelling already answers.
    let scope = FsQuirks::install(
        root.path(),
        Quirks {
            case_insensitive: true,
            unplugged_after: Some((Op::Rename, "report", 1)),
            ..Quirks::ext4()
        },
    );
    let parent = open_local_parent_directory(root.path())?;

    let renamed = rename_in_place(
        fs(),
        Entry::new(&parent, OsStr::new("report")),
        Entry::new(&parent, OsStr::new("Report")),
    );

    scope.replug();
    let (originals, inside) = originals_in(root.path())?;
    assert_eq!(names(root.path())?, [originals.as_str()]);
    assert_eq!(inside, ["report"]);
    assert_eq!(
        fs::read(root.path().join(&originals).join("report"))?,
        b"data"
    );
    assert_eq!(
        renamed,
        Err(RenameFailure::Stranded {
            temporary: Path::new(&originals).join("report").into_os_string(),
            error: PlaceError::Unconfirmed,
        })
    );
    Ok(())
}

#[test]
fn an_unplug_after_an_exchange_leaves_no_original_at_a_stage() -> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"new")?;
    fs::write(&target, b"old")?;
    let scope = FsQuirks::install(
        root.path(),
        Quirks {
            trash: Trash::Unsupported,
            unplugged_after: Some((Op::Exchange, "target", 0)),
            ..Quirks::ext4()
        },
    );

    let result = glib::MainContext::default().block_on(replace_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_err());
    scope.replug();
    let (originals, inside) = originals_in(root.path())?;
    assert_eq!(
        names(root.path())?,
        [originals.as_str(), "source", "target"]
    );
    assert_eq!(inside, ["target"]);
    assert_eq!(
        fs::read(root.path().join(&originals).join("target"))?,
        b"old"
    );
    assert_eq!(fs::read(&target)?, b"new");
    Ok(())
}

#[test]
fn a_refused_exchange_sets_the_original_aside_after_moving_the_copy_back()
-> Result<(), Box<dyn Error>> {
    let _serial = serial()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"new")?;
    fs::write(&target, b"old")?;
    // Unplugged once the original is aside: the copy went into a folder for the
    // exchange, came back to its stage name, and the original took its place.
    let scope = FsQuirks::install(
        root.path(),
        Quirks {
            trash: Trash::Unsupported,
            fail: vec![(Op::Exchange, "*", 0, Errno::INVAL)],
            unplugged_after: Some((Op::Rename, "target", 2)),
            ..Quirks::ext4()
        },
    );

    let result = glib::MainContext::default().block_on(replace_local(
        gio::File::for_path(&source),
        gio::File::for_path(&target),
        false,
        gio::Cancellable::new(),
        None,
    ));

    assert!(result.is_err());
    assert_eq!(scope.count(Op::Exchange), 1);
    scope.replug();
    let (originals, inside) = originals_in(root.path())?;
    assert_eq!(inside, ["target"]);
    assert_eq!(
        fs::read(root.path().join(&originals).join("target"))?,
        b"old"
    );
    let left = names(root.path())?;
    let [stage, ..] = left.as_slice() else {
        return Err(format!("the copy stays at its stage: {left:?}").into());
    };
    assert_eq!(classify(OsStr::new(stage)), Some(Leftover::IncompleteCopy));
    assert_eq!(fs::read(root.path().join(stage))?, b"new");
    assert_eq!(left, [stage.as_str(), originals.as_str(), "source"]);
    Ok(())
}
