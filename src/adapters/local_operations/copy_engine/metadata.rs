// SPDX-License-Identifier: MIT

//! A source's owner, xattrs, mode and times, applied best effort in that
//! order: `chown` clears `security.capability` and the special bits, and
//! xattrs go in before a read-only mode would refuse them.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    os::fd::BorrowedFd,
};

use rustix::io::Errno;

use super::{Mount, is_fatal, is_unsupported};
use crate::adapters::{
    local_operations::fs_ops::{EntryStat, FsOps},
    volume::XattrNamespace,
};

/// Never copied: the skip entries of libattr's `xattr.conf`, and ntfs3's
/// names that would set owner and mode behind `fchown` and `fchmod`.
const SKIPPED_XATTRS: [&str; 9] = [
    "security.evm",
    "trusted.SGI_*",
    "xfsroot.*",
    "afs.*",
    "user.Beagle.*",
    "$LXUID",
    "$LXGID",
    "$LXMOD",
    "$LXDEV",
];

fn skipped(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    SKIPPED_XATTRS
        .iter()
        .any(|pattern| match pattern.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix.as_bytes()),
            None => name == pattern.as_bytes(),
        })
}

/// Only device, handle and connection failures stop a metadata step.
fn best_effort(result: Result<(), Errno>) -> Result<(), Errno> {
    match result {
        Err(error) if is_fatal(error) => Err(error),
        _ => Ok(()),
    }
}

/// Within two seconds, the coarsest granularity a mount stores.
fn same_time(left: (i64, u32), right: (i64, u32)) -> bool {
    let nanoseconds = |(seconds, nanoseconds): (i64, u32)| {
        i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds)
    };
    (nanoseconds(left) - nanoseconds(right)).abs() < 2_000_000_000
}

pub(super) struct Snapshot {
    mode: u32,
    uid: u32,
    gid: u32,
    atime: (i64, u32),
    mtime: (i64, u32),
    xattrs: Vec<(OsString, Vec<u8>)>,
}

impl Snapshot {
    pub(super) fn read(
        fs: &dyn FsOps,
        fd: BorrowedFd<'_>,
        stat: &EntryStat,
    ) -> Result<Self, Errno> {
        let names = match fs.xattr_names(fd) {
            Ok(names) => names,
            Err(error) if is_fatal(error) => return Err(error),
            Err(_) => Vec::new(),
        };
        let mut xattrs = Vec::new();
        for name in names.into_iter().filter(|name| !skipped(name)) {
            match fs.get_xattr(fd, &name) {
                Ok(value) => xattrs.push((name, value)),
                Err(error) if is_fatal(error) => return Err(error),
                Err(_) => {}
            }
        }
        Ok(Self {
            mode: stat.mode,
            uid: stat.uid,
            gid: stat.gid,
            atime: stat.atime,
            mtime: stat.mtime,
            xattrs,
        })
    }

    /// `created` is the new entry as made in `parent`; inside a setgid
    /// folder it keeps the folder's group.
    pub(super) fn apply(
        &self,
        fs: &dyn FsOps,
        fd: BorrowedFd<'_>,
        created: &EntryStat,
        parent: &EntryStat,
        mount: &Mount,
        refused_names: &mut HashSet<OsString>,
    ) -> Result<(), Errno> {
        let shared_group = parent.mode & 0o2000 != 0;
        let other_group = self.gid != created.gid;
        let uid = (self.uid != created.uid).then_some(self.uid);
        let gid = (other_group && !shared_group).then_some(self.gid);
        let mut owner_kept = !(other_group && shared_group);
        if uid.is_some() || gid.is_some() {
            match fs.chown_fd(fd, uid, gid) {
                Ok(()) => {}
                Err(error) if is_fatal(error) => return Err(error),
                Err(_) => owner_kept = false,
            }
        }
        let mut refused = mount.capabilities().refused_xattrs;
        for (name, value) in &self.xattrs {
            let namespace = XattrNamespace::of(name);
            if refused.contains(namespace) || refused_names.contains(name) {
                continue;
            }
            match fs.set_xattr(fd, name, value) {
                Ok(()) => {}
                Err(error) if is_fatal(error) => return Err(error),
                Err(error) if is_unsupported(error) => {
                    refused.insert(namespace);
                    mount.learn(|capabilities| capabilities.refused_xattrs.insert(namespace));
                }
                Err(Errno::PERM | Errno::ACCESS | Errno::INVAL) => {
                    refused_names.insert(name.clone());
                }
                Err(_) => {}
            }
        }
        // Special bits would grant another owner's or group's rights.
        let mode = if owner_kept {
            self.mode
        } else {
            self.mode & !0o6000
        };
        best_effort(fs.chmod_fd(fd, mode))?;
        self.apply_times(fs, fd)
    }

    pub(super) fn apply_times(&self, fs: &dyn FsOps, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        best_effort(fs.set_times_fd(fd, self.atime, self.mtime))
    }

    pub(super) fn times_match(&self, current: &EntryStat) -> bool {
        same_time(self.mtime, current.mtime) && same_time(self.atime, current.atime)
    }
}
