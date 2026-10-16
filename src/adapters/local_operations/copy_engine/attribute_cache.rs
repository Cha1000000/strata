// SPDX-License-Identifier: MIT

//! The SMB client answers stats from attributes it cached, whose change time
//! can predate the one the server sets once the client's deferred close of a
//! file it wrote reaches it, until they expire or a stat forces revalidation.
//! So a move records entries on a share this process wrote a file to that
//! recently with forced stats.

use std::{
    os::fd::BorrowedFd,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use super::{Mount, lock, manifest::now};
use crate::adapters::{local_operations::fs_ops::FsOps, volume::MountKey};

/// When each SMB mount's cached attributes of the files this process wrote
/// there last expire.
static EXPIRING: Mutex<Vec<(MountKey, SystemTime)>> = Mutex::new(Vec::new());

/// The latest of those times, so other mounts skip the lookup.
static LATEST: AtomicU64 = AtomicU64::new(0);

fn nanoseconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
    })
}

pub(super) fn wrote_file(mount: &Mount) {
    let Some(cached) = mount.attribute_cache else {
        return;
    };
    let expires = now() + cached;
    let mut expiring = lock(&EXPIRING);
    match expiring.iter_mut().find(|(key, _)| *key == mount.key) {
        Some((_, until)) => *until = (*until).max(expires),
        None => expiring.push((mount.key, expires)),
    }
    LATEST.fetch_max(nanoseconds(expires), Ordering::Relaxed);
}

/// Whether a stat of an entry in `dir` may answer from attributes cached
/// before this process's last file there was closed on the server.
pub(super) fn stale(fs: &dyn FsOps, dir: BorrowedFd<'_>) -> bool {
    if nanoseconds(now()) >= LATEST.load(Ordering::Relaxed) {
        return false;
    }
    let mount = Mount::new(fs, dir);
    if mount.attribute_cache.is_none() {
        return false;
    }
    let now = now();
    let mut expiring = lock(&EXPIRING);
    expiring.retain(|&(_, until)| until > now);
    expiring.iter().any(|&(key, _)| key == mount.key)
}

#[cfg(test)]
pub(super) fn forget() {
    lock(&EXPIRING).clear();
    LATEST.store(0, Ordering::Relaxed);
}
