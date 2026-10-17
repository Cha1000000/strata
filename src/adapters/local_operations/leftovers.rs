// SPDX-License-Identifier: MIT

//! Files unlinked while open, which network and FUSE clients keep under hidden names until
//! the last close.

use std::{
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime},
};

use rustix::{fs::AtFlags, io::Errno};

use super::{
    copy_engine::now,
    deferred_close::{self, Pause, Wait},
    fs_ops::{Entry, FsOps},
    mount_profile::known_mount,
};
use crate::{adapters::volume::SillyNames, model::hidden_names::foreign_leftover};

/// How long a leftover may outlive the last close on NFS and FUSE: the lab saw 84 ms at
/// most over loopback, and a network adds round trips.
const LINGERS: Duration = Duration::from_secs(2);

#[derive(Clone, Default)]
pub(super) struct Deadline(Arc<OnceLock<SystemTime>>);

#[derive(Clone, Copy, Debug)]
pub(super) enum Refused {
    Os(Errno),
    StillOpen(Errno),
    Stopped(Errno),
    Changed(Errno),
}

impl Refused {
    pub(super) fn errno(self) -> Errno {
        match self {
            Self::Os(errno)
            | Self::StillOpen(errno)
            | Self::Stopped(errno)
            | Self::Changed(errno) => errno,
        }
    }
}

pub(super) fn remove_folder(
    fs: &dyn FsOps,
    at: Entry<'_>,
    deadline: &Deadline,
    stopped: &dyn Fn() -> bool,
    unchanged: &dyn Fn() -> bool,
) -> Result<(), Refused> {
    let mut wait = None;
    let mut at_once = false;
    loop {
        let errno = match fs.unlink(at, AtFlags::REMOVEDIR) {
            Ok(()) => return Ok(()),
            Err(errno) => errno,
        };
        let Some(held) = held_only(fs, at, errno) else {
            return Err(Refused::Os(errno));
        };
        at_once = held.emptied && !at_once;
        if !at_once {
            let wait =
                wait.get_or_insert_with(|| Wait::since(*deadline.0.get_or_init(now), held.budget));
            match wait.pause(stopped) {
                Pause::Retry => {}
                Pause::Spent => return Err(Refused::StillOpen(errno)),
                Pause::Cancelled => return Err(Refused::Stopped(errno)),
            }
        }
        if !unchanged() {
            return Err(Refused::Changed(errno));
        }
    }
}

struct Held {
    emptied: bool,
    budget: Duration,
}

fn held_only(fs: &dyn FsOps, at: Entry<'_>, errno: Errno) -> Option<Held> {
    if !matches!(errno, Errno::NOTEMPTY | Errno::EXIST | Errno::ACCESS) {
        return None;
    }
    let names = fs.names(at).ok()?;
    if names.iter().any(|name| foreign_leftover(name).is_none()) {
        return None;
    }
    let (_, capabilities) = known_mount(fs, at.dir);
    let only_leftovers = match (capabilities.silly_names?, errno) {
        (SillyNames::Renamed(family), Errno::NOTEMPTY | Errno::EXIST) => names
            .iter()
            .all(|name| foreign_leftover(name) == Some(family)),
        (SillyNames::Unlisted, Errno::NOTEMPTY | Errno::EXIST | Errno::ACCESS) => names.is_empty(),
        _ => false,
    };
    only_leftovers.then(|| Held {
        emptied: names.is_empty() && errno != Errno::ACCESS,
        budget: capabilities
            .deferred_close
            .map_or(LINGERS, deferred_close::closing),
    })
}
