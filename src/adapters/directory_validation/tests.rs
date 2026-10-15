// SPDX-License-Identifier: MIT

use std::{
    fs,
    os::unix::{ffi::OsStringExt, fs::symlink},
};

use super::*;

#[test]
fn foreign_folder_validation_preserves_symlinks_and_native_names_and_rejects_mixed_or_missing_sources()
 {
    let directory = tempfile::tempdir().expect("fixture");
    let folder = directory
        .path()
        .join(std::ffi::OsString::from_vec(b"folder-\xff".to_vec()));
    let link = directory.path().join("alias");
    let file = directory.path().join("file");
    fs::create_dir(&folder).expect("folder");
    symlink(&folder, &link).expect("directory symlink");
    fs::write(&file, b"file").expect("file");
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| {
            context.block_on(async {
                let folder = Location::local(folder);
                let link = Location::local(link);
                let file = Location::local(file);
                for (locations, expected) in [
                    (vec![], false),
                    (vec![folder.clone(), link.clone()], true),
                    (vec![file.clone()], false),
                    (vec![folder, file], false),
                    (
                        vec![Location::local(directory.path().join("missing"))],
                        false,
                    ),
                ] {
                    assert_eq!(all_directories(&locations).await, expected, "{locations:?}");
                }
                fs::remove_file(directory.path().join("alias")).expect("remove alias");
                symlink(
                    directory.path().join("missing"),
                    directory.path().join("alias"),
                )
                .expect("broken link");
                assert!(!all_directories(&[link]).await);
            })
        })
        .expect("private main context");
}
