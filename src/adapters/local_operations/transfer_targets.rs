// SPDX-License-Identifier: MIT

//! Where a paste would put each of its items, which the conflict prompt
//! shows before the paste starts.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
};

use gtk::{gio, glib, prelude::*};

use super::{
    copy_engine::stage::collision_key,
    fs_ops::{Entry, fs},
    mount_profile::NamePolicy,
    open_local_parent_directory, paste_targets, placement_reason, run_blocking,
    transfer_source_name, was_cancelled,
};
use crate::{adapters::gio_file_for_location, model::Location, services::NameMode};

/// Where a paste puts one of its items.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TransferTarget {
    /// The name the item takes there; `None` for an item without one.
    pub name: Option<OsString>,
    /// Whether an item already has that name there, and if so, whether both
    /// are folders, whose contents a merge can combine.
    pub collision: Option<Collision>,
    /// Whether names inside the item change to suit the destination.
    pub fixes_inside: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Collision {
    pub mergeable: bool,
}

/// Where a paste of `sources` into `destination` would put each, by the
/// names the paste plans for them, and what is already there. Nothing
/// collides with a link the paste leaves out, as the destination is known
/// not to hold links. Fails when cancelled, or when no name is left.
pub(crate) async fn transfer_targets(
    destination: &Location,
    sources: &[Location],
    move_sources: bool,
    cancellable: &gio::Cancellable,
) -> Result<Vec<TransferTarget>, glib::Error> {
    let destination = gio_file_for_location(destination);
    let sources = sources
        .iter()
        .map(gio_file_for_location)
        .collect::<Vec<_>>();
    let names = NamePolicy::new(NameMode::Fix);
    let pasted_into = names.destination(&destination, &sources, cancellable).await;
    let cancellable = cancellable.clone();
    run_blocking(move || {
        let (kinds, filesystems) = item_kinds(&sources, &cancellable)?;
        let profile = pasted_into.profile;
        let rules = names.applied(profile.names, profile.strictness);
        let crossing = pasted_into.fixing(&filesystems);
        let own_names = sources.iter().map(transfer_source_name).collect::<Vec<_>>();
        let planned = paste_targets(&own_names, &rules, &crossing, &kinds).map_err(|error| {
            glib::Error::new(gio::IOErrorEnum::Exists, &placement_reason(error))
        })?;
        let left_out = |index: usize| {
            kinds[index] == Some(gio::FileType::SymbolicLink)
                && profile.symlinks == Some(false)
                && (crossing[index] || !move_sources)
        };
        let looked_up = planned
            .iter()
            .enumerate()
            .map(|(index, planned)| {
                let name = planned.as_ref()?.target.as_os_str();
                let duplicate = sources[index].equal(&destination.child(name));
                (!duplicate && !left_out(index)).then_some(name)
            })
            .collect::<Vec<_>>();
        let existing = existing_folders(&destination, &looked_up, &cancellable)?;
        Ok(planned
            .into_iter()
            .zip(existing)
            .enumerate()
            .map(|(index, (planned, existing))| {
                let is_directory = kinds[index] == Some(gio::FileType::Directory);
                TransferTarget {
                    name: planned.map(|planned| planned.target),
                    collision: existing.map(|folder| Collision {
                        mergeable: is_directory && folder,
                    }),
                    fixes_inside: is_directory && crossing[index] && rules.changes_characters(),
                }
            })
            .collect())
    })
    .await
}

/// Each item's kind, not following a link, and GIO's id of the filesystem
/// of each that is not native, as far as they could be read.
fn item_kinds(
    items: &[gio::File],
    cancellable: &gio::Cancellable,
) -> Result<(Vec<Option<gio::FileType>>, Vec<Option<glib::GString>>), glib::Error> {
    let mut kinds = Vec::with_capacity(items.len());
    let mut filesystems = Vec::with_capacity(items.len());
    for item in items {
        let attributes = if item.is_native() {
            gio::FILE_ATTRIBUTE_STANDARD_TYPE.to_string()
        } else {
            format!(
                "{},{}",
                gio::FILE_ATTRIBUTE_STANDARD_TYPE,
                gio::FILE_ATTRIBUTE_ID_FILESYSTEM
            )
        };
        match item.query_info(
            &attributes,
            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Some(cancellable),
        ) {
            Ok(info) => {
                kinds.push(Some(info.file_type()));
                filesystems.push(info.attribute_string(gio::FILE_ATTRIBUTE_ID_FILESYSTEM));
            }
            Err(error) if was_cancelled(&error) => return Err(error),
            Err(_) => {
                kinds.push(None);
                filesystems.push(None);
            }
        }
    }
    Ok((kinds, filesystems))
}

/// For each of `names`, whether an item has it in `destination` and is a
/// folder. In a native folder, only names its listing may hold are looked
/// up.
fn existing_folders(
    destination: &gio::File,
    names: &[Option<&OsStr>],
    cancellable: &gio::Cancellable,
) -> Result<Vec<Option<bool>>, glib::Error> {
    let Some(path) = destination.path().filter(|_| destination.is_native()) else {
        return names
            .iter()
            .map(|name| {
                let Some(name) = name else {
                    return Ok(None);
                };
                match destination.child(name).query_info(
                    gio::FILE_ATTRIBUTE_STANDARD_TYPE,
                    gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                    Some(cancellable),
                ) {
                    Ok(info) => Ok(Some(info.file_type() == gio::FileType::Directory)),
                    Err(error) if was_cancelled(&error) => Err(error),
                    Err(_) => Ok(None),
                }
            })
            .collect();
    };
    let fs = fs();
    let folder = names
        .iter()
        .any(Option::is_some)
        .then(|| open_local_parent_directory(&path).ok())
        .flatten();
    let Some(folder) = folder else {
        return Ok(vec![None; names.len()]);
    };
    let listed = fs
        .names(Entry::new(&folder, OsStr::new(".")))
        .ok()
        .map(|listed| {
            listed
                .iter()
                .map(|name| collision_key(name))
                .collect::<HashSet<_>>()
        });
    Ok(names
        .iter()
        .map(|name| {
            let name = (*name)?;
            if listed
                .as_ref()
                .is_some_and(|listed| !listed.contains(&collision_key(name)))
            {
                return None;
            }
            fs.stat(Entry::new(&folder, name), false)
                .ok()
                .map(|stat| stat.is_directory())
        })
        .collect())
}
