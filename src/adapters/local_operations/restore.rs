// SPDX-License-Identifier: MIT

use crate::{adapters::{gio_file_for_location, location_for_file, trash_restore::{RestoreContext, plan_restore_for_location}, trash_support::trash_with_cleanup}, model::Location, services::{KeptCause, KeptItem, MoveRecord, OperationEvent, OperationRequestId, PlacedSnapshot, RenameCause, RenamedItem, TransferReport, TrashedOriginal}};
use gtk::{gio, glib, prelude::*};
use std::{collections::{HashMap, HashSet}, ffi::{OsStr, OsString}, future::Future, io, os::unix::ffi::{OsStrExt, OsStringExt}, path::{Path, PathBuf}, pin::Pin, rc::Rc, sync::Arc};
use super::{fs_ops::{Entry, fs}, placement::PlaceError};

use super::{await_cancellable, cancelled_event, cancelled_local_operation, copy_engine, deletion::local_parent_worker_count, deletion_error_summary, deletion_undone_when_cancelled, io_error, item_error, kept_items, native_fs::{open_local_parent_beneath, open_local_parent_directory, os_error_detail}};

use super::{owed, placement, placement_failure, progress::TransferProgressTracker, rename_native, run_local_fs_step, translated_io_error};

use super::was_cancelled;

pub(super) async fn restore_renamed(record: &MoveRecord, progress: &TransferProgressTracker) {
    let mut renamed = record
        .renamed
        .iter()
        .filter_map(|item| {
            let saved_as = item.saved_as.rebase(&record.current, &record.original)?;
            let requested = item.requested.rebase(&record.current, &record.original)?;
            Some((saved_as, requested))
        })
        .collect::<Vec<_>>();
    // Entries before the folders holding them, whose paths they still use.
    renamed.sort_by_key(|(saved_as, _)| {
        std::cmp::Reverse(saved_as.native_path().map(|path| path.components().count()))
    });
    let mut taken = Vec::new();
    for (saved_as, requested) in renamed {
        let (Some(from), Some(to)) = (saved_as.native_path(), requested.native_path()) else {
            continue;
        };
        match rename_native(from.to_path_buf(), to.to_path_buf()).await {
            Ok(()) => {}
            Err(error) if error.matches(gio::IOErrorEnum::Exists) => taken.push(RenamedItem {
                requested,
                saved_as,
                cause: RenameCause::Taken,
            }),
            Err(error) => {
                tracing::warn!(
                    ?from,
                    ?to,
                    "could not give back a renamed entry its name: {error}"
                );
            }
        }
    }
    progress.report(TransferReport {
        renamed: taken,
        ..TransferReport::default()
    });
}

pub(super) fn could_not_restore(name: &str, error: &str) -> String {
    rust_i18n::t!(
        "Could not restore %{name}: %{error}",
        name = name,
        error = error
    )
    .into_owned()
}

pub(super) async fn move_restore_path(
    source_path: PathBuf,
    target_path: PathBuf,
    allowed_root: PathBuf,
    unlinks: Arc<copy_engine::Unlinks>,
    cancellable: gio::Cancellable,
) -> Result<TransferReport, glib::Error> {
    if cancellable.is_cancelled() {
        return Err(cancelled_local_operation());
    }
    let Some(source_parent_path) = source_path.parent().map(Path::to_path_buf) else {
        return Err(translated_io_error("Cannot restore the filesystem root"));
    };
    let Some(source_name) = source_path.file_name().map(OsStr::to_os_string) else {
        return Err(translated_io_error("Invalid restore source"));
    };
    let Some(target_parent_path) = target_path.parent().map(Path::to_path_buf) else {
        return Err(translated_io_error(
            "The restore destination has no parent directory",
        ));
    };
    let Some(target_name) = target_path.file_name().map(OsStr::to_os_string) else {
        return Err(translated_io_error("Invalid restore destination"));
    };

    let display_name = source_name.to_string_lossy().into_owned();
    let step_source_name = source_name.clone();
    let step_cancellable = cancellable.clone();
    let step_display_name = display_name.clone();
    let (trash_path, barrier_path) = (source_parent_path.clone(), target_parent_path.clone());
    let placed = run_local_fs_step(move || {
        let source_parent = Arc::new(open_local_parent_directory(&source_parent_path)?);
        let target_parent = Arc::new(open_local_parent_beneath(
            &target_parent_path,
            &allowed_root,
        )?);
        let from = Entry::new(&*source_parent, &step_source_name);
        let to = Entry::new(&*target_parent, &target_name);
        let identity = fs()
            .stat(from, false)
            .map_err(|error| could_not_restore(&step_display_name, &os_error_detail(error)))?
            .identity;
        if step_cancellable.is_cancelled() {
            return Ok(None);
        }
        let placed = placement::place(fs(), from, to, Some(identity));
        Ok(Some((source_parent, target_parent, identity, placed)))
    })
    .await?;
    let Some((source_parent, target_parent, source_identity, placed)) = placed else {
        return Err(cancelled_local_operation());
    };
    match placed {
        Ok(_) => return Ok(TransferReport::default()),
        Err(PlaceError::Unsupported) => {}
        Err(PlaceError::CrossDevice) => {
            return Err(io_error(rust_i18n::t!(
                "Could not restore %{name} across volumes",
                name = display_name
            )));
        }
        Err(PlaceError::Exists) => return Err(restore_destination_exists(&display_name)),
        Err(error) => return Err(placement_failure(error, &display_name, could_not_restore)),
    }

    let barrier = copy_engine::barrier(barrier_path).await?;
    let options = copy_engine::Options {
        durable: true,
        ..copy_engine::Options::default()
    };
    let (sources, mut report) = copy_engine::copy_in_place(
        (source_parent, source_name, source_identity),
        (target_parent, target_path),
        options,
        &cancellable,
    )
    .await
    .map_err(|error| {
        // A cancelled copy removed what it made, or failed.
        if was_cancelled(&error) {
            error
        } else if error.matches(gio::IOErrorEnum::Exists) {
            restore_destination_exists(&display_name)
        } else {
            io_error(could_not_restore(
                &display_name,
                &crate::services::gio_error_detail(&error),
            ))
        }
    })?;
    owed::flush(barrier).await.map_err(io_error)?;
    // The copy is complete, so a cancel no longer stops this restore.
    let (kept, deleted) =
        copy_engine::delete_unchanged_sources(sources, unlinks, &gio::Cancellable::new()).await;
    deleted.map_err(|error| {
        io_error(rust_i18n::t!(
            "The item was restored, but its trash copy could not be removed: %{error}",
            error = crate::services::gio_error_detail(&error)
        ))
    })?;
    report.kept_in_trash = kept_items(&trash_path, kept);
    Ok(report)
}

pub(super) fn restore_destination_exists(display_name: &str) -> glib::Error {
    io_error(rust_i18n::t!(
        "Could not restore %{name}: something already exists at the destination",
        name = display_name
    ))
}

pub(super) async fn move_restore(
    source: gio::File,
    target: gio::File,
    allowed_root: PathBuf,
    unlinks: Arc<copy_engine::Unlinks>,
    cancellable: gio::Cancellable,
) -> Result<TransferReport, glib::Error> {
    if cancellable.is_cancelled() {
        return Err(cancelled_local_operation());
    }
    if source.is_native()
        && target.is_native()
        && let (Some(source_path), Some(target_path)) = (source.path(), target.path())
    {
        return move_restore_path(source_path, target_path, allowed_root, unlinks, cancellable)
            .await;
    }
    Err(io_error(crate::i18n::tr(
        "Trash restore requires a local source and destination",
    )))
}

impl TrashedOriginal {
    pub(super) fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    pub(super) async fn matches_path(self, path: PathBuf) -> bool {
        gio::spawn_blocking(move || {
            std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| self == Self::from_metadata(&metadata))
        })
        .await
        .unwrap_or(false)
    }
}

#[derive(Clone)]
pub(super) struct RestoreEntry {
    pub(super) source: Location,
    pub(super) display_name: String,
    pub(super) original_target: Option<Location>,
    pub(super) trash_info: Option<PathBuf>,
    pub(super) confirmed_destination: Option<PathBuf>,
    pub(super) physical_path: Option<PathBuf>,
}

pub(super) async fn trashed_entries_for_originals(
    original_locations: &[Location],
    cancellable: &gio::Cancellable,
    originals: &HashMap<Location, TrashedOriginal>,
) -> Result<Vec<RestoreEntry>, glib::Error> {
    if cancellable.is_cancelled() {
        return Err(cancelled_local_operation());
    }
    let requested = original_locations
        .iter()
        .filter_map(|location| location.native_path().map(Path::to_path_buf))
        .collect::<HashSet<_>>();
    // GVfs can miss an item re-trashed under the same basename after a restore, so prefer the
    // authoritative freedesktop.org metadata for the home trash before consulting trash:///.
    let fallback_requested = requested.clone();
    let fallback_cancellable = cancellable.clone();
    let fallback_originals = originals.clone();
    let mut fallback = gio::spawn_blocking(move || {
        home_trash_entries(
            &fallback_requested,
            &fallback_cancellable,
            &fallback_originals,
        )
    })
    .await
    .map_err(|_| glib::Error::new(gio::IOErrorEnum::Failed, "Trash lookup task failed"))?;
    if cancellable.is_cancelled() {
        return Err(cancelled_local_operation());
    }
    if fallback.len() == requested.len() && requested.len() == original_locations.len() {
        return Ok(original_locations
            .iter()
            .filter_map(|location| location.native_path())
            .filter_map(|path| fallback.remove(path))
            .collect());
    }

    let context = RestoreContext::current().await;
    let trash = gio::File::for_uri("trash:///");
    let enumerator = await_cancellable(&trash, cancellable, |trash, cancellable, result| {
        trash.enumerate_children_async(
            "standard::name,standard::display-name,trash::orig-path,trash::deletion-date",
            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            glib::Priority::DEFAULT,
            Some(cancellable),
            move |value| result.resolve(value),
        );
    })
    .await?;
    let mut newest = HashMap::<PathBuf, (String, Location, String)>::new();
    loop {
        if cancellable.is_cancelled() {
            return Err(cancelled_local_operation());
        }
        let infos = await_cancellable(
            &enumerator,
            cancellable,
            |enumerator, cancellable, result| {
                enumerator.next_files_async(
                    64,
                    glib::Priority::DEFAULT,
                    Some(cancellable),
                    move |value| result.resolve(value),
                );
            },
        )
        .await?;
        if infos.is_empty() {
            break;
        }
        for info in infos {
            let Some(original_path) = info.attribute_byte_string("trash::orig-path") else {
                continue;
            };
            let original_path = PathBuf::from(original_path.as_str());
            if !requested.contains(&original_path) {
                continue;
            }
            let deletion_date = info
                .attribute_string("trash::deletion-date")
                .map(|value| value.to_string())
                .unwrap_or_default();
            let Some(location) = location_for_file(&trash.child(info.name())) else {
                continue;
            };
            let original = Location::local(&original_path);
            if let Some(&expected) = originals.get(&original) {
                let matches = match plan_restore_for_location(
                    &location,
                    Some(&original),
                    None,
                    None,
                    &context,
                )
                .await
                {
                    Ok(plan) => expected.matches_path(plan.source_path).await,
                    Err(_) => false,
                };
                if !matches {
                    continue;
                }
            }
            let candidate = (deletion_date, location, info.display_name().to_string());
            match newest.get(&original_path) {
                Some(current) if current.0 >= candidate.0 => {}
                _ => {
                    newest.insert(original_path, candidate);
                }
            }
        }
    }

    if requested
        .iter()
        .any(|path| !fallback.contains_key(path) && !newest.contains_key(path))
        || requested.len() != original_locations.len()
    {
        return Err(glib::Error::new(
            gio::IOErrorEnum::NotFound,
            &crate::i18n::tr("One or more recently trashed items are no longer available"),
        ));
    }
    Ok(original_locations
        .iter()
        .filter_map(|location| location.native_path())
        .filter_map(|path| {
            fallback.remove(path).or_else(|| {
                newest
                    .remove(path)
                    .map(|(_, source, display_name)| RestoreEntry {
                        source,
                        display_name,
                        original_target: None,
                        trash_info: None,
                        confirmed_destination: None,
                        physical_path: None,
                    })
            })
        })
        .collect())
}

/// Where a successful restore lands: the confirmed destination for a Trash
/// dialog restore, the trashinfo original path for an undo restore.
pub(super) fn restored_destination(entry: &RestoreEntry) -> Location {
    entry
        .confirmed_destination
        .as_ref()
        .map(|path| Location::local(path.clone()))
        .or_else(|| entry.original_target.clone())
        .unwrap_or_else(|| entry.source.clone())
}

pub(super) async fn restore_worker_count(entries: &[RestoreEntry]) -> usize {
    local_parent_worker_count(entries.iter().filter_map(|entry| {
        restored_destination(entry)
            .native_path()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    }))
    .await
}

/// The entry's Trash metadata stays while anything of it does.
pub(super) async fn restore_trash_entry(
    entry: &RestoreEntry,
    context: &RestoreContext,
    affected_locations: &mut HashSet<Location>,
    unlinks: &Arc<copy_engine::Unlinks>,
    cancellable: &gio::Cancellable,
) -> Result<TransferReport, glib::Error> {
    let plan = match plan_restore_for_location(
        &entry.source,
        entry.original_target.as_ref(),
        entry.trash_info.as_deref(),
        entry.physical_path.as_deref(),
        context,
    )
    .await
    {
        Ok(plan)
            if entry
                .confirmed_destination
                .as_ref()
                .is_some_and(|confirmed| plan.destination != *confirmed) =>
        {
            return Err(glib::Error::new(
                gio::IOErrorEnum::Failed,
                &crate::i18n::tr(
                    "The original location changed and no longer matches the confirmed destination",
                ),
            ));
        }
        Ok(plan) => plan,
        Err(error) => {
            return Err(glib::Error::new(
                gio::IOErrorEnum::Failed,
                &error.user_message(),
            ));
        }
    };
    if let Some(parent) = plan.destination.parent() {
        affected_locations.insert(Location::local(parent));
    }
    let source = gio::File::for_path(&plan.source_path);
    let target = gio::File::for_path(&plan.destination);
    let report = move_restore(
        source,
        target,
        plan.allowed_root,
        unlinks.clone(),
        cancellable.clone(),
    )
    .await?;
    if report.kept_in_trash.is_empty()
        && let Some(info_path) = plan.trash_info.or_else(|| entry.trash_info.clone())
    {
        let item = plan.source_path;
        let removed = gio::spawn_blocking(move || match std::fs::symlink_metadata(&item) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                std::fs::remove_file(info_path)
            }
            // The Trash still holds something under the item's name.
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        })
        .await;
        if let Ok(Err(error)) = removed {
            tracing::warn!(%error, "unable to remove restored trash metadata");
        }
    }
    Ok(report)
}

/// Finds the Trash entry a merge staged for an overwritten original.
pub(super) fn trashed_merge_original(
    location: Location,
    cancellable: gio::Cancellable,
    originals: HashMap<Location, TrashedOriginal>,
) -> Pin<Box<dyn Future<Output = Result<RestoreEntry, glib::Error>>>> {
    Box::pin(async move {
        let entries = trashed_entries_for_originals(
            std::slice::from_ref(&location),
            &cancellable,
            &originals,
        )
        .await?;
        entries.into_iter().next().ok_or_else(|| {
            glib::Error::new(
                gio::IOErrorEnum::NotFound,
                &crate::i18n::tr("The original is no longer in Trash"),
            )
        })
    })
}

pub(super) enum Reverted {
    Restored(TransferReport),
    /// What the operation put in place of the original may have changed
    /// since, so it stays, and so does the original in Trash.
    Kept,
}

/// Deletes what an operation placed at `path` if all of it is unchanged, and
/// tells whether nothing of it is left. Sets `deleting` once it deleted
/// anything.
pub(super) async fn remove_unchanged(
    path: &Path,
    placed: &PlacedSnapshot,
    unlinks: &Arc<copy_engine::Unlinks>,
    cancellable: &gio::Cancellable,
    deleting: &mut bool,
) -> Result<bool, glib::Error> {
    let removed = copy_engine::remove_placed(
        path.to_path_buf(),
        placed.clone(),
        unlinks.clone(),
        cancellable,
    )
    .await;
    *deleting = removed.removed;
    removed.result?;
    if let Some((path, error)) = removed.kept.iter().find_map(|(path, cause)| match cause {
        KeptCause::Undeletable(error) => Some((path, *error)),
        _ => None,
    }) {
        return Err(io_error(rust_i18n::t!(
            "Could not delete %{name}: %{error}",
            name = path.file_name().unwrap_or_default().to_string_lossy(),
            error = crate::services::io_error_detail(&io::Error::from_raw_os_error(error))
        )));
    }
    Ok(removed.kept.is_empty())
}

/// Reverts one overwritten merge path: deletes what the operation put there
/// if all of it is unchanged, so the staged original can move back out of
/// Trash. Sets `deleting` once it deleted anything.
pub(super) async fn undo_merged_overwrite(
    (location, placed): (&Location, Option<&PlacedSnapshot>),
    staged: &RestoreEntry,
    context: &RestoreContext,
    affected_locations: &mut HashSet<Location>,
    unlinks: &Arc<copy_engine::Unlinks>,
    cancellable: &gio::Cancellable,
    deleting: &mut bool,
) -> Result<Reverted, glib::Error> {
    let freed = match (placed, location.native_path()) {
        (Some(placed), Some(path)) => {
            remove_unchanged(path, placed, unlinks, cancellable, deleting).await?
        }
        // Nothing tells what is there, so only an empty path takes the
        // original back.
        _ => {
            let file = gio_file_for_location(location);
            let queried = await_cancellable(&file, cancellable, |file, cancellable, result| {
                file.query_info_async(
                    "standard::type",
                    gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                    glib::Priority::DEFAULT,
                    Some(cancellable),
                    move |output| result.resolve(output),
                );
            })
            .await;
            match queried {
                Err(error) if error.matches(gio::IOErrorEnum::NotFound) => true,
                Err(error) => return Err(error),
                Ok(_) => false,
            }
        }
    };
    if !freed {
        return Ok(Reverted::Kept);
    }
    restore_trash_entry(staged, context, affected_locations, unlinks, cancellable)
        .await
        .map(Reverted::Restored)
}

/// Resolves where an overwritten original was staged. Production reads Trash;
/// tests hand back fixture entries pointing at a fake trash root.
pub(super) type StagedOriginalLookup = Rc<
    dyn Fn(
        Location,
        gio::Cancellable,
    ) -> Pin<Box<dyn Future<Output = Result<RestoreEntry, glib::Error>>>>,
>;

pub(super) async fn run_merge_undo(
    request_id: OperationRequestId,
    (created, overwritten, placed): (
        Vec<Location>,
        Vec<Location>,
        HashMap<Location, PlacedSnapshot>,
    ),
    emit: Rc<dyn Fn(OperationEvent)>,
    cancellable: gio::Cancellable,
    staged_original: StagedOriginalLookup,
) {
    let total = overwritten.len() + created.len();
    let pending: Vec<Location> = overwritten.iter().chain(&created).cloned().collect();
    let mut affected_locations = HashSet::from([Location::uri("trash:///")]);
    for location in &pending {
        if let Some(parent) = location.parent() {
            affected_locations.insert(parent);
        }
    }
    let context = RestoreContext::current().await;
    let unlinks = Arc::default();
    let mut completed_locations = Vec::new();
    let mut restored = Vec::new();
    let mut removed = Vec::new();
    let mut failed_locations = Vec::new();
    let mut errors = Vec::new();
    let mut completed = 0usize;
    let remaining = |index: usize| pending[index..].to_vec();
    // Undo applies to copies only, so an incoming item at an overwritten path
    // is a duplicate of the surviving source: delete it permanently, if it
    // is unchanged, to free the path, then move the staged original back
    // out of Trash.
    for (index, location) in overwritten.iter().enumerate() {
        if cancellable.is_cancelled() {
            emit(cancelled_event(
                request_id,
                completed_locations,
                failed_locations,
                remaining(index),
                affected_locations,
            ));
            return;
        }
        let mut deleting = false;
        let result = match staged_original(location.clone(), cancellable.clone()).await {
            Ok(staged) => {
                undo_merged_overwrite(
                    (location, placed.get(location)),
                    &staged,
                    &context,
                    &mut affected_locations,
                    &unlinks,
                    &cancellable,
                    &mut deleting,
                )
                .await
            }
            Err(error) => Err(error),
        };
        let restored_location = match result {
            Ok(Reverted::Restored(report)) => {
                if !report.is_empty() {
                    emit(OperationEvent::ItemsReported { request_id, report });
                }
                completed_locations.push(location.clone());
                restored.push(location.clone());
                Some(location.clone())
            }
            Ok(Reverted::Kept) => {
                emit(OperationEvent::ItemsReported {
                    request_id,
                    report: TransferReport {
                        kept_in_trash: vec![KeptItem {
                            location: location.clone(),
                            cause: KeptCause::ReplacementChanged,
                        }],
                        ..TransferReport::default()
                    },
                });
                emit(OperationEvent::ItemKept {
                    request_id,
                    location: location.clone(),
                });
                completed_locations.push(location.clone());
                None
            }
            // Once its incoming copy is deleted, a cut-off item is no longer whole.
            Err(error) if was_cancelled(&error) => {
                if deleting {
                    failed_locations.push(location.clone());
                }
                emit(cancelled_event(
                    request_id,
                    completed_locations,
                    failed_locations,
                    remaining(index + usize::from(deleting)),
                    affected_locations,
                ));
                return;
            }
            Err(error) => {
                errors.push(item_error(
                    &location.display_name(),
                    crate::services::gio_error_detail(&error),
                ));
                failed_locations.push(location.clone());
                None
            }
        };
        completed += 1;
        emit(OperationEvent::RestoreProgress {
            request_id,
            completed,
            total,
            restored_location,
        });
    }
    for (index, location) in created.iter().enumerate() {
        if cancellable.is_cancelled() {
            emit(cancelled_event(
                request_id,
                completed_locations,
                failed_locations,
                remaining(overwritten.len() + index),
                affected_locations,
            ));
            return;
        }
        let file = gio_file_for_location(location);
        let queried = await_cancellable(&file, &cancellable, |file, cancellable, result| {
            file.query_info_async(
                "standard::type",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(cancellable),
                move |output| result.resolve(output),
            );
        })
        .await;
        let found = queried.is_ok();
        let mut trashed_identity = None;
        // Without Trash, only what the merge left unchanged is deleted.
        let mut deleting = false;
        let mut kept = false;
        let result = match queried {
            Err(error) if error.matches(gio::IOErrorEnum::NotFound) => Ok(()),
            Err(error) => Err(error),
            Ok(_) => match trash_with_cleanup(&file, &cancellable).await {
                Err(error) if error.is_unsupported() => {
                    match (placed.get(location), location.native_path()) {
                        (Some(placed), Some(path)) => {
                            remove_unchanged(path, placed, &unlinks, &cancellable, &mut deleting)
                                .await
                        }
                        _ => Ok(false),
                    }
                    .and_then(|removed| {
                        // What changed once part of it was deleted holds its folder.
                        if !removed && deleting {
                            return Err(io_error(crate::services::io_error_detail(
                                &io::Error::from_raw_os_error(
                                    rustix::io::Errno::NOTEMPTY.raw_os_error(),
                                ),
                            )));
                        }
                        kept = !removed;
                        Ok(())
                    })
                }
                result => result.map(|identity| trashed_identity = identity).map_err(|error| error.error),
            },
        };
        let deleted_locations = match result {
            Ok(()) if kept => {
                emit(OperationEvent::ItemKept {
                    request_id,
                    location: location.clone(),
                });
                errors.push(item_error(
                    &location.display_name(),
                    crate::i18n::tr("It was kept, as it may have changed since it was copied"),
                ));
                failed_locations.push(location.clone());
                Vec::new()
            }
            Ok(()) => {
                if found {
                    crate::adapters::bookmarks::deletion_completed(std::slice::from_ref(location));
                }
                if let Some(identity) = trashed_identity {
                    emit(OperationEvent::ItemTrashed {
                        request_id,
                        location: location.clone(),
                        identity,
                    });
                }
                completed_locations.push(location.clone());
                removed.push(location.clone());
                vec![location.clone()]
            }
            Err(error) if was_cancelled(&error) => {
                // Without Trash, an item stays whole until anything of it is deleted.
                let untouched = !found || deletion_undone_when_cancelled(location, deleting);
                if !untouched {
                    failed_locations.push(location.clone());
                }
                emit(cancelled_event(
                    request_id,
                    completed_locations,
                    failed_locations,
                    remaining(overwritten.len() + index + usize::from(!untouched)),
                    affected_locations,
                ));
                return;
            }
            Err(error) => {
                errors.push(item_error(
                    &location.display_name(),
                    crate::services::gio_error_detail(&error),
                ));
                failed_locations.push(location.clone());
                Vec::new()
            }
        };
        completed += 1;
        emit(OperationEvent::DeleteProgress {
            request_id,
            completed,
            total,
            deleted_locations,
        });
    }
    if cancellable.is_cancelled() {
        emit(cancelled_event(
            request_id,
            completed_locations,
            failed_locations,
            Vec::new(),
            affected_locations,
        ));
    } else if errors.is_empty() {
        emit(OperationEvent::Restored {
            request_id,
            locations: removed,
            restored,
        });
    } else {
        emit(OperationEvent::CompletedWithErrors {
            request_id,
            deleted_locations: removed,
            retryable_locations: Vec::new(),
            has_non_retryable_failures: true,
            message: deletion_error_summary(&errors, true),
        });
    }
}

pub(super) fn home_trash_entries(
    requested: &HashSet<PathBuf>,
    cancellable: &gio::Cancellable,
    originals: &HashMap<Location, TrashedOriginal>,
) -> HashMap<PathBuf, RestoreEntry> {
    home_trash_entries_at(
        &glib::user_data_dir().join("Trash"),
        requested,
        cancellable,
        originals,
    )
}

pub(super) fn home_trash_entries_at(
    trash_root: &Path,
    requested: &HashSet<PathBuf>,
    cancellable: &gio::Cancellable,
    originals: &HashMap<Location, TrashedOriginal>,
) -> HashMap<PathBuf, RestoreEntry> {
    let info_root = trash_root.join("info");
    let files_root = trash_root.join("files");
    let mut newest = HashMap::<PathBuf, (String, RestoreEntry)>::new();
    let Ok(infos) = std::fs::read_dir(info_root) else {
        return HashMap::new();
    };
    for info in infos.flatten() {
        if cancellable.is_cancelled() {
            break;
        }
        let info_path = info.path();
        let Some(name) = info_path.file_name() else {
            continue;
        };
        let bytes = name.as_bytes();
        let Some(file_name) = bytes.strip_suffix(b".trashinfo") else {
            continue;
        };
        let Ok(contents) = std::fs::read_to_string(&info_path) else {
            continue;
        };
        let encoded_path = contents.lines().find_map(|line| line.strip_prefix("Path="));
        let deletion_date = contents
            .lines()
            .find_map(|line| line.strip_prefix("DeletionDate="))
            .unwrap_or_default();
        let Some(original_path) =
            encoded_path.and_then(|path| gio::File::for_uri(&format!("file://{path}")).path())
        else {
            continue;
        };
        if !requested.contains(&original_path) {
            continue;
        }
        let source_path = files_root.join(OsString::from_vec(file_name.to_vec()));
        let Ok(metadata) = std::fs::symlink_metadata(&source_path) else {
            continue;
        };
        if originals
            .get(&Location::local(&original_path))
            .is_some_and(|expected| *expected != TrashedOriginal::from_metadata(&metadata))
        {
            continue;
        }
        let entry = RestoreEntry {
            source: Location::local(&source_path),
            display_name: source_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| crate::i18n::tr("Trashed item")),
            original_target: Some(Location::local(&original_path)),
            trash_info: Some(info_path),
            confirmed_destination: None,
            physical_path: Some(source_path.clone()),
        };
        match newest.get(&original_path) {
            Some((current_date, _)) if current_date.as_str() >= deletion_date => {}
            _ => {
                newest.insert(original_path, (deletion_date.to_owned(), entry));
            }
        }
    }
    newest
        .into_iter()
        .map(|(path, (_, entry))| (path, entry))
        .collect()
}

