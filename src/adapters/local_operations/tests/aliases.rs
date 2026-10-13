// SPDX-License-Identifier: MIT

use std::os::unix::fs::MetadataExt;

use super::*;
use crate::services::{RenameCause, RenamedItem};

/// Copies `source` to `target` with `workers`, and returns the names it
/// reported saved under others, beneath `target`.
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
