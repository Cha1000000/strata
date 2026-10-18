// SPDX-License-Identifier: MIT

//! Trash that only counts a local item as trashed once it has left its name:
//! GLib 2.86 to 2.88.4 can fail without an error and move nothing (GLib #4060).

use std::path::Path;

use gtk::{gio, glib, prelude::*};
use rustix::io::Errno;

use super::{
    await_cancellable,
    fs_ops::{Entry, EntryStat, fs},
    io_error,
    mount_profile::known_mount,
    native_fs::{self, Pinned, open_local_parent_directory, os_error_text},
    run_blocking,
};
use crate::services::TrashedOriginal;

const NOT_MOVED_REASON: &str = "Trash reported success, but the item was not moved";

pub(in crate::adapters) async fn trash_entry(
    file: &gio::File,
    cancellable: &gio::Cancellable,
) -> Result<Option<TrashedOriginal>, glib::Error> {
    if let Some(path) = file.path().filter(|_| file.is_native()) {
        let cancellable = cancellable.clone();
        return run_blocking(move || trash_local_blocking(&path, &cancellable).map(Some)).await;
    }
    await_cancellable(file, cancellable, |file, cancellable, result| {
        file.trash_async(glib::Priority::DEFAULT, Some(cancellable), move |output| {
            result.resolve(output)
        });
    })
    .await
    .map(|()| None)
}

pub(super) fn trash_local_blocking(
    path: &Path,
    cancellable: &gio::Cancellable,
) -> Result<TrashedOriginal, glib::Error> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(errno_error(Errno::INVAL));
    };
    let parent = open_local_parent_directory(parent).map_err(io_error)?;
    let at = Entry::new(&parent, name);
    let stat = fs().stat(at, false).map_err(errno_error)?;
    verify_trash(at, path, &stat, cancellable)?;
    Ok(stat.identity.into())
}

/// An item still there afterwards fails as `NotSupported`, so callers fall
/// back as they would without Trash.
pub(super) fn trash_verified(
    at: Entry<'_>,
    path: &Path,
    stat: &EntryStat,
    cancellable: &gio::Cancellable,
) -> Result<(), crate::adapters::trash_support::TrashError> {
    crate::adapters::trash_support::verified_with_cleanup(path, || {
        verify_trash(at, path, stat, cancellable)
    })
}

fn verify_trash(
    at: Entry<'_>,
    path: &Path,
    stat: &EntryStat,
    cancellable: &gio::Cancellable,
) -> Result<(), glib::Error> {
    let traits = known_mount(fs(), at.dir).1.identity;
    let pin = Pinned::at(fs(), at, stat, traits);
    let trashed = fs().trash(path, cancellable)?;
    let still_there = |current: &EntryStat| {
        pin.holds(fs(), current)
            .unwrap_or_else(|| native_fs::unchanged(traits, stat.identity, None, current))
    };
    if trashed
        && !fs()
            .stat(at, false)
            .is_ok_and(|current| still_there(&current))
    {
        return Ok(());
    }
    Err(glib::Error::new(
        gio::IOErrorEnum::NotSupported,
        &crate::i18n::tr(NOT_MOVED_REASON),
    ))
}

pub(super) fn errno_error(error: Errno) -> glib::Error {
    glib::Error::new(
        gio::io_error_from_errno(error.raw_os_error()),
        &os_error_text(error),
    )
}
