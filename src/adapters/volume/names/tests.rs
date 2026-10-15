// SPDX-License-Identifier: MIT

use super::{NameProfile, gvfs_profile};

#[test]
fn gvfs_backends_without_known_rules_learn_them_and_have_no_limit() {
    assert_eq!(gvfs_profile("smb-share"), gvfs_profile("smb"));
    for backend in ["dav", "ftp", "google-drive"] {
        assert_eq!(
            gvfs_profile(backend),
            NameProfile::posix(None).learning(),
            "{backend}"
        );
    }
}
