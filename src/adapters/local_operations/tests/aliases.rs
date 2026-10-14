// SPDX-License-Identifier: MIT

use std::os::unix::fs::MetadataExt;

use super::*;
use crate::services::{RenameCause, RenamedItem};

fn copy_renaming(
    source: &Path,
    target: &Path,
    workers: Option<usize>,
) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let relative = |location: &Location| {
        location
            .native_path()
            .and_then(|path| path.strip_prefix(target).ok())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Ok(copy_reporting(source, target, workers)?
        .renamed
        .into_iter()
        .map(
            |RenamedItem {
                 requested,
                 saved_as,
                 cause,
             }| {
                assert_eq!(cause, RenameCause::Alias);
                (relative(&requested), relative(&saved_as))
            },
        )
        .collect())
}

#[test]
fn case_aliases_inside_a_copied_folder_are_numbered_and_reported() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    // Names are claimed in byte order where they may collide, so the later
    // one is numbered; elsewhere the one made last is.
    let names_never_collide = || Quirks {
        case_insensitive: true,
        ..Quirks::ext4()
    };
    let rows: [(&str, Preset, &str, &str); 2] = [
        ("claimed", Quirks::vfat, "makefile", "makefile (2)"),
        ("made", names_never_collide, "Makefile", "Makefile (2)"),
    ];
    for (row, preset, aliased, saved_as) in rows {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir_all(source.join("Docs"))?;
        fs::create_dir_all(source.join("docs"))?;
        fs::create_dir(&destination)?;
        fs::write(source.join("Docs/upper.txt"), b"upper")?;
        fs::write(source.join("docs/lower.txt"), b"lower")?;
        for name in ["Makefile", "makefile", "makefile (1)"] {
            fs::write(source.join(name), name)?;
        }
        std::os::unix::fs::symlink("upper", source.join("Link"))?;
        std::os::unix::fs::symlink("lower", source.join("link"))?;
        let _sources = FsQuirks::install(&source, Quirks::default());
        let _destination = FsQuirks::install(&destination, preset());
        let target = destination.join("copy");

        let renamed = copy_renaming(&source, &target, Some(1))?;

        let mut expected = vec![
            ("docs".to_owned(), "docs (1)".to_owned()),
            ("link".to_owned(), "link (1)".to_owned()),
            (aliased.to_owned(), saved_as.to_owned()),
        ];
        expected.sort();
        let mut renamed = renamed;
        renamed.sort();
        assert_eq!(renamed, expected, "{row}");
        let other = if aliased == "makefile" {
            "Makefile"
        } else {
            "makefile"
        };
        let mut expected = vec!["Docs", "Link", "docs (1)", "link (1)", "makefile (1)"];
        expected.extend([other, saved_as]);
        expected.sort_unstable();
        assert_eq!(names(&target)?, expected, "{row}");
        assert_eq!(fs::read_to_string(target.join(saved_as))?, aliased, "{row}");
        assert_eq!(fs::read_to_string(target.join(other))?, other, "{row}");
        assert_eq!(
            fs::read_to_string(target.join("makefile (1)"))?,
            "makefile (1)",
            "{row}: the sibling keeps its own name"
        );
        assert_eq!(fs::read(target.join("Docs/upper.txt"))?, b"upper", "{row}");
        assert_eq!(
            fs::read(target.join("docs (1)/lower.txt"))?,
            b"lower",
            "{row}"
        );
        assert_eq!(fs::read_link(target.join("Link"))?, Path::new("upper"));
        assert_eq!(fs::read_link(target.join("link (1)"))?, Path::new("lower"));
    }
    Ok(())
}

#[test]
fn a_linked_name_that_aliases_another_is_numbered_and_reported() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(&source)?;
    fs::create_dir(&destination)?;
    fs::write(source.join("a"), b"linked")?;
    fs::hard_link(source.join("a"), source.join("B"))?;
    fs::write(source.join("b"), b"other")?;
    let _sources = FsQuirks::install(&source, Quirks::default());
    let _destination = FsQuirks::install(
        &destination,
        Quirks {
            case_insensitive: true,
            ..Quirks::ext4()
        },
    );
    let target = destination.join("copy");

    // One worker takes the folder's files last first, so `B` is linked
    // after `b` is made.
    let renamed = copy_renaming(&source, &target, Some(1))?;

    assert_eq!(renamed, [("B".to_owned(), "B (1)".to_owned())]);
    assert_eq!(names(&target)?, ["B (1)", "a", "b"]);
    assert_eq!(
        fs::metadata(target.join("B (1)"))?.ino(),
        fs::metadata(target.join("a"))?.ino()
    );
    assert_eq!(fs::read(target.join("b"))?, b"other");
    Ok(())
}

#[test]
fn two_names_are_never_taken_for_one_where_the_listing_changes_their_case()
-> Result<(), Box<dyn Error>> {
    use super::fs_quirks::{Op, Racer, renumbered};
    use crate::services::{PasteItem, PasteRequest};
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    // Samba with `preserve case = no` and vfat with `shortname=lower` list
    // `Makefile` as `makefile` once it is made; a racer renames it so. Where
    // inode numbers are not kept, the entry then reports another number.
    let fuse = || Quirks {
        mount_type: Some(("fuse.ciopfs", "rw")),
        case_insensitive: true,
        ..Quirks::default()
    };
    let rows: [(&str, Preset, bool, bool); 3] = [
        ("copy", Quirks::vfat, false, false),
        ("move", Quirks::vfat, true, false),
        ("unstable inodes", fuse, false, true),
    ];
    for (row, preset, moving, renumber) in rows {
        let root = tempfile::tempdir()?;
        let sources = root.path().join("sources");
        let folder = sources.join("folder");
        let destination = root.path().join("destination");
        fs::create_dir_all(&folder)?;
        fs::create_dir(&destination)?;
        fs::write(folder.join("Makefile"), b"upper")?;
        fs::write(folder.join("makefile"), b"lower")?;
        let live = destination.clone();
        let racer: Racer = Box::new(move || {
            let stage = names(&live)
                .expect("destination")
                .into_iter()
                .find(|name| name.starts_with(".strata-copy-"))
                .expect("stage");
            let lowered = live.join(stage).join("makefile");
            fs::rename(lowered.with_file_name("Makefile"), &lowered).expect("lower the name");
            if renumber {
                renumbered(&lowered);
            }
        });
        let _sources = FsQuirks::install(&sources, Quirks::default());
        let _destination = FsQuirks::install(
            &destination,
            Quirks {
                before: vec![(Op::CreateNode, "makefile", 0, racer)],
                ..preset()
            },
        );

        let events = run_paste(PasteRequest {
            id: OperationRequestId(1),
            destination: Location::local(&destination),
            items: vec![PasteItem {
                source: Location::local(&folder),
                conflict: TransferConflict::FailIfExists,
            }],
            move_sources: moving,
        });

        let copied = destination.join("folder");
        let renamed = events
            .iter()
            .filter_map(|event| match event {
                OperationEvent::ItemsReported { report, .. } => Some(report.renamed.clone()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(
            renamed,
            [RenamedItem {
                requested: Location::local(copied.join("makefile")),
                saved_as: Location::local(copied.join("makefile (1)")),
                cause: RenameCause::Alias,
            }],
            "{row}"
        );
        assert_eq!(names(&copied)?, ["makefile", "makefile (1)"], "{row}");
        assert_eq!(fs::read(copied.join("makefile"))?, b"upper", "{row}");
        assert_eq!(fs::read(copied.join("makefile (1)"))?, b"lower", "{row}");
        assert_eq!(folder.exists(), !moving, "{row}");
    }
    Ok(())
}

#[test]
fn names_a_fat_target_takes_for_one_are_numbered_and_reported() -> Result<(), Box<dyn Error>> {
    let _serial = ASYNC_MAIN_CONTEXT_DEFAULT.lock()?;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    fs::create_dir_all(&source)?;
    fs::create_dir(&destination)?;
    // `a:b` is saved as `a_b`, which the real `a_b` asks for too.
    for name in ["Makefile", "a:b", "a_b", "makefile"] {
        fs::write(source.join(name), name)?;
    }
    let _sources = FsQuirks::install(&source, Quirks::default());
    let _destination = FsQuirks::install(&destination, Quirks::vfat());
    let target = destination.join("copy");
    let options = super::super::copy_engine::Options {
        fat_family: true,
        ..super::super::copy_engine::Options::default()
    };

    let (copied, report) = reporting(
        &source,
        super::super::copy_engine::Destination::New(target.clone(), Taken::Fail),
        options,
    );

    copied?;
    let renamed = report
        .renamed
        .into_iter()
        .map(|item| (item.requested, item.saved_as, item.cause))
        .collect::<Vec<_>>();
    let alias = |requested: &str, saved_as: &str| {
        (
            Location::local(target.join(requested)),
            Location::local(target.join(saved_as)),
            RenameCause::Alias,
        )
    };
    assert_eq!(
        renamed,
        [alias("a_b", "a_b (1)"), alias("makefile", "makefile (1)")]
    );
    assert_eq!(
        names(&target)?,
        ["Makefile", "a_b", "a_b (1)", "makefile (1)"]
    );
    assert_eq!(fs::read(target.join("a_b"))?, b"a:b");
    assert_eq!(fs::read(target.join("a_b (1)"))?, b"a_b");
    assert_eq!(fs::read(target.join("makefile (1)"))?, b"makefile");
    Ok(())
}
