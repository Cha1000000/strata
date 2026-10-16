// SPDX-License-Identifier: MIT

//! Top-level items of one paste whose names differ only in case. A
//! case-insensitive destination whose rules do not fold, such as SMB or a
//! casefolded folder, shows that one took another's name only by "exists".

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
};

use gtk::{gio, prelude::*};

use super::{
    copy_engine::stage::collision_key,
    fs_ops::{Entry, fs},
    native_fs::{LocalFileIdentity, open_local_parent_directory},
};

pub(super) struct CaseVariants {
    /// The other items of each one, by index, whose names differ only in case.
    others: HashMap<usize, Vec<usize>>,
    saved: HashMap<usize, LocalFileIdentity>,
}

impl CaseVariants {
    pub(super) fn new(names: impl IntoIterator<Item = (usize, OsString)>) -> Self {
        let mut keyed = HashMap::<OsString, Vec<(usize, OsString)>>::new();
        for (index, name) in names {
            keyed
                .entry(collision_key(&name))
                .or_default()
                .push((index, name));
        }
        let mut others = HashMap::new();
        for items in keyed.values().filter(|items| items.len() > 1) {
            for (index, name) in items {
                let spelled_otherwise = items
                    .iter()
                    .filter(|(_, other)| other != name)
                    .map(|(other, _)| *other)
                    .collect::<Vec<_>>();
                if !spelled_otherwise.is_empty() {
                    others.insert(*index, spelled_otherwise);
                }
            }
        }
        Self {
            others,
            saved: HashMap::new(),
        }
    }

    /// Whether `earlier` comes before `index` and their names differ only in case.
    pub(super) fn follows(&self, index: usize, earlier: usize) -> bool {
        earlier < index
            && self
                .others
                .get(&index)
                .is_some_and(|others| others.contains(&earlier))
    }

    /// Records what `index` was saved as, where a later item may find it.
    pub(super) async fn saved(&mut self, index: usize, target: &gio::File) {
        let precedes = self
            .others
            .get(&index)
            .is_some_and(|others| others.iter().any(|&other| other > index));
        if precedes && let Some(identity) = identity(target).await {
            self.saved.insert(index, identity);
        }
    }

    /// What the earlier items whose names differ from `index`'s only in case
    /// were saved as.
    pub(super) fn earlier(&self, index: usize) -> Vec<LocalFileIdentity> {
        self.others
            .get(&index)
            .into_iter()
            .flatten()
            .filter(|&&other| other < index)
            .filter_map(|other| self.saved.get(other).copied())
            .collect()
    }
}

/// Whether `target` holds one of the items `earlier` names.
pub(super) async fn taken_by(target: &gio::File, earlier: &[LocalFileIdentity]) -> bool {
    !earlier.is_empty()
        && identity(target)
            .await
            .is_some_and(|found| earlier.contains(&found))
}

async fn identity(target: &gio::File) -> Option<LocalFileIdentity> {
    let path = target.path()?;
    gio::spawn_blocking(move || {
        let (parent, name) = (path.parent()?, path.file_name()?);
        let dir = open_local_parent_directory(parent).ok()?;
        fs().stat(Entry::new(&dir, OsStr::new(name)), false)
            .ok()
            .map(|stat| stat.identity)
    })
    .await
    .ok()
    .flatten()
}
