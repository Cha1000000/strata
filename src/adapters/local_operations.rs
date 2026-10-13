// SPDX-License-Identifier: MIT

#[cfg(test)]
mod tests;

mod archive;
mod copy_engine;
mod create_entry;
mod fs_ops;
mod merge_aside;
mod owed;
mod placement;
mod trash;
mod work_queue;

#[cfg(test)]
pub(crate) use archive::ArchiveListing;
pub(crate) use archive::{
    ARCHIVE_PREVIEW_FAILED_MESSAGE, ARCHIVE_TOO_LARGE_MESSAGE, ARCHIVE_UNSUPPORTED_MESSAGE,
    ArchiveListingStatus, INVALID_ARCHIVE, MAX_ARCHIVE_PASSWORD_BYTES, MAYBE_BAD_PASSWORD,
    PASSWORD_REQUIRED, archive_payload_valid, decode_archive_listing, encode_archive_result,
    list_archive_entries_direct,
};

#[cfg(test)]
pub(crate) use archive::write_compression_fixture;

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    future::Future,
    io,
    os::{
        fd::{AsFd, OwnedFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use gtk::{gio, glib, prelude::*};

use copy_engine::SharedProgress;
use fs_ops::{Entry, EntryStat, OffMain, fs};
use owed::{Deletion, Done, Owed, Settling};
use placement::{COPY_PREFIX, Displacement, PlaceError, REPLACED_PREFIX, RenameFailure};
use work_queue::{Group, Job, WorkQueue};

use crate::{
    adapters::{
        gio_file_for_location, location_for_file,
        trash_restore::{RestoreContext, plan_restore_for_location},
        volume::MountTable,
    },
    model::{FileEntry, Location},
    services::{
        CancelledOperation, CompressRequest, CreateDirectoryRequest, CreateFileRequest,
        DeleteRequest, ExtractRequest, KeptCause, KeptItem, LoadHandle, MoveRecord, OperationEvent,
        OperationProvider, OperationRequestId, PasteRequest, RenameCause, RenameRequest,
        RenamedItem, ReplacedOriginal, RestoreRequest, RestoreSource, TransferConflict,
        TransferReport, TrashedOriginal, UndoCopyRequest, UndoMergeRequest, UndoMoveRequest,
        UndoRenameRequest, validate_basename,
    },
};

const INVALID_NAME_REASON: &str = crate::services::INVALID_NAME_REASON;
const UNCONFIRMED_REASON: &str =
    "The result could not be confirmed. Check both locations before trying again.";
const UNSUPPORTED_REASON: &str =
    "This location can\u{2019}t guarantee that another item won\u{2019}t be overwritten";

#[derive(Clone, Debug)]
pub(crate) struct MountFlushHint {
    pub root: PathBuf,
    pub removable: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum SyncDestination<'a> {
    Local(&'a Path),
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "copy helper tests pass a destination that has no local path"
        )
    )]
    NonLocal,
}

fn blocking_join_message(error: Box<dyn std::any::Any + Send>) -> String {
    error
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            error
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
        })
        .unwrap_or_else(|| "filesystem sync panicked".to_owned())
}

fn device_flush_failure(error: &str) -> String {
    rust_i18n::t!(
        "The device could not finish writing: %{error}. Earlier writes may still be pending; wait for safe eject before unplugging.",
        error = error
    )
    .into_owned()
}

pub(crate) fn sync_filesystem(root: &Path) -> io::Result<()> {
    let handle = rustix::fs::open(
        root,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)?;
    rustix::fs::syncfs(&handle).map_err(io::Error::from)?;
    Ok(())
}

pub(crate) fn flush_filesystem(root: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(kind) = sync_probe_before(root) {
        return Err(io::Error::new(kind, "flush failed"));
    }
    sync_filesystem(root)
}

#[cfg(test)]
mod sync_probe {
    use std::{
        io,
        path::{Path, PathBuf},
        sync::{Arc, Condvar, Mutex, atomic::AtomicBool, atomic::Ordering},
        thread,
    };

    struct SyncGate {
        started: AtomicBool,
        released: Mutex<bool>,
        released_cv: Condvar,
    }

    struct SyncProbeState {
        fail: Option<io::ErrorKind>,
        gate: Option<Arc<SyncGate>>,
        calls: Vec<SyncObservation>,
    }

    type SyncObserver = Arc<dyn Fn(&Path) + Send + Sync>;

    static SYNC_PROBE: Mutex<Option<SyncProbeState>> = Mutex::new(None);
    static SYNC_OBSERVER: Mutex<Option<SyncObserver>> = Mutex::new(None);

    #[derive(Clone, Debug)]
    pub(crate) struct SyncObservation {
        pub root: PathBuf,
        pub thread_id: thread::ThreadId,
    }

    pub(crate) fn install_sync_probe(fail: Option<io::ErrorKind>, gate: bool) {
        let gate = gate.then(|| {
            Arc::new(SyncGate {
                started: AtomicBool::new(false),
                released: Mutex::new(false),
                released_cv: Condvar::new(),
            })
        });
        let mut slot = SYNC_PROBE.lock().unwrap_or_else(|error| error.into_inner());
        *slot = Some(SyncProbeState {
            fail,
            gate,
            calls: Vec::new(),
        });
    }

    pub(crate) fn clear_sync_probe() {
        release_sync_probe();
        let mut slot = SYNC_PROBE.lock().unwrap_or_else(|error| error.into_inner());
        *slot = None;
    }

    pub(crate) fn set_sync_observer(observer: Option<SyncObserver>) {
        let mut slot = SYNC_OBSERVER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *slot = observer;
    }

    pub(crate) fn release_sync_probe() {
        let gate = {
            let slot = SYNC_PROBE.lock().unwrap_or_else(|error| error.into_inner());
            slot.as_ref().and_then(|probe| probe.gate.clone())
        };
        let Some(gate) = gate else {
            return;
        };
        let mut released = gate
            .released
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *released = true;
        gate.released_cv.notify_all();
    }

    pub(crate) fn sync_probe_is_blocked() -> bool {
        let slot = SYNC_PROBE.lock().unwrap_or_else(|error| error.into_inner());
        slot.as_ref()
            .and_then(|probe| probe.gate.as_ref())
            .is_some_and(|gate| gate.started.load(Ordering::SeqCst))
    }

    pub(crate) fn sync_probe_len() -> usize {
        let slot = SYNC_PROBE.lock().unwrap_or_else(|error| error.into_inner());
        slot.as_ref().map(|probe| probe.calls.len()).unwrap_or(0)
    }

    pub(crate) fn sync_probe_observations() -> Vec<SyncObservation> {
        let slot = SYNC_PROBE.lock().unwrap_or_else(|error| error.into_inner());
        slot.as_ref()
            .map(|probe| probe.calls.clone())
            .unwrap_or_default()
    }

    pub(super) fn sync_probe_before(root: &Path) -> Option<io::ErrorKind> {
        let (fail, gate) = {
            let mut slot = SYNC_PROBE.lock().unwrap_or_else(|error| error.into_inner());
            let probe = slot.as_mut()?;
            probe.calls.push(SyncObservation {
                root: root.to_path_buf(),
                thread_id: thread::current().id(),
            });
            (probe.fail, probe.gate.clone())
        };
        let observer = SYNC_OBSERVER
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(observer) = observer {
            observer(root);
        }
        if let Some(gate) = gate {
            gate.started.store(true, Ordering::SeqCst);
            let mut released = gate
                .released
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            while !*released {
                released = gate
                    .released_cv
                    .wait(released)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
        fail
    }
}

#[cfg(test)]
use sync_probe::sync_probe_before;
#[cfg(test)]
pub(crate) use sync_probe::{
    clear_sync_probe, install_sync_probe, release_sync_probe, set_sync_observer,
    sync_probe_is_blocked, sync_probe_len, sync_probe_observations,
};

#[cfg(test)]
thread_local! {
    static REMOVABLE_ROOTS_FOR_TEST: RefCell<Option<Vec<PathBuf>>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_removable_roots_for_test(roots: Option<Vec<PathBuf>>) {
    REMOVABLE_ROOTS_FOR_TEST.with(|slot| *slot.borrow_mut() = roots);
}

#[cfg(test)]
fn removable_hints_for_test() -> Option<Vec<MountFlushHint>> {
    REMOVABLE_ROOTS_FOR_TEST.with(|slot| {
        slot.borrow().as_ref().map(|roots| {
            roots
                .iter()
                .map(|root| MountFlushHint {
                    root: root.clone(),
                    removable: true,
                })
                .collect()
        })
    })
}

pub(crate) fn removable_sync_roots(
    destinations: &[SyncDestination<'_>],
    hints: &[MountFlushHint],
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for destination in destinations {
        let SyncDestination::Local(path) = destination else {
            continue;
        };
        let Some(hint) = hints
            .iter()
            .filter(|hint| path.starts_with(&hint.root))
            .max_by_key(|hint| hint.root.as_os_str().len())
        else {
            continue;
        };
        if hint.removable && !roots.iter().any(|root| root == &hint.root) {
            roots.push(hint.root.clone());
        }
    }
    roots
}

/// Reads the mount table off the main thread; the volume monitor is
/// in-memory GIO state and is matched here.
async fn removable_roots_for_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    if paths.is_empty() {
        return Vec::new();
    }
    let destinations: Vec<_> = paths
        .iter()
        .map(|path| SyncDestination::Local(path))
        .collect();
    #[cfg(test)]
    if let Some(hints) = removable_hints_for_test() {
        return removable_sync_roots(&destinations, &hints);
    }
    let table = MountTable::load().await;
    let mounts = gio::VolumeMonitor::get().mounts();
    let mut hints = Vec::new();
    for path in paths {
        let Some(root) = table.mount_point_for(path).map(Path::to_path_buf) else {
            continue;
        };
        if hints.iter().any(|hint: &MountFlushHint| hint.root == root) {
            continue;
        }
        let removable = mounts.iter().any(|mount| {
            mount.root().path().as_deref() == Some(root.as_path())
                && mount_drive_is_removable(mount)
        });
        hints.push(MountFlushHint { root, removable });
    }
    removable_sync_roots(&destinations, &hints)
}

fn mount_drive_is_removable(mount: &gio::Mount) -> bool {
    let drive = mount
        .drive()
        .or_else(|| mount.volume().and_then(|volume| volume.drive()));
    drive.is_some_and(|drive| drive.is_removable() || drive.is_media_removable())
}

async fn flush_removable_writes(
    paths: Vec<PathBuf>,
    cancellable: &gio::Cancellable,
    emit: &Rc<dyn Fn(OperationEvent)>,
    request_id: OperationRequestId,
    completed: &[Location],
    affected_locations: HashSet<Location>,
) -> bool {
    let roots = removable_roots_for_paths(&paths).await;
    if roots.is_empty() {
        if cancellable.is_cancelled() {
            emit(cancelled_event(
                request_id,
                completed.to_vec(),
                Vec::new(),
                Vec::new(),
                affected_locations,
            ));
            return false;
        }
        return true;
    }
    emit(OperationEvent::FlushingToDevice { request_id });
    for root in roots {
        let synced = gio::spawn_blocking(move || flush_filesystem(&root)).await;
        let error = match synced {
            Ok(Ok(())) => continue,
            Ok(Err(error)) => os_error_text(error),
            Err(error) => blocking_join_message(error),
        };
        emit(OperationEvent::TransferFailed {
            request_id,
            completed_locations: completed.to_vec(),
            message: device_flush_failure(&error),
        });
        return false;
    }
    if cancellable.is_cancelled() {
        emit(cancelled_event(
            request_id,
            completed.to_vec(),
            Vec::new(),
            Vec::new(),
            affected_locations,
        ));
        return false;
    }
    true
}

async fn flush_written_roots(
    paths: &[PathBuf],
    emit: &Rc<dyn Fn(OperationEvent)>,
    request_id: OperationRequestId,
) -> Result<(), String> {
    let roots = removable_roots_for_paths(paths).await;
    if roots.is_empty() {
        return Ok(());
    }
    emit(OperationEvent::FlushingToDevice { request_id });
    for root in roots {
        let result = gio::spawn_blocking(move || flush_filesystem(&root)).await;
        let error = match result {
            Ok(Ok(())) => continue,
            Ok(Err(error)) => os_error_text(error),
            Err(error) => blocking_join_message(error),
        };
        return Err(device_flush_failure(&error));
    }
    Ok(())
}

struct TransferStop {
    completed: Vec<Location>,
    failed: Vec<Location>,
    not_attempted: Vec<Location>,
    affected_locations: HashSet<Location>,
    failure: Option<String>,
    skip_flush: bool,
}

/// Both reasons a transfer stopped, as one message.
fn joined_failures(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(crate::i18n::join_sentences(&first, &second)),
        (first, second) => first.or(second),
    }
}

async fn stop_transfer(
    paths: &[PathBuf],
    emit: &Rc<dyn Fn(OperationEvent)>,
    request_id: OperationRequestId,
    stop: TransferStop,
) {
    let flush_error = if stop.skip_flush {
        None
    } else {
        flush_written_roots(paths, emit, request_id).await.err()
    };
    match (stop.failure, flush_error) {
        (failure, Some(flush_error)) => emit(OperationEvent::TransferFailed {
            request_id,
            completed_locations: stop.completed,
            message: failure.map_or(flush_error.clone(), |message| {
                crate::i18n::join_sentences(&message, &flush_error)
            }),
        }),
        (Some(message), None) => emit(OperationEvent::TransferFailed {
            request_id,
            completed_locations: stop.completed,
            message,
        }),
        (None, None) => emit(cancelled_event(
            request_id,
            stop.completed,
            stop.failed,
            stop.not_attempted,
            stop.affected_locations,
        )),
    }
}

async fn await_cancellable<O, T>(
    object: &O,
    cancellable: &gio::Cancellable,
    start: impl FnOnce(&O, &gio::Cancellable, gio::GioFutureResult<Result<T, glib::Error>>) + 'static,
) -> Result<T, glib::Error>
where
    O: Clone + 'static,
    T: 'static,
{
    // The backend's callback is authoritative: cancellation can race with a successful result.
    let cancellable = cancellable.clone();
    gio::GioFuture::new(object, move |object, _, result| {
        start(object, &cancellable, result);
    })
    .await
}

async fn join_local_tasks<K, T>(
    tasks: Vec<(K, glib::JoinHandle<T>)>,
) -> Vec<(K, Result<T, glib::JoinError>)>
where
    T: 'static,
{
    let mut results = Vec::with_capacity(tasks.len());
    for (key, task) in tasks {
        results.push((key, task.await));
    }
    results
}

/// Runs `work` on a worker thread with a flag that follows `cancellable`.
async fn run_cancellable<T: Send + 'static>(
    cancellable: &gio::Cancellable,
    work: impl FnOnce(Arc<AtomicBool>) -> T + Send + 'static,
) -> Result<T, Box<dyn std::any::Any + Send>> {
    let cancelled = Arc::new(AtomicBool::new(cancellable.is_cancelled()));
    let flag = cancelled.clone();
    let handler = cancellable.connect_cancelled(move |_| flag.store(true, Ordering::Release));
    let result = gio::spawn_blocking(move || work(cancelled)).await;
    if let Some(handler) = handler {
        cancellable.disconnect_cancelled(handler);
    }
    result
}

const MAX_LOCAL_IO_WORKERS: usize = 8;
const TRANSFER_PROGRESS_INTERVAL: Duration = Duration::from_millis(33);
// Smaller copies normally finish before the first sample; keep them on GIO's lower-overhead path.
const BYTE_PROGRESS_MIN_FILE_SIZE: u64 = 8 * 1024 * 1024;

/// Bytes and files an item has added to a transfer's progress.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Reported {
    bytes: u64,
    files: usize,
}

impl Reported {
    fn since(self, earlier: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_sub(earlier.bytes),
            files: self.files.saturating_sub(earlier.files),
        }
    }
}

struct TransferProgressTracker {
    request_id: OperationRequestId,
    completed_items: Cell<usize>,
    completed_files: Cell<usize>,
    total_files: Option<usize>,
    current_file: RefCell<Option<String>>,
    transferred_bytes: Cell<u64>,
    total_bytes: Option<u64>,
    /// What finished items reported beyond their estimates, as files that grew.
    excess: Cell<Reported>,
    last_emit: Cell<Option<Instant>>,
    emit: Rc<dyn Fn(OperationEvent)>,
}

impl TransferProgressTracker {
    fn new(
        request_id: OperationRequestId,
        total_bytes: Option<u64>,
        total_files: Option<usize>,
        emit: Rc<dyn Fn(OperationEvent)>,
    ) -> Rc<Self> {
        Rc::new(Self {
            request_id,
            completed_items: Cell::new(0),
            completed_files: Cell::new(0),
            total_files,
            current_file: RefCell::new(None),
            transferred_bytes: Cell::new(0),
            total_bytes,
            excess: Cell::new(Reported::default()),
            last_emit: Cell::new(None),
            emit,
        })
    }

    /// The estimates raised by what items reported beyond them, and never
    /// below what was transferred.
    fn totals(&self) -> (Option<u64>, Option<usize>) {
        let excess = self.excess.get();
        (
            self.total_bytes.map(|total| {
                total
                    .saturating_add(excess.bytes)
                    .max(self.transferred_bytes.get())
            }),
            self.total_files.map(|total| {
                total
                    .saturating_add(excess.files)
                    .max(self.completed_files.get())
            }),
        )
    }

    fn reported(&self) -> Reported {
        Reported {
            bytes: self.transferred_bytes.get(),
            files: self.completed_files.get(),
        }
    }

    fn emit(&self) {
        self.last_emit.set(Some(Instant::now()));
        self.emit_progress(None);
    }

    fn emit_if_due(&self) {
        if self
            .last_emit
            .get()
            .is_none_or(|last| last.elapsed() >= TRANSFER_PROGRESS_INTERVAL)
        {
            self.emit();
        }
    }

    fn emit_progress(&self, created_location: Option<Location>) {
        let (total_bytes, total_files) = self.totals();
        (self.emit)(OperationEvent::TransferProgress {
            request_id: self.request_id,
            completed_items: self.completed_items.get(),
            completed_files: self.completed_files.get(),
            total_files,
            current_file: self.current_file.borrow().clone(),
            transferred_bytes: self.transferred_bytes.get(),
            total_bytes,
            created_location,
        });
    }

    /// Emits at once when the last file completes, so it is never throttled away.
    fn emit_after_files(&self) {
        if self.totals().1 == Some(self.completed_files.get()) {
            self.emit();
        } else {
            self.emit_if_due();
        }
    }

    /// Reports what an item did to items inside it, unless nothing.
    fn report(&self, report: TransferReport) {
        if !report.is_empty() {
            (self.emit)(OperationEvent::ItemsReported {
                request_id: self.request_id,
                report,
            });
        }
    }

    fn set_current_file(&self, name: String) {
        self.current_file.replace(Some(name));
        self.emit_if_due();
    }

    /// Feeds what workers report to `shared` into this tracker until stopped.
    fn follow(self: &Rc<Self>, shared: &Arc<SharedProgress>) -> ProgressFollower {
        let seen = Rc::new(Cell::new(Reported::default()));
        let (tracker, timer_shared, timer_seen) = (self.clone(), shared.clone(), seen.clone());
        let source = glib::timeout_add_local(TRANSFER_PROGRESS_INTERVAL, move || {
            tracker.fold(&timer_shared, &timer_seen);
            glib::ControlFlow::Continue
        });
        ProgressFollower {
            tracker: self.clone(),
            shared: shared.clone(),
            seen,
            source: Some(source),
        }
    }

    fn fold(&self, shared: &SharedProgress, seen: &Cell<Reported>) {
        if let Some(name) = shared.take_current() {
            self.current_file.replace(Some(name));
        }
        let now = shared.reported();
        let delta = now.since(seen.replace(now));
        if delta == Reported::default() {
            return;
        }
        self.transferred_bytes
            .set(self.transferred_bytes.get().saturating_add(delta.bytes));
        self.completed_files
            .set(self.completed_files.get().saturating_add(delta.files));
        if delta.files > 0 {
            self.emit_after_files();
        } else {
            self.emit_if_due();
        }
    }

    fn add_bytes(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.transferred_bytes
            .set(self.transferred_bytes.get().saturating_add(bytes));
        self.emit_if_due();
    }

    fn begin_file(self: &Rc<Self>, name: String, track_bytes: bool) -> FileTransferProgress {
        self.set_current_file(name);
        let byte_progress = track_bytes.then(|| {
            let reported_bytes = Arc::new(AtomicU64::new(0));
            let reported_total = Arc::new(AtomicU64::new(0));
            let observed_bytes = Rc::new(Cell::new(0));
            let tracker = self.clone();
            let timer_reported = reported_bytes.clone();
            let timer_observed = observed_bytes.clone();
            let update_source = glib::timeout_add_local(TRANSFER_PROGRESS_INTERVAL, move || {
                record_file_progress(&tracker, &timer_reported, &timer_observed);
                glib::ControlFlow::Continue
            });
            FileByteProgress {
                reported_bytes,
                reported_total,
                observed_bytes,
                update_source: RefCell::new(Some(update_source)),
            }
        });
        FileTransferProgress {
            tracker: self.clone(),
            byte_progress,
        }
    }

    /// Credits what the item owes beyond what it `reported`, so concurrent
    /// items are never counted twice.
    fn finish_item(
        &self,
        reported: Reported,
        expected_bytes: Option<u64>,
        expected_files: Option<usize>,
        created_location: Option<Location>,
    ) {
        let mut excess = self.excess.get();
        if let Some(expected_bytes) = expected_bytes {
            self.transferred_bytes.set(
                self.transferred_bytes
                    .get()
                    .saturating_add(expected_bytes.saturating_sub(reported.bytes)),
            );
            excess.bytes = excess
                .bytes
                .saturating_add(reported.bytes.saturating_sub(expected_bytes));
        }
        if let Some(expected_files) = expected_files {
            self.completed_files.set(
                self.completed_files
                    .get()
                    .saturating_add(expected_files.saturating_sub(reported.files)),
            );
            excess.files = excess
                .files
                .saturating_add(reported.files.saturating_sub(expected_files));
        }
        self.excess.set(excess);
        self.completed_items
            .set(self.completed_items.get().saturating_add(1));
        self.last_emit.set(Some(Instant::now()));
        self.emit_progress(created_location);
    }
}

struct ProgressFollower {
    tracker: Rc<TransferProgressTracker>,
    shared: Arc<SharedProgress>,
    seen: Rc<Cell<Reported>>,
    source: Option<glib::SourceId>,
}

impl ProgressFollower {
    /// Stops following and folds in the last report, so a late tick cannot
    /// count it twice.
    fn stop(mut self) -> Reported {
        if let Some(source) = self.source.take() {
            source.remove();
        }
        self.tracker.fold(&self.shared, &self.seen);
        self.seen.get()
    }
}

impl Drop for ProgressFollower {
    fn drop(&mut self) {
        if let Some(source) = self.source.take() {
            source.remove();
        }
    }
}

struct FileTransferProgress {
    tracker: Rc<TransferProgressTracker>,
    byte_progress: Option<FileByteProgress>,
}

struct FileByteProgress {
    reported_bytes: Arc<AtomicU64>,
    reported_total: Arc<AtomicU64>,
    observed_bytes: Rc<Cell<u64>>,
    update_source: RefCell<Option<glib::SourceId>>,
}

impl FileTransferProgress {
    fn reporter(&self) -> Option<FileProgressReporter> {
        self.byte_progress
            .as_ref()
            .map(|progress| FileProgressReporter {
                reported_bytes: progress.reported_bytes.clone(),
                reported_total: progress.reported_total.clone(),
            })
    }

    fn finish(&self, bytes: Option<u64>) {
        if let Some(progress) = &self.byte_progress {
            progress.stop_updates();
            let final_bytes = bytes
                .unwrap_or_else(|| progress.reported_total.load(Ordering::Relaxed))
                .max(progress.reported_bytes.load(Ordering::Relaxed));
            progress
                .reported_bytes
                .fetch_max(final_bytes, Ordering::Relaxed);
            record_file_progress(
                &self.tracker,
                &progress.reported_bytes,
                &progress.observed_bytes,
            );
        } else if let Some(bytes) = bytes {
            self.tracker
                .transferred_bytes
                .set(self.tracker.transferred_bytes.get().saturating_add(bytes));
        }
        self.tracker
            .completed_files
            .set(self.tracker.completed_files.get().saturating_add(1));
        self.tracker.emit_after_files();
    }
}

impl Drop for FileTransferProgress {
    fn drop(&mut self) {
        if let Some(progress) = &self.byte_progress {
            progress.stop_updates();
        }
    }
}

impl FileByteProgress {
    fn stop_updates(&self) {
        if let Some(source) = self.update_source.borrow_mut().take() {
            source.remove();
        }
    }
}

#[derive(Clone)]
struct FileProgressReporter {
    reported_bytes: Arc<AtomicU64>,
    reported_total: Arc<AtomicU64>,
}

impl FileProgressReporter {
    fn record(&self, current: i64, total: i64) {
        self.reported_bytes
            .fetch_max(current.max(0) as u64, Ordering::Relaxed);
        if total >= 0 {
            self.reported_total
                .fetch_max(total as u64, Ordering::Relaxed);
        }
    }
}

fn record_file_progress(
    tracker: &TransferProgressTracker,
    reported_bytes: &AtomicU64,
    observed_bytes: &Cell<u64>,
) {
    let current = reported_bytes.load(Ordering::Relaxed);
    let previous = observed_bytes.replace(current);
    tracker.add_bytes(current.saturating_sub(previous));
}

async fn run_file_operation_with_progress(
    reporter: FileProgressReporter,
    operation: impl FnOnce(&mut dyn FnMut(i64, i64)) -> Result<(), glib::Error> + Send + 'static,
    worker_error: &'static str,
) -> Result<(), glib::Error> {
    gio::spawn_blocking(move || {
        let mut callback = move |current, total| reporter.record(current, total);
        operation(&mut callback)
    })
    .await
    .map_err(|_| io_error(worker_error))?
}

async fn copy_file_with_progress(
    source: gio::File,
    target: gio::File,
    flags: gio::FileCopyFlags,
    cancellable: gio::Cancellable,
    reporter: Option<FileProgressReporter>,
) -> Result<(), glib::Error> {
    let Some(reporter) = reporter else {
        return await_cancellable(&source, &cancellable, move |source, cancellable, result| {
            source.copy_async(
                &target,
                flags,
                glib::Priority::DEFAULT,
                Some(cancellable),
                None,
                move |output| result.resolve(output),
            );
        })
        .await;
    };
    run_file_operation_with_progress(
        reporter,
        move |progress| source.copy(&target, flags, Some(&cancellable), Some(progress)),
        "Copy worker stopped unexpectedly",
    )
    .await
}

async fn move_file_with_progress(
    source: gio::File,
    target: gio::File,
    flags: gio::FileCopyFlags,
    cancellable: gio::Cancellable,
    reporter: Option<FileProgressReporter>,
) -> Result<(), glib::Error> {
    let Some(reporter) = reporter else {
        return await_cancellable(&source, &cancellable, move |source, cancellable, result| {
            source.move_async(
                &target,
                flags,
                glib::Priority::DEFAULT,
                Some(cancellable),
                None,
                move |output| result.resolve(output),
            );
        })
        .await;
    };
    run_file_operation_with_progress(
        reporter,
        move |progress| source.move_(&target, flags, Some(&cancellable), Some(progress)),
        "Move worker stopped unexpectedly",
    )
    .await
}

const FAT32_MAX_FILE_SIZE: u64 = u32::MAX as u64;

fn copy_failure_on_fat32(error: &glib::Error, fat32_destination: bool) -> (Option<String>, bool) {
    if was_cancelled(error) {
        return (None, false);
    }
    if !fat32_destination {
        return (Some(crate::services::gio_error_message(error)), false);
    }
    let description = if error.message().contains("File too large") {
        crate::i18n::tr(
            "A file is too large for this FAT32 drive (maximum file size: 4 GiB). Use an exFAT or another large-file-capable drive instead.",
        )
    } else {
        crate::services::gio_error_message(error)
    };
    let details = rust_i18n::t!(
        "Earlier completed copies may still be writing; wait for safe eject before unplugging. Details: %{error}",
        error = crate::services::gio_error_detail(error)
    );
    (
        Some(crate::i18n::join_sentences(&description, &details)),
        true,
    )
}

/// Size probes never fail with NoSpace on their own, so the code marks this refusal.
fn fat32_file_too_large(name: &str) -> glib::Error {
    glib::Error::new(
        gio::IOErrorEnum::NoSpace,
        &rust_i18n::t!(
            "%{name} is too large for a FAT32 drive (maximum file size: 4 GiB). Use an exFAT or another large-file-capable drive instead.",
            name = name
        ),
    )
}

fn fat32_file_size_limit(fs_type: Option<&str>) -> Option<u64> {
    matches!(fs_type, Some("msdos" | "vfat")).then_some(FAT32_MAX_FILE_SIZE)
}

#[derive(Clone, Copy)]
struct TransferEstimate {
    bytes: Option<u64>,
    files: usize,
    regular_file: bool,
}

fn transfer_size(
    file: gio::File,
    cancellable: gio::Cancellable,
    max_file_size: Option<u64>,
) -> Pin<Box<dyn Future<Output = Result<TransferEstimate, glib::Error>>>> {
    Box::pin(async move {
        let info = await_cancellable(&file, &cancellable, |file, cancellable, result| {
            file.query_info_async(
                "standard::type,standard::size",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(cancellable),
                move |output| result.resolve(output),
            );
        })
        .await?;
        if info.file_type() != gio::FileType::Directory {
            let size = info
                .has_attribute(gio::FILE_ATTRIBUTE_STANDARD_SIZE)
                .then(|| info.size().max(0) as u64);
            if info.file_type() == gio::FileType::Regular
                && let (Some(limit), Some(size)) = (max_file_size, size)
                && size > limit
            {
                return Err(fat32_file_too_large(
                    &transfer_source_name(&file)
                        .unwrap_or_default()
                        .to_string_lossy(),
                ));
            }
            return Ok(TransferEstimate {
                bytes: size,
                files: 1,
                regular_file: info.file_type() == gio::FileType::Regular,
            });
        }

        let enumerator = await_cancellable(&file, &cancellable, |file, cancellable, result| {
            file.enumerate_children_async(
                "standard::name",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(cancellable),
                move |output| result.resolve(output),
            );
        })
        .await?;
        let mut total = Some(0_u64);
        let mut files = 0_usize;
        loop {
            let children = await_cancellable(
                &enumerator,
                &cancellable,
                |enumerator, cancellable, result| {
                    enumerator.next_files_async(
                        64,
                        glib::Priority::DEFAULT,
                        Some(cancellable),
                        move |output| result.resolve(output),
                    );
                },
            )
            .await?;
            if children.is_empty() {
                return Ok(TransferEstimate {
                    bytes: total,
                    files,
                    regular_file: false,
                });
            }
            for chunk in children.chunks(MAX_LOCAL_IO_WORKERS) {
                let context = glib::MainContext::default();
                let tasks = chunk
                    .iter()
                    .map(|child| {
                        context.spawn_local(transfer_size(
                            file.child(child.name()),
                            cancellable.clone(),
                            max_file_size,
                        ))
                    })
                    .collect::<Vec<_>>();
                for task in tasks {
                    let child_size = task
                        .await
                        .map_err(|_| io_error("Transfer-size worker failed"))??;
                    total = total.and_then(|total| {
                        child_size.bytes.and_then(|size| total.checked_add(size))
                    });
                    files = files.saturating_add(child_size.files);
                }
            }
        }
    })
}

async fn transfer_sizes(
    files: &[gio::File],
    cancellable: &gio::Cancellable,
    max_file_size: Option<u64>,
) -> Result<
    (
        Vec<Option<u64>>,
        Option<u64>,
        Vec<Option<usize>>,
        Option<usize>,
        Vec<bool>,
    ),
    glib::Error,
> {
    let mut sizes = Vec::with_capacity(files.len());
    let mut counts = Vec::with_capacity(files.len());
    let mut total = Some(0_u64);
    let mut total_files = Some(0_usize);
    let mut regular_files = Vec::with_capacity(files.len());
    for chunk in files.chunks(MAX_LOCAL_IO_WORKERS) {
        let context = glib::MainContext::default();
        let tasks = chunk
            .iter()
            .map(|file| {
                context.spawn_local(transfer_size(
                    file.clone(),
                    cancellable.clone(),
                    max_file_size,
                ))
            })
            .collect::<Vec<_>>();
        for task in tasks {
            let size = match task.await {
                Ok(Ok(size)) => Some(size),
                Ok(Err(error))
                    if was_cancelled(&error) || error.matches(gio::IOErrorEnum::NoSpace) =>
                {
                    return Err(error);
                }
                Ok(Err(_)) | Err(_) => None,
            };
            total = total.and_then(|total| {
                size.and_then(|size| size.bytes.and_then(|bytes| total.checked_add(bytes)))
            });
            total_files =
                total_files.and_then(|total| size.map(|size| total.saturating_add(size.files)));
            counts.push(size.map(|size| size.files));
            sizes.push(size.and_then(|size| size.bytes));
            regular_files.push(size.is_some_and(|size| size.regular_file));
        }
    }
    Ok((sizes, total, counts, total_files, regular_files))
}

fn validated_child(parent: &gio::File, name: &str) -> Result<gio::File, &'static str> {
    validate_basename(name)?;
    Ok(parent.child(name))
}

fn transfer_is_noop(source: &gio::File, destination: &gio::File, target: &gio::File) -> bool {
    source.equal(target)
        || source.equal(destination)
        || destination.has_prefix(source)
        || target_is_within_source(source, target) == Some(true)
}

/// Compares device/inode identities so symlink, bind-mount, and
/// case-insensitive aliases of the source are caught. A symlink source keeps
/// its own identity, so moving a link into the folder it targets is allowed.
/// `None` means an ancestor could not be inspected.
fn target_is_within_source(source: &gio::File, target: &gio::File) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;

    fs_ops::assert_off_main_thread();
    let (Some(source_path), Some(target_path)) = (source.path(), target.path()) else {
        return Some(false);
    };
    let identity = |metadata: &std::fs::Metadata| (metadata.dev(), metadata.ino());
    let source_identity = identity(&std::fs::symlink_metadata(source_path).ok()?);
    if std::fs::symlink_metadata(&target_path)
        .is_ok_and(|metadata| identity(&metadata) == source_identity)
    {
        return Some(true);
    }
    for ancestor in target_path.ancestors().skip(1) {
        match std::fs::metadata(ancestor) {
            Ok(metadata) if identity(&metadata) == source_identity => return Some(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(false)
}

pub(crate) fn transfer_source_name(source: &gio::File) -> Option<OsString> {
    crate::model::transfer_file_name(source)
}

fn default_transfer_target(
    source: &gio::File,
    destination: &gio::File,
    fat_family: bool,
    used_names: &mut HashSet<OsString>,
) -> Option<gio::File> {
    let name = transfer_source_name(source)?;
    Some(destination.child(fat_family_child_name(&name, fat_family, used_names)))
}

#[derive(Clone)]
struct ParallelCopyPlan {
    index: usize,
    source_path: PathBuf,
    target_location: Location,
    written_path: PathBuf,
}

/// The bulk path's plans and worker count, or `None` when an item needs the
/// sequential path.
async fn parallel_copy_plans(
    request: &PasteRequest,
    sources: &[gio::File],
    destination: &gio::File,
    regular_files: Vec<bool>,
    fat_family: bool,
) -> Option<(Vec<ParallelCopyPlan>, usize)> {
    if request.items.len() <= MAX_LOCAL_IO_WORKERS
        || !destination.is_native()
        || request
            .items
            .iter()
            .any(|item| item.conflict != TransferConflict::FailIfExists)
    {
        return None;
    }

    let (sources, destination) = (sources.to_vec(), destination.clone());
    gio::spawn_blocking(move || {
        let mut used_names = HashSet::new();
        let plans = sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                if !source.is_native() || regular_files.get(index) != Some(&true) {
                    return None;
                }
                let target =
                    default_transfer_target(source, &destination, fat_family, &mut used_names)?;
                if transfer_is_noop(source, &destination, &target) {
                    return None;
                }
                Some(ParallelCopyPlan {
                    index,
                    source_path: source.path()?,
                    target_location: location_for_file(&target)?,
                    written_path: target.path()?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let available = thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        // A rotational disk on either side reads or writes one file at a time.
        let source_folders: HashSet<&Path> = plans
            .iter()
            .filter_map(|plan| plan.source_path.parent())
            .collect();
        let rotational = destination
            .path()
            .and_then(|path| std::fs::File::open(path).ok())
            .and_then(|directory| local_device_is_rotational(&directory))
            .unwrap_or(fat_family)
            || source_folders.into_iter().any(|folder| {
                open_local_parent_directory(folder)
                    .ok()
                    .and_then(local_device_is_rotational)
                    == Some(true)
            });
        Some((plans, bounded_local_worker_count(available, rotational)))
    })
    .await
    .ok()
    .flatten()
}

struct ParallelCopyOutcome {
    failed: Vec<(usize, glib::Error)>,
    next: usize,
    written_paths: Vec<PathBuf>,
}

/// Copies or moves the planned items a chunk at a time; items done join
/// `settling`, which stops the path when a batch it closed failed.
async fn execute_parallel_copy_plans(
    plans: &[ParallelCopyPlan],
    (item_sizes, item_files): (&[Option<u64>], &[Option<usize>]),
    (worker_count, moving): (usize, bool),
    cancellable: &gio::Cancellable,
    copying: &Copying,
    settling: &mut Settling,
) -> ParallelCopyOutcome {
    let mut outcome = ParallelCopyOutcome {
        failed: Vec::new(),
        next: 0,
        written_paths: Vec::new(),
    };
    let last = plans.last().map(|plan| plan.index);
    while outcome.next < plans.len() && !cancellable.is_cancelled() {
        let end = (outcome.next + worker_count).min(plans.len());
        let chunk = &plans[outcome.next..end];
        outcome
            .written_paths
            .extend(chunk.iter().map(|plan| plan.written_path.clone()));
        let context = glib::MainContext::default();
        let tasks = chunk
            .iter()
            .cloned()
            .map(|plan| {
                let (cancellable, copying) = (cancellable.clone(), copying.clone());
                context.spawn_local(async move {
                    if moving {
                        let source = gio::File::for_path(&plan.source_path);
                        let target = gio::File::for_path(&plan.written_path);
                        return move_owing(source, target, Taken::Fail, cancellable, copying).await;
                    }
                    let copied = copy_engine::copy_local(
                        plan.source_path,
                        copy_engine::Destination::New(plan.written_path.clone(), Taken::Fail),
                        copy_engine::Options::default(),
                        copying.progress,
                        &cancellable,
                    )
                    .await?;
                    Ok(Transferred {
                        target: gio::File::for_path(plan.written_path),
                        reported: copied.reported,
                        owed: Owed::default(),
                    })
                })
            })
            .collect::<Vec<_>>();
        for (plan, task) in chunk.iter().zip(tasks) {
            match task.await {
                Ok(Ok(transferred)) => {
                    if let Some(progress) = &copying.progress {
                        progress.finish_item(
                            transferred.reported,
                            item_sizes[plan.index],
                            item_files[plan.index],
                            (!moving).then(|| plan.target_location.clone()),
                        );
                    }
                    let done = Done {
                        index: plan.index,
                        moved_to: location_for_file(&transferred.target),
                    };
                    let copied = transferred.reported.bytes;
                    let is_last = Some(plan.index) == last;
                    settling.owe(done, transferred.owed, copied, is_last).await;
                }
                Ok(Err(error)) => outcome.failed.push((plan.index, error)),
                Err(_) => outcome
                    .failed
                    .push((plan.index, io_error("Copy worker failed"))),
            }
        }
        outcome.next = end;
        if !outcome.failed.is_empty() || settling.failure().is_some() {
            break;
        }
    }
    outcome
}

fn parse_copy_suffix(stem: &OsStr) -> (&OsStr, Option<u64>) {
    let bytes = stem.as_bytes();
    if let Some(without_closing_parenthesis) = bytes.strip_suffix(b")")
        && let Some(separator) = without_closing_parenthesis
            .windows(b" (".len())
            .rposition(|window| window == b" (")
    {
        let suffix = &without_closing_parenthesis[separator + b" (".len()..];
        if !suffix.is_empty()
            && suffix[0] != b'0'
            && suffix.iter().all(u8::is_ascii_digit)
            && let Ok(suffix) = std::str::from_utf8(suffix)
            && let Ok(number) = suffix.parse::<u64>()
            && number < u64::MAX
        {
            return (OsStr::from_bytes(&bytes[..separator]), Some(number));
        }
    }
    (stem, None)
}

fn duplicate_candidate_name(
    base_stem: &OsStr,
    extension: Option<&OsStr>,
    copy_number: u64,
) -> OsString {
    let mut candidate = base_stem.as_bytes().to_vec();
    candidate.extend_from_slice(b" (");
    candidate.extend_from_slice(copy_number.to_string().as_bytes());
    candidate.push(b')');
    if let Some(extension) = extension {
        candidate.push(b'.');
        candidate.extend_from_slice(extension.as_bytes());
    }
    OsString::from_vec(candidate)
}

/// Keep Both's names for `name`: "name (1)", or one past the number it has,
/// with a file's extension kept, wrapping past the largest number; at most
/// `NUMBERED_NAME_ATTEMPTS` of them.
fn numbered_names(name: &OsStr, is_directory: bool) -> impl Iterator<Item = OsString> + '_ {
    let (stem, extension) = if is_directory {
        (name, None)
    } else {
        let path = Path::new(name);
        let extension = path.extension().filter(|extension| !extension.is_empty());
        (path.file_stem().unwrap_or(name), extension)
    };
    let (base_stem, copy_num) = parse_copy_suffix(stem);
    let start = copy_num.map_or(1, |number| number + 1);
    (start..=u64::MAX)
        .chain(1..start)
        .take(placement::NUMBERED_NAME_ATTEMPTS as usize)
        .map(move |index| duplicate_candidate_name(base_stem, extension, index))
}

/// What a new item does when its name is taken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Taken {
    Fail,
    /// Keep Both: the item takes the first free numbered name instead.
    Number,
}

impl Taken {
    /// The names a new item tries, in order.
    fn names(self, name: &OsStr, is_directory: bool) -> impl Iterator<Item = OsString> + '_ {
        let (plain, numbered) = match self {
            Self::Fail => (Some(name.to_os_string()), None),
            Self::Number => (None, Some(numbered_names(name, is_directory))),
        };
        plain.into_iter().chain(numbered.into_iter().flatten())
    }
}

/// Runs `attempt` at `target`, or under [`Taken::Number`] at each numbered
/// name beside it that a probe off the main thread finds free, until one is
/// not taken meanwhile.
async fn first_free_target<T, F: Future<Output = Result<T, glib::Error>>>(
    target: &gio::File,
    taken: Taken,
    is_directory: bool,
    cancellable: &gio::Cancellable,
    mut attempt: impl FnMut(gio::File) -> F,
) -> Result<(gio::File, T), glib::Error> {
    if taken == Taken::Fail {
        return attempt(target.clone())
            .await
            .map(|value| (target.clone(), value));
    }
    if let (Some(parent), Some(name)) = (target.parent(), target.basename()) {
        for name in numbered_names(name.as_os_str(), is_directory) {
            cancellable.set_error_if_cancelled()?;
            let candidate = parent.child(&name);
            if query_exists(&candidate, cancellable).await {
                continue;
            }
            match attempt(candidate.clone()).await {
                Err(error) if error.matches(gio::IOErrorEnum::Exists) => {}
                result => return result.map(|value| (candidate, value)),
            }
        }
    }
    cancellable.set_error_if_cancelled()?;
    Err(translated_io_error(
        "Could not find an unused duplicate name",
    ))
}

const FAT_INVALID_BYTES: &[u8] = b"\"*/:<>?\\|";

fn fat_sanitized_name(name: &OsStr) -> OsString {
    let mut bytes: Vec<u8> = name
        .as_bytes()
        .iter()
        .map(|&byte| {
            if FAT_INVALID_BYTES.contains(&byte) || byte < 0x20 {
                b'_'
            } else {
                byte
            }
        })
        .collect();
    // FAT drivers strip trailing dots and spaces.
    while matches!(bytes.last(), Some(b'.' | b' ')) {
        bytes.pop();
    }
    if bytes.is_empty() {
        bytes.push(b'_');
    }
    OsString::from_vec(bytes)
}

fn fat_name_key(name: &OsStr) -> OsString {
    match name.to_str() {
        Some(name) => OsString::from(name.to_uppercase()),
        None => OsString::from_vec(name.as_bytes().to_ascii_uppercase()),
    }
}

fn unique_fat_sibling_name(candidate: OsString, used: &mut HashSet<OsString>) -> OsString {
    if used.insert(fat_name_key(&candidate)) {
        return candidate;
    }
    let numbered = numbered_names(&candidate, false).find(|name| used.insert(fat_name_key(name)));
    numbered.unwrap_or(candidate)
}

fn fat_family_child_name(name: &OsStr, fat_family: bool, used: &mut HashSet<OsString>) -> OsString {
    if fat_family {
        unique_fat_sibling_name(fat_sanitized_name(name), used)
    } else {
        name.to_os_string()
    }
}

/// What a recursive GIO copy meets at its target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyInto {
    /// Nothing: every entry is created, and a taken name fails.
    New,
    /// Entries that may exist, which a merge copies over and into.
    Existing,
}

impl CopyInto {
    fn file_flags(self) -> gio::FileCopyFlags {
        let overwrite = if self == Self::Existing {
            gio::FileCopyFlags::OVERWRITE
        } else {
            gio::FileCopyFlags::NONE
        };
        gio::FileCopyFlags::ALL_METADATA | overwrite
    }

    /// Whether the copy makes the target folder, probing only for a merge.
    async fn makes_folder(self, target: &gio::File, cancellable: &gio::Cancellable) -> bool {
        match self {
            Self::New => true,
            Self::Existing => !query_exists(target, cancellable).await,
        }
    }
}

/// `g_file_query_exists` off the calling thread.
async fn query_exists(file: &gio::File, cancellable: &gio::Cancellable) -> bool {
    await_cancellable(file, cancellable, |file, cancellable, result| {
        file.query_info_async(
            gio::FILE_ATTRIBUTE_STANDARD_TYPE,
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
            Some(cancellable),
            move |output| result.resolve(output),
        );
    })
    .await
    .is_ok()
}

fn target_is_fat_family(target: &gio::File, mounts: &MountTable) -> bool {
    target
        .path()
        .is_some_and(|path| matches!(mounts.fs_type_for(&path), Some("msdos" | "vfat" | "exfat")))
}

async fn copy_children(
    enumerator: &gio::FileEnumerator,
    cancellable: &gio::Cancellable,
    fat_family: bool,
) -> Result<Vec<gio::FileInfo>, glib::Error> {
    let mut children = Vec::new();
    loop {
        let batch = await_cancellable(
            enumerator,
            cancellable,
            |enumerator, cancellable, result| {
                enumerator.next_files_async(
                    64,
                    glib::Priority::DEFAULT,
                    Some(cancellable),
                    move |output| result.resolve(output),
                );
            },
        )
        .await?;
        let finished = batch.is_empty();
        children.extend(batch);
        if !fat_family || finished {
            break;
        }
    }
    if fat_family {
        // Planning and copying must allocate collision suffixes in the same order.
        children.sort_by_key(|child| child.name());
    }
    Ok(children)
}

fn was_cancelled(error: &glib::Error) -> bool {
    error.matches(gio::IOErrorEnum::Cancelled)
}

/// Whether a delete failure is retryable as a permanent delete: it was a
/// trash attempt (never a permanent one, which has no further fallback),
/// and the destination doesn't support Trash at all rather than some other,
/// unrelated failure.
fn is_trash_unsupported_failure(permanent: bool, error: &glib::Error) -> bool {
    !permanent && error.matches(gio::IOErrorEnum::NotSupported)
}

fn cancelled_local_operation() -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Cancelled, "Operation cancelled")
}

async fn run_local_fs_step<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, glib::Error> {
    run_blocking(move || work().map_err(io_error)).await
}

fn open_local_child_directory<Fd: AsFd>(parent: &Fd, name: &OsStr) -> Result<OwnedFd, String> {
    // RESOLVE_NO_SYMLINKS (stronger than O_NOFOLLOW) plus RESOLVE_BENEATH and
    // RESOLVE_NO_MAGICLINKS: if `name` changed to a symlink (or a magic
    // link) since it was last inspected, this fails closed instead of
    // opening whatever it now points to.
    retry_local_open(|| {
        rustix::fs::openat2(
            parent,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::BENEATH
                | rustix::fs::ResolveFlags::NO_SYMLINKS
                | rustix::fs::ResolveFlags::NO_MAGICLINKS,
        )
    })
    .map_err(|error| {
        rust_i18n::t!(
            "%{name} changed while it was being read: %{error}",
            name = name.to_string_lossy(),
            error = os_error_detail(error)
        )
        .into_owned()
    })
}

fn local_directory_children<Fd: AsFd>(handle: &Fd) -> Result<Vec<OsString>, String> {
    let mut children = Vec::new();
    for entry in rustix::fs::Dir::read_from(handle).map_err(os_error_text)? {
        let entry = entry.map_err(os_error_text)?;
        let entry_name = entry.file_name();
        if entry_name == c"." || entry_name == c".." {
            continue;
        }
        children.push(OsString::from_vec(entry_name.to_bytes().to_vec()));
    }
    Ok(children)
}

struct CreatedCopyRoot {
    was_created: Cell<bool>,
    identity: Cell<Option<LocalFileIdentity>>,
}

impl CreatedCopyRoot {
    fn new() -> Self {
        Self {
            was_created: Cell::new(false),
            identity: Cell::new(None),
        }
    }
}

/// Makes a copy's target folder, recording it when it is the copy's root.
/// The synchronous call reports a folder it made even if the copy is
/// cancelled meanwhile, where `make_directory_async` reports only the cancel
/// and the folder would be left behind.
async fn make_copy_folder(
    target: &gio::File,
    cancellable: &gio::Cancellable,
    created_root: &Option<Rc<CreatedCopyRoot>>,
) -> Result<(), glib::Error> {
    let (folder, cancellable) = (target.clone(), cancellable.clone());
    run_blocking(move || folder.make_directory(Some(&cancellable))).await?;
    if let Some(created_root) = created_root {
        created_root.was_created.set(true);
        created_root
            .identity
            .set(local_file_identity(target).await?);
    }
    Ok(())
}

type RemoteFileStageCopy = Rc<
    dyn Fn(
        gio::File,
        gio::File,
        gio::Cancellable,
    ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>>,
>;

type RemoteFileStageCommit = Rc<
    dyn Fn(
        gio::File,
        gio::File,
        gio::Cancellable,
    ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>>,
>;

async fn discard_incomplete_copy(stage: gio::File) -> Result<(), glib::Error> {
    match permanently_delete(stage, false, gio::Cancellable::new()).await {
        Err(error) if error.matches(gio::IOErrorEnum::NotFound) => Ok(()),
        result => result,
    }
}

/// Reserves an unpredictable sibling so a partial remote file is never exposed at the final path.
async fn create_remote_file_stage(
    target: &gio::File,
    cancellable: &gio::Cancellable,
) -> Result<gio::File, glib::Error> {
    let parent = target
        .parent()
        .ok_or_else(|| translated_io_error("The destination has no parent directory"))?;
    let stage = parent.child(format!("{COPY_PREFIX}{}", glib::uuid_string_random()));
    let stream = match await_cancellable(&stage, cancellable, |stage, cancellable, result| {
        stage.create_async(
            gio::FileCreateFlags::PRIVATE,
            glib::Priority::DEFAULT,
            Some(cancellable),
            move |output| result.resolve(output),
        );
    })
    .await
    {
        Ok(stream) => stream,
        Err(error) if was_cancelled(&error) => {
            let cleanup = discard_incomplete_copy(stage).await;
            return Err(copy_failure_after_cleanup(error, cleanup));
        }
        Err(error) => return Err(error),
    };
    if let Err(error) = await_cancellable(&stream, cancellable, |stream, cancellable, result| {
        stream.close_async(glib::Priority::DEFAULT, Some(cancellable), move |output| {
            result.resolve(output)
        });
    })
    .await
    {
        let cleanup = discard_incomplete_copy(stage).await;
        return Err(copy_failure_after_cleanup(error, cleanup));
    }
    Ok(stage)
}

fn copy_failure_after_cleanup(
    copy_error: glib::Error,
    cleanup_result: Result<(), glib::Error>,
) -> glib::Error {
    match cleanup_result {
        Ok(()) => copy_error,
        Err(cleanup_error) => io_error(rust_i18n::t!(
            "%{error}; the incomplete copy could not be removed: %{cleanup_error}",
            error = crate::services::gio_error_message(&copy_error),
            cleanup_error = crate::services::gio_error_detail(&cleanup_error)
        )),
    }
}

async fn copy_new_remote_file_with(
    source: gio::File,
    target: gio::File,
    taken: Taken,
    cancellable: gio::Cancellable,
    copy_to_stage: RemoteFileStageCopy,
    commit_stage: RemoteFileStageCommit,
) -> Result<gio::File, glib::Error> {
    let stage = create_remote_file_stage(&target, &cancellable).await?;
    if let Err(error) = copy_to_stage(source, stage.clone(), cancellable.clone()).await {
        let cleanup = discard_incomplete_copy(stage).await;
        return Err(copy_failure_after_cleanup(error, cleanup));
    }
    if let Err(error) = cancellable.set_error_if_cancelled() {
        let cleanup = discard_incomplete_copy(stage).await;
        return Err(copy_failure_after_cleanup(error, cleanup));
    }
    let committed = first_free_target(&target, taken, false, &cancellable, |target| {
        commit_stage(stage.clone(), target, cancellable.clone())
    })
    .await;
    match committed {
        Ok((target, ())) => Ok(target),
        Err(error) => {
            let cleanup = discard_incomplete_copy(stage).await;
            Err(copy_failure_after_cleanup(error, cleanup))
        }
    }
}

fn copy_recursively_with_progress(
    source: gio::File,
    target: gio::File,
    into: CopyInto,
    cancellable: gio::Cancellable,
    created_root: Option<Rc<CreatedCopyRoot>>,
    progress: Option<Rc<TransferProgressTracker>>,
    fat_family: bool,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    Box::pin(async move {
        let info = await_cancellable(&source, &cancellable, |source, cancellable, result| {
            source.query_info_async(
                "standard::type,standard::size",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(cancellable),
                move |output| result.resolve(output),
            );
        })
        .await?;
        if info.file_type() == gio::FileType::Directory {
            if into.makes_folder(&target, &cancellable).await {
                make_copy_folder(&target, &cancellable, &created_root).await?;
            }
            let enumerator =
                await_cancellable(&source, &cancellable, |source, cancellable, result| {
                    source.enumerate_children_async(
                        "standard::name",
                        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                        glib::Priority::DEFAULT,
                        Some(cancellable),
                        move |output| result.resolve(output),
                    );
                })
                .await?;
            let mut used_names = HashSet::new();
            loop {
                let children = copy_children(&enumerator, &cancellable, fat_family).await?;
                if children.is_empty() {
                    break;
                }
                for child in children {
                    let child_name = child.name();
                    let target_name =
                        fat_family_child_name(child_name.as_os_str(), fat_family, &mut used_names);
                    copy_recursively_with_progress(
                        source.child(&child_name),
                        target.child(&target_name),
                        into,
                        cancellable.clone(),
                        None,
                        progress.clone(),
                        fat_family,
                    )
                    .await?;
                }
                if fat_family {
                    break;
                }
            }
            Ok(())
        } else {
            let source_size = info
                .has_attribute(gio::FILE_ATTRIBUTE_STANDARD_SIZE)
                .then(|| info.size().max(0) as u64);
            let flags = into.file_flags() | gio::FileCopyFlags::NOFOLLOW_SYMLINKS;
            let track_bytes = source_size.is_none_or(|size| size >= BYTE_PROGRESS_MIN_FILE_SIZE);
            let file_progress = progress.as_ref().map(|progress| {
                progress.begin_file(
                    transfer_source_name(&source)
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    track_bytes,
                )
            });
            let reporter = file_progress
                .as_ref()
                .and_then(FileTransferProgress::reporter);
            let result =
                copy_file_with_progress(source, target, flags, cancellable.clone(), reporter).await;
            if result.is_ok()
                && let Some(file_progress) = file_progress
            {
                file_progress.finish(source_size);
            }
            result
        }
    })
}

/// A copy to the new `target`, or with `merge` into the folder `target`.
#[cfg(test)]
fn copy_recursively(
    source: gio::File,
    target: gio::File,
    merge: bool,
    cancellable: gio::Cancellable,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    Box::pin(async move {
        if merge {
            return merge_copy(None, false)(source, target, MergeCopy::default(), cancellable)
                .await
                .map(drop);
        }
        copy_new_recursively_on_filesystem(source, target, Taken::Fail, cancellable, None, false)
            .await
            .map(drop)
    })
}

/// Copies `source` to the free name `target`, or under [`Taken::Number`] to
/// the first free numbered name beside it, and returns where it went.
async fn copy_new_recursively_on_filesystem(
    source: gio::File,
    target: gio::File,
    taken: Taken,
    cancellable: gio::Cancellable,
    progress: Option<Rc<TransferProgressTracker>>,
    fat_family: bool,
) -> Result<gio::File, glib::Error> {
    if source.is_native()
        && target.is_native()
        && let (Some(source_path), Some(target_path)) = (source.path(), target.path())
    {
        let options = copy_engine::Options {
            fat_family,
            ..copy_engine::Options::default()
        };
        let copied = copy_engine::copy_local(
            source_path,
            copy_engine::Destination::New(target_path.clone(), taken),
            options,
            progress,
            &cancellable,
        )
        .await?;
        return Ok(gio::File::for_path(target_path.with_file_name(copied.name)));
    }

    let source_type = if !target.is_native() || taken == Taken::Number {
        let info = await_cancellable(&source, &cancellable, |source, cancellable, result| {
            source.query_info_async(
                "standard::type",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(cancellable),
                move |output| result.resolve(output),
            );
        })
        .await?;
        Some(info.file_type())
    } else {
        None
    };
    let is_directory = source_type == Some(gio::FileType::Directory);
    if !target.is_native() && !is_directory {
        let copy_progress = progress.clone();
        return copy_new_remote_file_with(
            source,
            target,
            taken,
            cancellable,
            Rc::new(move |source, stage, cancellable| {
                let progress = copy_progress.clone();
                Box::pin(async move {
                    copy_recursively_with_progress(
                        source,
                        stage,
                        CopyInto::Existing,
                        cancellable,
                        None,
                        progress,
                        fat_family,
                    )
                    .await
                })
            }),
            Rc::new(|stage, target, cancellable| {
                Box::pin(async move {
                    await_cancellable(&stage, &cancellable, move |stage, cancellable, result| {
                        stage.move_async(
                            &target,
                            gio::FileCopyFlags::NO_FALLBACK_FOR_MOVE,
                            glib::Priority::DEFAULT,
                            Some(cancellable),
                            None,
                            move |output| result.resolve(output),
                        );
                    })
                    .await
                })
            }),
        )
        .await;
    }

    let copied = first_free_target(&target, taken, is_directory, &cancellable, |target| {
        let (source, cancellable, progress) =
            (source.clone(), cancellable.clone(), progress.clone());
        async move {
            let created_root = Rc::new(CreatedCopyRoot::new());
            let result = copy_recursively_with_progress(
                source,
                target.clone(),
                CopyInto::New,
                cancellable.clone(),
                Some(created_root.clone()),
                progress,
                fat_family,
            )
            .await;
            match result {
                Err(error) if was_cancelled(&error) && created_root.was_created.get() => {
                    let cleanup_result = permanently_delete_maybe_local_if_unchanged(
                        target,
                        true,
                        created_root.identity.get(),
                        gio::Cancellable::new(),
                    )
                    .await;
                    Err(copy_failure_after_cleanup(error, cleanup_result))
                }
                // Taken inside a folder this copy made, where no other
                // name for the folder helps.
                Err(error)
                    if taken == Taken::Number
                        && error.matches(gio::IOErrorEnum::Exists)
                        && created_root.was_created.get() =>
                {
                    Err(io_error(crate::services::gio_error_message(&error)))
                }
                result => result,
            }
        }
    })
    .await?;
    Ok(copied.0)
}

/// How a move attempt ended: done at the given target, or the item must be
/// copied and the source deleted afterwards.
enum Moved {
    Done(gio::File),
    Copy,
}

type MoveAttempt = Rc<
    dyn Fn(
        gio::File,
        gio::File,
        Taken,
        gio::Cancellable,
    ) -> Pin<Box<dyn Future<Output = Result<Moved, glib::Error>>>>,
>;

/// Copies a move's native source to a target GIO writes, and returns where
/// it went.
type RemoteCopy = Rc<
    dyn Fn(
        gio::File,
        gio::File,
        Taken,
        gio::Cancellable,
    ) -> Pin<Box<dyn Future<Output = Result<gio::File, glib::Error>>>>,
>;

fn native_path(file: &gio::File) -> Option<PathBuf> {
    file.is_native().then(|| file.path()).flatten()
}

/// What one item of a transfer became, what it added to progress, and what
/// it owes once its copy is on disk.
struct Transferred {
    target: gio::File,
    reported: Reported,
    owed: Owed,
}

/// Moves `source` to `target` as [`move_local`] does, and returns where it
/// went with what it owes.
async fn move_local_owing(
    source: gio::File,
    target: gio::File,
    taken: Taken,
    cancellable: gio::Cancellable,
    copying: Copying,
    attempt_move: MoveAttempt,
    remote_copy: RemoteCopy,
) -> Result<Transferred, glib::Error> {
    if let Moved::Done(target) =
        attempt_move(source.clone(), target.clone(), taken, cancellable.clone()).await?
    {
        return Ok(Transferred {
            target,
            reported: Reported::default(),
            owed: Owed::default(),
        });
    }
    let Copying {
        progress,
        fat_family,
    } = copying;
    let mut owed = Owed::default();
    if let Some(target_path) = native_path(&target) {
        owed.barrier = Some(copy_engine::barrier(split_local_path(&target_path)?.0).await?);
    }
    let (target, reported, deletion) = match (native_path(&source), native_path(&target)) {
        (Some(source_path), Some(target_path)) => {
            let options = copy_engine::Options {
                fat_family,
                manifest: true,
                durable: true,
                ..copy_engine::Options::default()
            };
            let copied = copy_engine::copy_local(
                source_path,
                copy_engine::Destination::New(target_path.clone(), taken),
                options,
                progress.clone(),
                &cancellable,
            )
            .await?;
            let target = gio::File::for_path(target_path.with_file_name(copied.name));
            let deletion = match copied.manifest {
                Some(sources) => Deletion::Sources(source, sources),
                None => Deletion::Source(source),
            };
            (target, copied.reported, deletion)
        }
        // Recorded before GIO copies it, so a change during the copy shows.
        (Some(source_path), None) => {
            let sources = copy_engine::snapshot(source_path, &cancellable).await?;
            let target = remote_copy(source.clone(), target, taken, cancellable.clone()).await?;
            (
                target,
                Reported::default(),
                Deletion::Sources(source, sources),
            )
        }
        _ => {
            let target = copy_new_recursively_on_filesystem(
                source.clone(),
                target,
                taken,
                cancellable.clone(),
                progress.clone(),
                fat_family,
            )
            .await?;
            if let Some(path) = native_path(&target)
                && owed
                    .barrier
                    .as_ref()
                    .is_some_and(copy_engine::Barrier::syncs_each_file)
            {
                copy_engine::sync_files(vec![path], &cancellable).await?;
            }
            (target, Reported::default(), Deletion::Source(source))
        }
    };
    owed.owe(deletion);
    Ok(Transferred {
        target,
        reported,
        owed,
    })
}

/// Moves `source` to `target` as [`move_local`] does, deletes what it owes
/// at once, and returns where it went.
async fn move_local_with_progress(
    source: gio::File,
    target: gio::File,
    taken: Taken,
    cancellable: gio::Cancellable,
    copying: Copying,
    attempt_move: MoveAttempt,
    remote_copy: RemoteCopy,
) -> Result<gio::File, glib::Error> {
    let progress = copying.progress.clone();
    let moved = move_local_owing(
        source,
        target,
        taken,
        cancellable.clone(),
        copying,
        attempt_move,
        remote_copy,
    )
    .await?;
    moved.owed.settle(progress.as_deref(), &cancellable).await?;
    Ok(moved.target)
}

/// Deletes what the copy of `source` holds unchanged, and reports what stays.
async fn delete_moved_sources(
    source: &gio::File,
    sources: copy_engine::Manifest,
    unlinks: Arc<copy_engine::Unlinks>,
    progress: Option<&TransferProgressTracker>,
    cancellable: &gio::Cancellable,
) -> Result<(), glib::Error> {
    let (kept, deleted) =
        copy_engine::delete_unchanged_sources(sources, unlinks, cancellable).await;
    if let (Some(progress), Some(parent)) =
        (progress, source.path().as_deref().and_then(Path::parent))
    {
        progress.report(TransferReport {
            kept: kept_items(parent, kept),
            ..TransferReport::default()
        });
    }
    deleted
}

/// What a deletion through a manifest kept, by path beneath `parent`.
fn kept_items(parent: &Path, kept: Vec<(PathBuf, KeptCause)>) -> Vec<KeptItem> {
    kept.into_iter()
        .map(|(path, cause)| KeptItem {
            location: Location::local(parent.join(path)),
            cause,
        })
        .collect()
}

#[cfg(test)]
async fn move_local_with(
    source: gio::File,
    target: gio::File,
    cancellable: gio::Cancellable,
    attempt_move: MoveAttempt,
) -> Result<gio::File, glib::Error> {
    move_local_with_progress(
        source,
        target,
        Taken::Fail,
        cancellable,
        Copying::default(),
        attempt_move,
        remote_copy(Copying::default()),
    )
    .await
}

/// Standalone reason for a placement failure, for a `…: %{error}` slot.
fn placement_reason(error: PlaceError) -> String {
    match error {
        PlaceError::Exists => crate::i18n::tr("An item with that name already exists"),
        PlaceError::InvalidName => crate::i18n::tr(INVALID_NAME_REASON),
        PlaceError::IntoItself => crate::i18n::tr("Can\u{2019}t put a folder inside itself"),
        PlaceError::Changed => {
            crate::i18n::tr("The target changed while the operation was in progress")
        }
        PlaceError::Unconfirmed => crate::i18n::tr(UNCONFIRMED_REASON),
        PlaceError::CrossDevice => os_error_text(rustix::io::Errno::XDEV),
        PlaceError::Unsupported => crate::i18n::tr(UNSUPPORTED_REASON),
        PlaceError::Io(error) => os_error_text(error),
    }
}

fn placement_error_kind(error: PlaceError) -> gio::IOErrorEnum {
    match error {
        PlaceError::Exists => gio::IOErrorEnum::Exists,
        PlaceError::InvalidName => gio::IOErrorEnum::InvalidFilename,
        _ => gio::IOErrorEnum::Failed,
    }
}

/// A placement failure as a complete message about `name`, using `template`
/// (with `%{name}` and `%{error}`) unless the failure has its own sentence.
/// Its kind stays generic, so callers that reword known kinds keep the name.
fn placement_failure(
    error: PlaceError,
    name: &str,
    template: fn(&str, &str) -> String,
) -> glib::Error {
    let message = match error {
        PlaceError::IntoItself => placement_reason(error),
        PlaceError::Changed => rust_i18n::t!(
            "%{name} changed while the operation was in progress",
            name = name
        )
        .into_owned(),
        error => template(
            name,
            &crate::services::error_detail(placement_reason(error)),
        ),
    };
    io_error(message)
}

fn could_not_move(name: &str, error: &str) -> String {
    rust_i18n::t!(
        "Could not move %{name}: %{error}",
        name = name,
        error = error
    )
    .into_owned()
}

/// Gives the entries a move saved under other names their names back, now
/// that `record` is undone, where nothing else has taken them, and reports
/// those that keep theirs.
async fn restore_renamed(record: &MoveRecord, progress: &TransferProgressTracker) {
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

/// Renames a native item without replacing another, in one blocking step.
async fn rename_native(source: PathBuf, target: PathBuf) -> Result<(), glib::Error> {
    let (Some(source_parent), Some(source_name), Some(target_parent), Some(target_name)) = (
        source.parent().map(Path::to_path_buf),
        source.file_name().map(OsStr::to_os_string),
        target.parent().map(Path::to_path_buf),
        target.file_name().map(OsStr::to_os_string),
    ) else {
        return Err(translated_io_error("Invalid move destination"));
    };
    let name = source_name.to_string_lossy().into_owned();
    let new_name = target_name.to_string_lossy().into_owned();
    let outcome = run_local_fs_step(move || {
        let source_dir = open_local_parent_directory(&source_parent)?;
        let target_dir = if target_parent == source_parent {
            None
        } else {
            Some(open_local_parent_directory(&target_parent)?)
        };
        Ok(placement::rename_in_place(
            fs(),
            Entry::new(&source_dir, &source_name),
            Entry::new(target_dir.as_ref().unwrap_or(&source_dir), &target_name),
        ))
    })
    .await?;
    match outcome {
        Ok(_) => Ok(()),
        Err(RenameFailure::Place(PlaceError::Exists)) => Err(glib::Error::new(
            gio::IOErrorEnum::Exists,
            &rust_i18n::t!("“%{name}” already exists", name = new_name),
        )),
        Err(RenameFailure::Place(error)) => Err(placement_error(error)),
        Err(RenameFailure::Stranded { temporary, error }) => Err(io_error(rust_i18n::t!(
            "Could not rename “%{name}”; it was left as “%{temporary}”: %{error}",
            name = name,
            temporary = temporary.to_string_lossy(),
            error = crate::services::error_detail(placement_reason(error))
        ))),
    }
}

/// Pins both resolved parents and places the entry through the no-clobber
/// ladder in one blocking step, under [`Taken::Number`] at the first numbered
/// name not taken; a copy is allowed only once the target is known to be
/// outside the source.
fn move_local_path(
    source_path: PathBuf,
    target_path: PathBuf,
    taken: Taken,
    cancellable: gio::Cancellable,
) -> Pin<Box<dyn Future<Output = Result<Moved, glib::Error>>>> {
    Box::pin(async move {
        if cancellable.is_cancelled() {
            return Err(cancelled_local_operation());
        }
        let Some(source_parent_path) = source_path.parent().map(Path::to_path_buf) else {
            return Err(translated_io_error("Cannot move the filesystem root"));
        };
        let Some(source_name) = source_path.file_name().map(OsStr::to_os_string) else {
            return Err(translated_io_error("Invalid move source"));
        };
        let Some(target_parent_path) = target_path.parent().map(Path::to_path_buf) else {
            return Err(translated_io_error(
                "The move destination has no parent directory",
            ));
        };
        let Some(target_name) = target_path.file_name().map(OsStr::to_os_string) else {
            return Err(translated_io_error("Invalid move destination"));
        };

        let display_name = source_name.to_string_lossy().into_owned();
        let outcome = run_local_fs_step(move || {
            let source_parent = open_local_parent_directory(&source_parent_path)?;
            let target_parent = open_local_parent_directory(&target_parent_path)?;
            let from = Entry::new(&source_parent, &source_name);
            let is_directory = match taken {
                Taken::Fail => false,
                Taken::Number => match fs().stat(from, false) {
                    Ok(stat) => stat.is_directory(),
                    Err(error) => return Ok(Err(PlaceError::Io(error))),
                },
            };
            for name in taken.names(&target_name, is_directory) {
                let to = Entry::new(&target_parent, &name);
                let placed = placement::place(fs(), from, to, None);
                if taken == Taken::Number && matches!(placed, Err(PlaceError::Exists)) {
                    continue;
                }
                return Ok(match placed {
                    Ok(_) => Ok(Moved::Done(gio::File::for_path(
                        target_parent_path.join(&name),
                    ))),
                    Err(PlaceError::CrossDevice | PlaceError::Unsupported) => {
                        match fs().stat(from, false) {
                            Ok(stat)
                                if stat.is_directory()
                                    && placement::contains(fs(), stat.identity, to.dir)
                                        != Some(false) =>
                            {
                                Err(PlaceError::IntoItself)
                            }
                            Ok(_) => Ok(Moved::Copy),
                            Err(error) => Err(PlaceError::Io(error)),
                        }
                    }
                    Err(error) => Err(error),
                });
            }
            Ok(Err(PlaceError::Exists))
        })
        .await?;
        outcome.map_err(|error| placement_failure(error, &display_name, could_not_move))
    })
}

fn could_not_restore(name: &str, error: &str) -> String {
    rust_i18n::t!(
        "Could not restore %{name}: %{error}",
        name = name,
        error = error
    )
    .into_owned()
}

/// Moves the Trash item at `source_path` back to the free name
/// `target_path` beneath `allowed_root`, and returns what to report about
/// it.
async fn move_restore_path(
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

    // Nothing places the item or a stage here, so it is copied entry by
    // entry, and the Trash copy deleted once the copy is on disk.
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
        if error.matches(gio::IOErrorEnum::Exists) {
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

fn restore_destination_exists(display_name: &str) -> glib::Error {
    io_error(rust_i18n::t!(
        "Could not restore %{name}: something already exists at the destination",
        name = display_name
    ))
}

async fn move_restore(
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

/// Moves `source` to the free name `target`, or under [`Taken::Number`] to
/// the first free numbered name beside it, and returns where it went.
async fn move_local(
    source: gio::File,
    target: gio::File,
    taken: Taken,
    cancellable: gio::Cancellable,
    progress: Option<Rc<TransferProgressTracker>>,
    fat_family: bool,
) -> Result<gio::File, glib::Error> {
    let copying = Copying {
        progress: progress.clone(),
        fat_family,
    };
    move_local_with_progress(
        source,
        target,
        taken,
        cancellable,
        copying.clone(),
        attempt_move(progress),
        remote_copy(copying),
    )
    .await
}

/// Moves `source` to `target` as [`move_local`] does, and returns where it
/// went with what it owes.
async fn move_owing(
    source: gio::File,
    target: gio::File,
    taken: Taken,
    cancellable: gio::Cancellable,
    copying: Copying,
) -> Result<Transferred, glib::Error> {
    let (attempt, remote) = (
        attempt_move(copying.progress.clone()),
        remote_copy(copying.clone()),
    );
    move_local_owing(source, target, taken, cancellable, copying, attempt, remote).await
}

/// Moves an item in one step where that is safe, or tells it must be copied.
fn attempt_move(progress: Option<Rc<TransferProgressTracker>>) -> MoveAttempt {
    Rc::new(move |source, target, taken, cancellable| {
        if let (Some(source_path), Some(target_path)) = (native_path(&source), native_path(&target))
        {
            return move_local_path(source_path, target_path, taken, cancellable);
        }
        // GIO would delete a native source itself, without checking what it copied.
        if source.is_native() {
            return Box::pin(async { Ok(Moved::Copy) });
        }
        // Remote (GVfs) sources have no local descriptor to walk against, so
        // they keep the GIO path-based move below rather than claiming an
        // equivalent guarantee.
        let move_progress = progress.as_ref().map(|progress| {
            progress.begin_file(
                transfer_source_name(&source)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                true,
            )
        });
        Box::pin(async move {
            let is_directory = taken == Taken::Number
                && await_cancellable(&source, &cancellable, |source, cancellable, result| {
                    source.query_info_async(
                        "standard::type",
                        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                        glib::Priority::DEFAULT,
                        Some(cancellable),
                        move |output| result.resolve(output),
                    );
                })
                .await?
                .file_type()
                    == gio::FileType::Directory;
            let flags = gio::FileCopyFlags::ALL_METADATA | gio::FileCopyFlags::NOFOLLOW_SYMLINKS;
            let moved = first_free_target(&target, taken, is_directory, &cancellable, |target| {
                let reporter = move_progress
                    .as_ref()
                    .and_then(FileTransferProgress::reporter);
                move_file_with_progress(
                    source.clone(),
                    target,
                    flags,
                    cancellable.clone(),
                    reporter,
                )
            })
            .await;
            match moved {
                Ok((target, ())) => {
                    if let Some(move_progress) = move_progress {
                        move_progress.finish(None);
                    }
                    Ok(Moved::Done(target))
                }
                Err(error) if error.matches(gio::IOErrorEnum::WouldRecurse) => {
                    let (guard_source, guard_target) = (source.clone(), target.clone());
                    let within = run_local_fs_step(move || {
                        Ok(target_is_within_source(&guard_source, &guard_target))
                    })
                    .await?;
                    if within != Some(false) {
                        return Err(placement_error(PlaceError::IntoItself));
                    }
                    Ok(Moved::Copy)
                }
                Err(error) => Err(error),
            }
        })
    })
}

/// GIO's copy, for a native source moved to a target only GIO writes.
fn remote_copy(copying: Copying) -> RemoteCopy {
    Rc::new(move |source, target, taken, cancellable| {
        let Copying {
            progress,
            fat_family,
        } = copying.clone();
        Box::pin(copy_new_recursively_on_filesystem(
            source,
            target,
            taken,
            cancellable,
            progress,
            fat_family,
        ))
    })
}

enum StageContent {
    Directory,
    Symlink(OsString),
}

/// A hidden sibling that a copy is written into before it is placed at its
/// final name. Created and discarded off the main thread relative to its
/// pinned parent; there is no `Drop` cleanup.
struct StagedSibling {
    parent: Arc<OwnedFd>,
    name: OsString,
    identity: LocalFileIdentity,
    /// Folders inside that keep their owner out, opened up again before a discard.
    restrictive: Vec<copy_engine::Restrictive>,
}

impl StagedSibling {
    /// An empty folder or a symlink at a fresh `prefix` name in `parent`.
    fn create(
        parent: Arc<OwnedFd>,
        prefix: &str,
        content: StageContent,
    ) -> Result<Self, PlaceError> {
        let (name, identity) = placement::reserve_hidden_name(prefix, |name| {
            let entry = Entry::new(&*parent, name);
            let made = match &content {
                StageContent::Directory => {
                    fs().mkdir(entry, rustix::fs::Mode::from_raw_mode(0o700))
                }
                StageContent::Symlink(link) => fs().symlink(link, entry),
            };
            match made {
                Ok(()) => fs()
                    .stat(entry, false)
                    .map(|stat| stat.identity)
                    .map_err(PlaceError::Io),
                Err(rustix::io::Errno::EXIST) => {
                    placement::left_by_lost_reply(fs(), entry, |stat| match &content {
                        StageContent::Directory => {
                            stat.is_directory()
                                && fs().names(entry).is_ok_and(|names| names.is_empty())
                        }
                        StageContent::Symlink(link) => {
                            stat.kind == rustix::fs::FileType::Symlink
                                && fs().readlink(entry).is_ok_and(|current| current == *link)
                        }
                    })
                    .ok_or(PlaceError::Exists)
                }
                Err(error) => Err(PlaceError::Io(error)),
            }
        })?;
        Ok(Self {
            parent,
            name,
            identity,
            restrictive: Vec::new(),
        })
    }
}

async fn discard_incomplete_staged(staged: StagedSibling) -> Result<(), glib::Error> {
    let staged = if staged.restrictive.is_empty() {
        staged
    } else {
        run_blocking(move || {
            copy_engine::unlock_stage(&staged);
            Ok(staged)
        })
        .await?
    };
    parallel_delete_local(
        vec![PinnedEntry {
            parent: staged.parent,
            name: staged.name,
            identity: Some(staged.identity),
        }],
        gio::Cancellable::new(),
    )
    .await
}

fn placement_error(error: PlaceError) -> glib::Error {
    glib::Error::new(placement_error_kind(error), &placement_reason(error))
}

/// `durable`: what only the copy will hold is deleted once its barrier is
/// reached.
async fn publish_staged_without_replace(
    staged: StagedSibling,
    target_path: PathBuf,
    durable: bool,
    cancellable: gio::Cancellable,
) -> Result<(), glib::Error> {
    let Some(target_name) = target_path.file_name().map(OsStr::to_os_string) else {
        let cleanup = discard_incomplete_staged(staged).await;
        return Err(copy_failure_after_cleanup(
            translated_io_error("Invalid local filesystem target"),
            cleanup,
        ));
    };
    let parent = staged.parent.clone();
    let (stage_name, name) = (staged.name.clone(), target_name.clone());
    let recorded = staged.identity;
    let step = gio::spawn_blocking(move || {
        copy_engine::stage::publish(fs(), parent.as_fd(), &stage_name, recorded, &name)
    })
    .await;
    let placed = match step {
        Ok(step) => step,
        Err(error) => {
            let cleanup = discard_incomplete_staged(staged).await;
            return Err(copy_failure_after_cleanup(
                io_error(blocking_join_message(error)),
                cleanup,
            ));
        }
    };
    let result = match placed {
        Ok(_) => return Ok(()),
        // Where the ladder cannot place the stage, publication falls back to
        // exclusive creation, even though it exposes partial copies.
        Err(PlaceError::Unsupported) => {
            copy_engine::copy_from_stage(&staged, target_name, durable, &cancellable).await
        }
        // Either name may hold the copy, so neither is removed.
        Err(PlaceError::Unconfirmed) => return Err(placement_error(PlaceError::Unconfirmed)),
        Err(error) => Err(placement_error(error)),
    };
    let cleanup = discard_incomplete_staged(staged).await;
    match result {
        Ok(()) => cleanup.map_err(|error| {
            io_error(rust_i18n::t!(
                "The item was copied, but its staging copy could not be removed: %{error}",
                error = crate::services::gio_error_detail(&error)
            ))
        }),
        Err(error) => Err(copy_failure_after_cleanup(error, cleanup)),
    }
}

fn io_error(error: impl std::fmt::Display) -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Failed, &error.to_string())
}

fn os_error_text(error: impl Into<io::Error>) -> String {
    crate::services::io_error_message(&error.into())
}

fn os_error_detail(error: impl Into<io::Error>) -> String {
    crate::services::io_error_detail(&error.into())
}

fn os_io_error(error: impl Into<io::Error>) -> glib::Error {
    io_error(os_error_text(error))
}

fn translated_io_error(message: &str) -> glib::Error {
    io_error(crate::i18n::tr(message))
}

/// Copies a source into a target, as a merge copies into its folder. A move
/// (`true`) gets what the copy took from a native source.
type CopyStep = Rc<
    dyn Fn(
        gio::File,
        gio::File,
        MergeCopy,
        gio::Cancellable,
    ) -> Pin<Box<dyn Future<Output = Result<MergeCopied, glib::Error>>>>,
>;

/// What the deletions after a merge's copy need of it.
#[derive(Clone, Copy, Default)]
struct MergeCopy {
    /// The source is deleted, so the copy records what it took.
    moving: bool,
    /// See [`copy_engine::Options::durable`].
    durable: bool,
}

/// What a merge's copy leaves for the deletions after it.
#[derive(Default)]
struct MergeCopied {
    /// What it took from the source, for a move.
    sources: Option<copy_engine::Manifest>,
    /// See [`copy_engine::Copied::nested`].
    nested: Vec<copy_engine::Barrier>,
}

fn cannot_replace_safely(name: &str) -> String {
    rust_i18n::t!(
        "“%{name}” can’t be replaced safely at this location. Delete or rename it first, then try again.",
        name = name
    )
    .into_owned()
}

fn original_kept_aside(name: &str, kept: &OsStr, error: &str) -> String {
    rust_i18n::t!(
        "Could not replace “%{name}”; the original was kept as “%{kept}”: %{error}",
        name = name,
        kept = kept.to_string_lossy(),
        error = error
    )
    .into_owned()
}

fn original_not_removed(name: &str, kept: &OsStr, error: &glib::Error) -> String {
    rust_i18n::t!(
        "“%{name}” was replaced, but its original could not be removed from “%{kept}”: %{error}",
        name = name,
        kept = kept.to_string_lossy(),
        error = crate::services::gio_error_detail(error)
    )
    .into_owned()
}

fn could_not_replace(_name: &str, error: &str) -> String {
    rust_i18n::t!("Could not safely replace the item: %{error}", error = error).into_owned()
}

fn original_in_trash(error: &str) -> glib::Error {
    io_error(rust_i18n::t!(
        "Could not finish placing the replacement item; the original is in Trash: %{error}",
        error = error
    ))
}

async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, glib::Error> + Send + 'static,
) -> Result<T, glib::Error> {
    gio::spawn_blocking(work)
        .await
        .map_err(|_| io_error("Local filesystem task panicked"))?
}

/// How the original leaves the target name.
#[derive(Clone, Copy)]
enum Removal {
    Trash,
    /// Already in Trash; only the replacement is left to place.
    Trashed,
    Without(Displacement),
}

#[derive(Clone, Copy)]
enum ReplaceStrategy {
    /// A move within one filesystem renames the source itself onto the target.
    PlaceSource,
    Staged(Removal),
}

struct ReplacePlan {
    source_parent: Arc<OwnedFd>,
    source_name: OsString,
    source: EntryStat,
    target_parent: Arc<OwnedFd>,
    target_name: OsString,
    target: EntryStat,
    strategy: ReplaceStrategy,
}

impl ReplacePlan {
    fn source_entry(&self) -> Entry<'_> {
        Entry::new(&*self.source_parent, &self.source_name)
    }

    fn target_entry(&self) -> Entry<'_> {
        Entry::new(&*self.target_parent, &self.target_name)
    }

    fn target_display(&self) -> String {
        self.target_name.to_string_lossy().into_owned()
    }

    fn directory(&self) -> bool {
        self.source.is_directory()
    }

    fn trashed_original(&self) -> TrashedOriginal {
        self.target.identity.into()
    }

    /// A folder may only be copied once the target is known to lie outside it.
    fn copy_stays_outside_source(&self) -> bool {
        !self.directory()
            || placement::contains(fs(), self.source.identity, self.target_parent.as_fd())
                == Some(false)
    }
}

fn split_local_path(path: &Path) -> Result<(PathBuf, OsString), glib::Error> {
    path.parent()
        .zip(path.file_name())
        .map(|(parent, name)| (parent.to_path_buf(), name.to_os_string()))
        .ok_or_else(|| translated_io_error("Invalid local filesystem target"))
}

/// Pins both parents, compares the two items and chooses how the original
/// leaves before anything is copied. `None` means both names are one item.
fn plan_replace(
    source_path: &Path,
    target_path: &Path,
    move_source: bool,
) -> Result<Option<ReplacePlan>, glib::Error> {
    let (source_parent_path, source_name) = split_local_path(source_path)?;
    let (target_parent_path, target_name) = split_local_path(target_path)?;
    let source_parent = open_local_parent_directory(&source_parent_path).map_err(io_error)?;
    let target_parent = open_local_parent_directory(&target_parent_path).map_err(io_error)?;
    let inspect = |parent: &OwnedFd, name: &OsStr, path: &Path| {
        fs().stat(Entry::new(parent, name), false).map_err(|error| {
            io_error(rust_i18n::t!(
                "Could not inspect %{path}: %{error}",
                path = path.display(),
                error = os_error_detail(error)
            ))
        })
    };
    let source = inspect(&source_parent, &source_name, source_path)?;
    let target = inspect(&target_parent, &target_name, target_path)?;
    if source.is_directory() != target.is_directory() {
        return Err(glib::Error::new(
            gio::IOErrorEnum::NotSupported,
            &crate::i18n::tr("A file and a folder cannot safely replace one another"),
        ));
    }
    let target_directory = fs().stat_fd(target_parent.as_fd()).ok();
    let shared_parent = target_directory.is_some()
        && fs()
            .stat_fd(source_parent.as_fd())
            .ok()
            .map(|dir| dir.identity)
            == target_directory.map(|dir| dir.identity);
    // A hard link elsewhere in the folder is a separate name and is replaced.
    if shared_parent
        && source.identity == target.identity
        && (source_name == target_name
            || placement::alias_of(
                fs(),
                Entry::new(&source_parent, &source_name),
                Entry::new(&target_parent, &target_name),
            )
            .is_some())
    {
        return Ok(None);
    }
    let strategy = if move_source
        && fs().mount_key(source_parent.as_fd()) == fs().mount_key(target_parent.as_fd())
        && target_directory.is_some_and(|dir| dir.identity.device == source.identity.device)
    {
        ReplaceStrategy::PlaceSource
    } else if fs().can_trash(target_path) {
        ReplaceStrategy::Staged(Removal::Trash)
    } else {
        ReplaceStrategy::Staged(Removal::Without(displacement_without_trash(
            &target_parent,
            source.is_directory(),
            &target_name.to_string_lossy(),
        )?))
    };
    Ok(Some(ReplacePlan {
        source_parent: Arc::new(source_parent),
        source_name,
        source,
        target_parent: Arc::new(target_parent),
        target_name,
        target,
        strategy,
    }))
}

fn displacement_without_trash(
    parent: &OwnedFd,
    directory: bool,
    name: &str,
) -> Result<Displacement, glib::Error> {
    displacement_or_refusal(
        placement::displacement_without_trash(fs(), parent.as_fd(), directory),
        name,
    )
}

fn displacement_or_refusal(
    displacement: Result<Option<Displacement>, PlaceError>,
    name: &str,
) -> Result<Displacement, glib::Error> {
    match displacement {
        Ok(Some(displacement)) => Ok(displacement),
        Ok(None) => Err(io_error(cannot_replace_safely(name))),
        Err(error) => Err(placement_failure(error, name, could_not_replace)),
    }
}

fn ensure_unchanged(entry: Entry<'_>, expected: LocalFileIdentity) -> Result<(), glib::Error> {
    match fs().stat(entry, false) {
        Ok(stat) if stat.identity == expected => Ok(()),
        Ok(_) | Err(rustix::io::Errno::NOENT) => Err(translated_io_error(
            "The target changed while the operation was in progress",
        )),
        Err(error) => Err(os_io_error(error)),
    }
}

fn set_aside_failure(error: PlaceError, name: &str) -> glib::Error {
    match error {
        PlaceError::Unsupported => io_error(cannot_replace_safely(name)),
        PlaceError::Changed => {
            translated_io_error("The target changed while the operation was in progress")
        }
        error => placement_failure(error, name, could_not_replace),
    }
}

/// An original kept under a hidden name until its replacement is safe on disk.
struct DisplacedOriginal {
    parent: OffMain<Arc<OwnedFd>>,
    name: OsString,
    identity: LocalFileIdentity,
}

/// Deletes the original `name` replaced, once nothing else needs it.
async fn remove_displaced_original(
    original: DisplacedOriginal,
    name: &str,
) -> Result<(), glib::Error> {
    let kept = original.name.clone();
    parallel_delete_local(
        vec![PinnedEntry {
            parent: original.parent.into_inner(),
            name: original.name,
            identity: Some(original.identity),
        }],
        gio::Cancellable::new(),
    )
    .await
    .map_err(|error| io_error(original_not_removed(name, &kept, &error)))
}

enum SourcePlaced {
    Trashed,
    SetAside(placement::SetAside),
    /// The original is in Trash, but the source must be copied into place.
    CopyAfterTrash,
    /// The original is back at its name, and the source must be copied.
    PutBack,
}

/// Moves the original out of the way and renames the source onto its name.
fn place_source(
    plan: &ReplacePlan,
    target_path: &Path,
    cancellable: &gio::Cancellable,
) -> Result<SourcePlaced, glib::Error> {
    let target = plan.target_entry();
    let source = plan.source_entry();
    let expected = Some(plan.source.identity);
    ensure_unchanged(target, plan.target.identity)?;
    match trash::trash_verified(target, target_path, plan.target.identity, cancellable) {
        Ok(()) => {
            return match placement::place(fs(), source, target, expected) {
                Ok(_) => Ok(SourcePlaced::Trashed),
                Err(
                    error @ (PlaceError::Exists
                    | PlaceError::Changed
                    | PlaceError::Unconfirmed
                    | PlaceError::IntoItself),
                ) => Err(original_in_trash(&crate::services::error_detail(
                    placement_reason(error),
                ))),
                Err(_) if !plan.copy_stays_outside_source() => Err(original_in_trash(
                    &crate::services::error_detail(placement_reason(PlaceError::IntoItself)),
                )),
                Err(_) => Ok(SourcePlaced::CopyAfterTrash),
            };
        }
        Err(error) if is_trash_unsupported_failure(false, &error) => {}
        Err(error) => return Err(error),
    }
    let name = plan.target_display();
    let aside = placement::set_aside(fs(), target, REPLACED_PREFIX, plan.target.identity)
        .map_err(|error| set_aside_failure(error, &name))?;
    let error = match placement::place(fs(), source, target, expected) {
        Ok(_) => return Ok(SourcePlaced::SetAside(aside)),
        Err(error) => error,
    };
    if placement::put_back(fs(), plan.target_parent.as_fd(), &aside, &plan.target_name).is_err() {
        return Err(io_error(original_kept_aside(
            &name,
            &aside.name,
            &crate::services::error_detail(placement_reason(error)),
        )));
    }
    match error {
        PlaceError::CrossDevice | PlaceError::Unsupported if plan.copy_stays_outside_source() => {
            Ok(SourcePlaced::PutBack)
        }
        error => Err(placement_failure(
            error,
            &plan.source_name.to_string_lossy(),
            could_not_replace,
        )),
    }
}

struct StagedReplacement {
    original: ReplacedOriginal,
    displaced: Option<DisplacedOriginal>,
}

/// Copies the source next to the target, then moves the original out of the
/// way and publishes the copy. Every failure discards the stage. A move also
/// returns what the copy took from the source.
/// How a Replace copy reaches the disk before something only it holds is
/// deleted.
#[derive(Clone, Copy)]
struct Durability {
    /// See [`copy_engine::Options::durable`].
    durable: bool,
    /// The barrier reaches only files synced one by one.
    each_file: bool,
}

async fn replace_through_stage(
    plan: &Arc<ReplacePlan>,
    mut removal: Removal,
    (source_path, moving): (&Path, bool),
    target_path: &Path,
    cancellable: &gio::Cancellable,
    (
        copying,
        Durability {
            mut durable,
            each_file,
        },
    ): (Copying, Durability),
) -> Result<(StagedReplacement, StagedCopy), glib::Error> {
    let in_trash = |error: glib::Error, removal: Removal| match removal {
        Removal::Trashed => original_in_trash(&crate::services::gio_error_detail(&error)),
        _ => error,
    };
    let options = copy_engine::Options {
        fat_family: copying.fat_family,
        manifest: moving,
        durable,
        ..copy_engine::Options::default()
    };
    let (staged, sources, report) = copy_engine::copy_to_stage(
        source_path.to_path_buf(),
        target_path.to_path_buf(),
        options,
        copying.progress,
        cancellable,
    )
    .await
    .map_err(|error| in_trash(error, removal))?;
    let copied = (sources, report);
    loop {
        let parent = staged.parent.clone();
        let step_plan = plan.clone();
        let step = match removal {
            Removal::Trashed => {
                publish_staged_replacement(staged, target_path.to_path_buf(), durable).await?;
                let replaced = StagedReplacement {
                    original: ReplacedOriginal::Trashed(plan.trashed_original()),
                    displaced: None,
                };
                return Ok((replaced, copied));
            }
            Removal::Without(Displacement::SetAside) => {
                let replaced = set_aside_and_publish(plan, staged, target_path, durable).await?;
                return Ok((replaced, copied));
            }
            Removal::Trash => {
                let (path, cancellable) = (target_path.to_path_buf(), cancellable.clone());
                run_blocking(move || {
                    let target = Entry::new(&*parent, &step_plan.target_name);
                    ensure_unchanged(target, step_plan.target.identity)?;
                    match trash::trash_verified(
                        target,
                        &path,
                        step_plan.target.identity,
                        &cancellable,
                    ) {
                        Ok(()) => Ok(Removal::Trashed),
                        Err(error) if is_trash_unsupported_failure(false, &error) => {
                            displacement_without_trash(
                                &parent,
                                step_plan.directory(),
                                &step_plan.target_display(),
                            )
                            .map(Removal::Without)
                        }
                        Err(error) => Err(error),
                    }
                })
                .await
            }
            Removal::Without(Displacement::Exchange) => {
                let stage_name = staged.name.clone();
                let exchanged = run_blocking(move || {
                    let target = Entry::new(&*parent, &step_plan.target_name);
                    ensure_unchanged(target, step_plan.target.identity)?;
                    let name = step_plan.target_display();
                    match placement::exchange(fs(), Entry::new(&*parent, &stage_name), target) {
                        Ok(true) => Ok(None),
                        Ok(false) => displacement_or_refusal(
                            placement::set_aside_if_safe(
                                fs(),
                                parent.as_fd(),
                                step_plan.directory(),
                            ),
                            &name,
                        )
                        .map(|displacement| Some(Removal::Without(displacement))),
                        Err(error) => Err(placement_failure(error, &name, could_not_replace)),
                    }
                })
                .await;
                match exchanged {
                    Ok(None) => {
                        let replaced = StagedReplacement {
                            original: ReplacedOriginal::Deleted,
                            displaced: Some(DisplacedOriginal {
                                parent: OffMain::new(staged.parent),
                                name: staged.name,
                                identity: plan.target.identity,
                            }),
                        };
                        return Ok((replaced, copied));
                    }
                    Ok(Some(next)) => Ok(next),
                    Err(error) => Err(error),
                }
            }
        };
        let step = match step {
            // The original is deleted after all, once the stage is on disk.
            Ok(Removal::Without(displacement))
                if matches!(removal, Removal::Trash) && !durable && each_file =>
            {
                durable = true;
                let stage = target_path.with_file_name(&staged.name);
                copy_engine::sync_files(vec![stage], cancellable)
                    .await
                    .map(|()| Removal::Without(displacement))
            }
            step => step,
        };
        match step {
            Ok(next) => removal = next,
            Err(error) => {
                let cleanup = discard_incomplete_staged(staged).await;
                return Err(copy_failure_after_cleanup(error, cleanup));
            }
        }
    }
}

async fn set_aside_and_publish(
    plan: &Arc<ReplacePlan>,
    staged: StagedSibling,
    target_path: &Path,
    durable: bool,
) -> Result<StagedReplacement, glib::Error> {
    let name = plan.target_display();
    let parent = staged.parent.clone();
    let step_plan = plan.clone();
    let step_name = name.clone();
    let aside = run_blocking(move || {
        placement::set_aside(
            fs(),
            Entry::new(&*parent, &step_plan.target_name),
            REPLACED_PREFIX,
            step_plan.target.identity,
        )
        .map_err(|error| set_aside_failure(error, &step_name))
    })
    .await;
    let aside = match aside {
        Ok(aside) => aside,
        Err(error) => {
            let cleanup = discard_incomplete_staged(staged).await;
            return Err(copy_failure_after_cleanup(error, cleanup));
        }
    };
    let parent = staged.parent.clone();
    // Publication finishes regardless of cancellation once the original is aside.
    if let Err(error) = publish_staged_without_replace(
        staged,
        target_path.to_path_buf(),
        durable,
        gio::Cancellable::new(),
    )
    .await
    {
        let detail = crate::services::gio_error_detail(&error);
        let kept = aside.name.clone();
        let step_plan = plan.clone();
        let restored = run_blocking(move || {
            placement::put_back(fs(), parent.as_fd(), &aside, &step_plan.target_name)
                .map_err(placement_error)
        })
        .await;
        return Err(io_error(match restored {
            Ok(()) => could_not_replace(&name, &detail),
            Err(_) => original_kept_aside(&name, &kept, &detail),
        }));
    }
    Ok(StagedReplacement {
        original: ReplacedOriginal::Deleted,
        displaced: Some(DisplacedOriginal {
            parent: OffMain::new(parent),
            name: aside.name,
            identity: aside.identity,
        }),
    })
}

/// Replaces `target` with `source` without a window in which neither
/// exists. The original goes to Trash where possible, otherwise it is
/// exchanged or set aside, and owed with a moved source until the
/// replacement is on disk.
async fn replace_owing(
    source: gio::File,
    target: gio::File,
    move_source: bool,
    cancellable: &gio::Cancellable,
    affected_locations: Option<&mut HashSet<Location>>,
    copying: Copying,
    on_replaced: &dyn Fn(ReplacedOriginal),
) -> Result<Owed, glib::Error> {
    if let Some(locations) = affected_locations {
        locations.extend([&source, &target].into_iter().filter_map(location_for_file));
    }
    let source_path = source.path().ok_or_else(|| {
        glib::Error::new(
            gio::IOErrorEnum::NotSupported,
            &crate::i18n::tr("Safe replacement is unavailable for this source"),
        )
    })?;
    let target_path = target.path().ok_or_else(|| {
        glib::Error::new(
            gio::IOErrorEnum::NotSupported,
            &crate::i18n::tr("Safe replacement is unavailable at this destination"),
        )
    })?;
    cancellable.set_error_if_cancelled()?;
    let (planned_source, planned_target) = (source_path.clone(), target_path.clone());
    let Some(plan) =
        run_blocking(move || plan_replace(&planned_source, &planned_target, move_source)).await?
    else {
        return Ok(Owed::default());
    };
    let plan = Arc::new(plan);
    let removal = match plan.strategy {
        ReplaceStrategy::Staged(removal) => removal,
        ReplaceStrategy::PlaceSource => {
            cancellable.set_error_if_cancelled()?;
            let (step_plan, path, step_cancellable) =
                (plan.clone(), target_path.clone(), cancellable.clone());
            match run_blocking(move || place_source(&step_plan, &path, &step_cancellable)).await? {
                SourcePlaced::Trashed => {
                    on_replaced(ReplacedOriginal::Trashed(plan.trashed_original()));
                    return Ok(Owed::default());
                }
                // The source took its place by a rename, so it was not copied.
                SourcePlaced::SetAside(aside) => {
                    on_replaced(ReplacedOriginal::Deleted);
                    let original = DisplacedOriginal {
                        parent: OffMain::new(plan.target_parent.clone()),
                        name: aside.name,
                        identity: aside.identity,
                    };
                    remove_displaced_original(original, &plan.target_display()).await?;
                    return Ok(Owed::default());
                }
                SourcePlaced::CopyAfterTrash => Removal::Trashed,
                SourcePlaced::PutBack => Removal::Without(Displacement::SetAside),
            }
        }
    };
    let mut owed = Owed::default();
    // Unless the original is in Trash already, it may be exchanged or set
    // aside, and then deleted.
    if move_source || !matches!(removal, Removal::Trashed) {
        owed.barrier = Some(copy_engine::barrier(split_local_path(&target_path)?.0).await?);
    }
    let progress = copying.progress.clone();
    let durability = Durability {
        // A Trash attempt that falls back syncs the stage then.
        durable: move_source || matches!(removal, Removal::Without(_)),
        each_file: owed
            .barrier
            .as_ref()
            .is_some_and(copy_engine::Barrier::syncs_each_file),
    };
    let (replaced, (sources, report)) = replace_through_stage(
        &plan,
        removal,
        (&source_path, move_source),
        &target_path,
        cancellable,
        (copying, durability),
    )
    .await?;
    on_replaced(replaced.original);
    if let Some(progress) = &progress {
        progress.report(report);
    }
    if let Some(original) = replaced.displaced {
        owed.owe(Deletion::Displaced(original, plan.target_display()));
    }
    if let Some(mut sources) = sources {
        let (parent, name) = (plan.target_parent.clone(), plan.target_name.clone());
        sources.copied_to(copy_engine::published(parent, name).await?);
        owed.owe(Deletion::Sources(source, sources));
    }
    Ok(owed)
}

/// Replaces `target` with `source` as [`replace_owing`] does, deleting
/// what that owes at once.
async fn replace_local_with(
    source: gio::File,
    target: gio::File,
    move_source: bool,
    cancellable: gio::Cancellable,
    affected_locations: Option<&mut HashSet<Location>>,
    copying: Copying,
    on_replaced: &dyn Fn(ReplacedOriginal),
) -> Result<(), glib::Error> {
    let progress = copying.progress.clone();
    replace_owing(
        source,
        target,
        move_source,
        &cancellable,
        affected_locations,
        copying,
        on_replaced,
    )
    .await?
    .settle(progress.as_deref(), &cancellable)
    .await
}

async fn publish_staged_replacement(
    staged: StagedSibling,
    target_path: PathBuf,
    durable: bool,
) -> Result<(), glib::Error> {
    publish_staged_without_replace(staged, target_path, durable, gio::Cancellable::new())
        .await
        .map_err(|error| original_in_trash(&crate::services::gio_error_detail(&error)))
}

/// What a Replace took from a moved source, and what to report once the
/// replacement is published.
type StagedCopy = (Option<copy_engine::Manifest>, TransferReport);

/// How a copy reports its progress and names what it makes.
#[derive(Clone, Default)]
struct Copying {
    progress: Option<Rc<TransferProgressTracker>>,
    fat_family: bool,
}

#[cfg(test)]
async fn replace_local(
    source: gio::File,
    target: gio::File,
    move_source: bool,
    cancellable: gio::Cancellable,
    affected_locations: Option<&mut HashSet<Location>>,
) -> Result<(), glib::Error> {
    replace_local_with(
        source,
        target,
        move_source,
        cancellable,
        affected_locations,
        Copying::default(),
        &|_| {},
    )
    .await
}

/// What a merge will write: `created` are the topmost destination paths with
/// no existing counterpart, `overwritten` are leaf collisions whose originals
/// were staged in Trash so undo can restore them.
#[derive(Default)]
struct MergePlan {
    created: Vec<Location>,
    overwritten: Vec<Location>,
}

/// Backs up an overwritten original before the incoming copy lands. Production
/// stages through Trash; tests substitute a rename so fixtures on filesystems
/// without Trash support still exercise the flow. `NotSupported` sets a local
/// original aside instead.
type StageOverwrite = Rc<
    dyn Fn(Location, gio::Cancellable) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>>,
>;

fn trash_stage_overwrite(
    location: Location,
    cancellable: gio::Cancellable,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    Box::pin(async move {
        trash::trash_entry(&gio_file_for_location(&location), &cancellable)
            .await
            .map(drop)
    })
}

/// The injectable steps of a merge, bundled to keep the call sites small.
struct MergeHooks<'a> {
    fat_family: bool,
    copy_into_target: CopyStep,
    stage_overwrite: StageOverwrite,
    on_merged: &'a dyn Fn(MergePlan),
}

fn classify_merge<'a>(
    source: &'a gio::File,
    target: &'a gio::File,
    plan: &'a mut MergePlan,
    cancellable: &'a gio::Cancellable,
    fat_family: bool,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'a>> {
    Box::pin(async move {
        let enumerator = await_cancellable(source, cancellable, |source, cancellable, result| {
            source.enumerate_children_async(
                "standard::name,standard::type",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(cancellable),
                move |output| result.resolve(output),
            );
        })
        .await?;
        let mut used_names = HashSet::new();
        loop {
            let children = copy_children(&enumerator, cancellable, fat_family).await?;
            if children.is_empty() {
                return Ok(());
            }
            for child in children {
                if cancellable.is_cancelled() {
                    return Err(cancelled_local_operation());
                }
                let child_name = child.name();
                let target_name =
                    fat_family_child_name(child_name.as_os_str(), fat_family, &mut used_names);
                let child_source = source.child(&child_name);
                let child_target = target.child(&target_name);
                let target_type = match await_cancellable(
                    &child_target,
                    cancellable,
                    |target, cancellable, result| {
                        target.query_info_async(
                            "standard::type",
                            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                            glib::Priority::DEFAULT,
                            Some(cancellable),
                            move |output| result.resolve(output),
                        );
                    },
                )
                .await
                {
                    Ok(info) => Some(info.file_type()),
                    Err(error) if error.matches(gio::IOErrorEnum::NotFound) => None,
                    Err(error) => return Err(error),
                };
                let source_is_directory = child.file_type() == gio::FileType::Directory;
                match target_type {
                    None => {
                        if let Some(location) = location_for_file(&child_target) {
                            plan.created.push(location);
                        }
                    }
                    Some(gio::FileType::Directory) if source_is_directory => {
                        classify_merge(&child_source, &child_target, plan, cancellable, fat_family)
                            .await?;
                    }
                    Some(gio::FileType::Directory) => {
                        return Err(glib::Error::new(
                            gio::IOErrorEnum::NotSupported,
                            &crate::i18n::tr(
                                "A file and a folder cannot safely replace one another",
                            ),
                        ));
                    }
                    Some(_) if source_is_directory => {
                        return Err(glib::Error::new(
                            gio::IOErrorEnum::NotSupported,
                            &crate::i18n::tr(
                                "A file and a folder cannot safely replace one another",
                            ),
                        ));
                    }
                    Some(_) => {
                        if let Some(location) = location_for_file(&child_target) {
                            plan.overwritten.push(location);
                        }
                    }
                }
            }
            if fat_family {
                return Ok(());
            }
        }
    })
}

/// Combines the source folder into the existing destination folder: every
/// child lands inside `target`, overwriting same-named entries, while
/// destination-only contents stay. Unlike [`replace_owing`] this writes
/// into the live destination, so a failed or cancelled merge leaves a
/// partial result behind and never cleans what was already there. Returns
/// what it owes once the merge is on disk: the originals it set aside, and
/// a moved source.
async fn merge_owing(
    source: gio::File,
    target: gio::File,
    move_source: bool,
    cancellable: &gio::Cancellable,
    affected_locations: Option<&mut HashSet<Location>>,
    hooks: MergeHooks<'_>,
) -> Result<Owed, glib::Error> {
    if let Some(locations) = affected_locations {
        locations.extend([&source, &target].into_iter().filter_map(location_for_file));
    }
    for file in [&source, &target] {
        let file_type = await_cancellable(file, cancellable, |file, cancellable, result| {
            file.query_info_async(
                "standard::type",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(cancellable),
                move |output| result.resolve(output),
            );
        })
        .await?
        .file_type();
        if file_type != gio::FileType::Directory {
            return Err(glib::Error::new(
                gio::IOErrorEnum::NotSupported,
                &crate::i18n::tr("Only folders can be merged"),
            ));
        }
    }
    let mut plan = MergePlan::default();
    classify_merge(&source, &target, &mut plan, cancellable, hooks.fat_family).await?;
    let target_path = target.path();
    // Stage every overwritten original in Trash before the copy so undo can
    // restore it; on failure, report what was staged so it stays recoverable.
    // Without Trash, originals go aside and are deleted after the merge, so
    // undo keeps their incoming copies.
    let mut trashed = Vec::new();
    let mut aside = None;
    for location in &plan.overwritten {
        let staged = if cancellable.is_cancelled() {
            Err(cancelled_local_operation())
        } else {
            match (hooks.stage_overwrite)(location.clone(), cancellable.clone()).await {
                Ok(()) => {
                    trashed.push(location.clone());
                    Ok(())
                }
                Err(error) => match (target_path.as_deref(), location.native_path()) {
                    (Some(root), Some(path)) if is_trash_unsupported_failure(false, &error) => {
                        merge_aside::set_aside(&mut aside, root, path, location.display_name())
                            .await
                    }
                    _ if was_cancelled(&error) => Err(error),
                    _ => Err(glib::Error::new(
                        gio::IOErrorEnum::Failed,
                        &rust_i18n::t!(
                            "Could not move %{name} to Trash before merging: %{error}",
                            name = location.display_name(),
                            error = crate::services::gio_error_detail(&error)
                        ),
                    )),
                },
            }
        };
        if let Err(error) = staged {
            (hooks.on_merged)(MergePlan {
                created: Vec::new(),
                overwritten: trashed,
            });
            return Err(merge_aside::abandon(aside, error).await);
        }
    }
    // What GIO writes here, for a barrier that does not reach it.
    let written: Vec<PathBuf> = plan
        .created
        .iter()
        .chain(&plan.overwritten)
        .filter_map(|location| location.native_path().map(Path::to_path_buf))
        .collect();
    (hooks.on_merged)(MergePlan {
        created: plan.created,
        overwritten: trashed,
    });
    let mut owed = Owed::default();
    if let Some(path) = native_path(&target).filter(|_| move_source || aside.is_some()) {
        match copy_engine::barrier(path).await {
            Ok(barrier) => owed.barrier = Some(barrier),
            Err(error) => return Err(merge_aside::abandon(aside, error).await),
        }
    }
    let copying = MergeCopy {
        moving: move_source,
        durable: owed.barrier.is_some(),
    };
    let copied =
        (hooks.copy_into_target)(source.clone(), target, copying, cancellable.clone()).await;
    let copied = match copied {
        Ok(copied) => copied,
        Err(error) => return Err(merge_aside::abandon(aside, error).await),
    };
    if native_path(&source).is_none()
        && owed
            .barrier
            .as_ref()
            .is_some_and(copy_engine::Barrier::syncs_each_file)
        && let Err(error) = copy_engine::sync_files(written, cancellable).await
    {
        return Err(merge_aside::abandon(aside, error).await);
    }
    owed.nested = copied.nested;
    if let Some(aside) = merge_aside::finish(aside).await? {
        owed.owe(Deletion::Aside(aside));
    }
    if move_source {
        owed.owe(match copied.sources {
            Some(sources) => Deletion::Sources(source, sources),
            None => Deletion::Source(source),
        });
    }
    Ok(owed)
}

/// Merges `source` into `target` as [`merge_owing`] does, deleting what
/// that owes at once.
#[cfg(test)]
async fn merge_local_with(
    source: gio::File,
    target: gio::File,
    move_source: bool,
    cancellable: gio::Cancellable,
    affected_locations: Option<&mut HashSet<Location>>,
    hooks: MergeHooks<'_>,
) -> Result<(), glib::Error> {
    merge_owing(
        source,
        target,
        move_source,
        &cancellable,
        affected_locations,
        hooks,
    )
    .await?
    .settle(None, &cancellable)
    .await
}

fn progress_merge_hooks(
    progress: Option<Rc<TransferProgressTracker>>,
    fat_family: bool,
    on_merged: &dyn Fn(MergePlan),
) -> MergeHooks<'_> {
    MergeHooks {
        fat_family,
        copy_into_target: merge_copy(progress, fat_family),
        stage_overwrite: Rc::new(trash_stage_overwrite),
        on_merged,
    }
}

/// Copies a folder's contents into the folder of the same name, never
/// replacing an entry there.
fn merge_copy(progress: Option<Rc<TransferProgressTracker>>, fat_family: bool) -> CopyStep {
    Rc::new(
        move |source, target, MergeCopy { moving, durable }, cancellable| {
            let progress = progress.clone();
            Box::pin(async move {
                match (native_path(&source), native_path(&target)) {
                    (Some(source_path), Some(target_path)) => {
                        let options = copy_engine::Options {
                            fat_family,
                            manifest: moving,
                            durable,
                            ..copy_engine::Options::default()
                        };
                        let copied = copy_engine::copy_local(
                            source_path,
                            copy_engine::Destination::Merge(target_path),
                            options,
                            progress,
                            &cancellable,
                        )
                        .await?;
                        Ok(MergeCopied {
                            sources: copied.manifest,
                            nested: copied.nested,
                        })
                    }
                    (source_path, _) => {
                        // Recorded before GIO copies it, so a change during the copy shows.
                        let sources = match source_path.filter(|_| moving) {
                            Some(source_path) => {
                                Some(copy_engine::snapshot(source_path, &cancellable).await?)
                            }
                            None => None,
                        };
                        copy_recursively_with_progress(
                            source,
                            target,
                            CopyInto::Existing,
                            cancellable,
                            None,
                            progress,
                            fat_family,
                        )
                        .await?;
                        Ok(MergeCopied {
                            sources,
                            ..MergeCopied::default()
                        })
                    }
                }
            })
        },
    )
}

#[cfg(test)]
async fn merge_local(
    source: gio::File,
    target: gio::File,
    move_source: bool,
    cancellable: gio::Cancellable,
    on_merged: &dyn Fn(MergePlan),
) -> Result<(), glib::Error> {
    merge_local_with(
        source,
        target,
        move_source,
        cancellable,
        None,
        progress_merge_hooks(None, false, on_merged),
    )
    .await
}

fn permanently_delete(
    file: gio::File,
    directory: bool,
    cancellable: gio::Cancellable,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    Box::pin(async move {
        if directory {
            let enumerator = await_cancellable(&file, &cancellable, |file, cancellable, result| {
                file.enumerate_children_async(
                    "standard::name,standard::type",
                    gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                    glib::Priority::DEFAULT,
                    Some(cancellable),
                    move |output| result.resolve(output),
                );
            })
            .await?;
            loop {
                let children = await_cancellable(
                    &enumerator,
                    &cancellable,
                    |enumerator, cancellable, result| {
                        enumerator.next_files_async(
                            64,
                            glib::Priority::DEFAULT,
                            Some(cancellable),
                            move |output| result.resolve(output),
                        );
                    },
                )
                .await?;
                if children.is_empty() {
                    break;
                }
                for child in children {
                    permanently_delete(
                        file.child(child.name()),
                        child.file_type() == gio::FileType::Directory,
                        cancellable.clone(),
                    )
                    .await?;
                }
            }
        }
        await_cancellable(&file, &cancellable, |file, cancellable, result| {
            file.delete_async(glib::Priority::DEFAULT, Some(cancellable), move |output| {
                result.resolve(output)
            });
        })
        .await
    })
}

/// The outcome of resolving one delete target relative to its parent
/// directory's file descriptor.
enum LocalDeleteStep {
    /// A non-directory entry (file, symlink, or other special file) that has
    /// already been unlinked.
    Removed,
    /// A directory that was opened (not yet removed) along with its
    /// immediate children, still to be deleted before the directory itself.
    Directory {
        handle: OwnedFd,
        children: Vec<OsString>,
    },
}

/// Many filesystems hand a freed inode number to the next item created, so
/// the type and, where reported, the birth time tell the two apart.
#[derive(Clone, Copy, Debug)]
struct LocalFileIdentity {
    device: u64,
    inode: u64,
    kind: rustix::fs::FileType,
    birth: Option<(i64, u32)>,
}

impl PartialEq for LocalFileIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode && self.may_be(other)
    }
}

impl LocalFileIdentity {
    /// Whether `other` may be this item, by what tells them apart besides
    /// inode numbers: the type, and the birth time where both have one.
    fn may_be(&self, other: &Self) -> bool {
        self.kind == other.kind
            && match (self.birth, other.birth) {
                (Some(left), Some(right)) => left == right,
                _ => true,
            }
    }

    fn from_stat(stat: &rustix::fs::Stat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
            kind: rustix::fs::FileType::from_raw_mode(stat.st_mode),
            birth: None,
        }
    }
}

impl From<LocalFileIdentity> for TrashedOriginal {
    fn from(identity: LocalFileIdentity) -> Self {
        Self {
            device: identity.device,
            inode: identity.inode,
        }
    }
}

fn ensure_expected_local_identity(
    name: &OsStr,
    current: LocalFileIdentity,
    expected: Option<LocalFileIdentity>,
) -> Result<(), String> {
    if expected.is_some_and(|expected| expected != current) {
        return Err(rust_i18n::t!(
            "%{name} changed while the operation was in progress",
            name = name.to_string_lossy()
        )
        .into_owned());
    }
    Ok(())
}

/// Fails closed if an opened directory is no longer at its original name.
fn ensure_local_delete_target_unchanged<ParentFd: AsFd, TargetFd: AsFd>(
    parent: &ParentFd,
    name: &OsStr,
    target: &TargetFd,
) -> Result<(), String> {
    let named = fs()
        .stat(Entry::new(parent, name), false)
        .map_err(|error| {
            rust_i18n::t!(
                "%{name} changed while it was being deleted: %{error}",
                name = name.to_string_lossy(),
                error = os_error_detail(error)
            )
            .into_owned()
        })?;
    let opened = fs().stat_fd(target.as_fd()).map_err(|error| {
        rust_i18n::t!(
            "Could not recheck %{name}: %{error}",
            name = name.to_string_lossy(),
            error = os_error_detail(error)
        )
        .into_owned()
    })?;
    if named.identity != opened.identity {
        return Err(rust_i18n::t!(
            "%{name} changed while it was being deleted",
            name = name.to_string_lossy()
        )
        .into_owned());
    }
    Ok(())
}

/// Inspects and, for non-directories, immediately deletes the entry named
/// `name` inside `parent`. The type is re-read from disk here rather than
/// trusted from any earlier listing.
fn open_local_delete_target<Fd: AsFd>(
    parent: &Fd,
    name: &OsStr,
    expected: Option<LocalFileIdentity>,
) -> Result<LocalDeleteStep, String> {
    let stat = fs()
        .stat(Entry::new(parent, name), false)
        .map_err(|error| {
            rust_i18n::t!(
                "Could not inspect %{name}: %{error}",
                name = name.to_string_lossy(),
                error = os_error_detail(error)
            )
            .into_owned()
        })?;
    ensure_expected_local_identity(name, stat.identity, expected)?;
    if !stat.is_directory() {
        rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty()).map_err(|error| {
            rust_i18n::t!(
                "Could not delete %{name}: %{error}",
                name = name.to_string_lossy(),
                error = os_error_detail(error)
            )
            .into_owned()
        })?;
        return Ok(LocalDeleteStep::Removed);
    }
    // RESOLVE_NO_SYMLINKS (stronger than O_NOFOLLOW) plus RESOLVE_BENEATH and
    // RESOLVE_NO_MAGICLINKS: if `name` changed to a symlink (or a magic link)
    // in the moment since the statat above, this fails closed instead of
    // opening whatever it now points to.
    let handle = retry_local_open(|| {
        rustix::fs::openat2(
            parent,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::BENEATH
                | rustix::fs::ResolveFlags::NO_SYMLINKS
                | rustix::fs::ResolveFlags::NO_MAGICLINKS,
        )
    })
    .map_err(|error| {
        rust_i18n::t!(
            "%{name} changed while it was being deleted: %{error}",
            name = name.to_string_lossy(),
            error = os_error_detail(error)
        )
        .into_owned()
    })?;
    let opened = fs().stat_fd(handle.as_fd()).map_err(|error| {
        rust_i18n::t!(
            "Could not recheck %{name}: %{error}",
            name = name.to_string_lossy(),
            error = os_error_detail(error)
        )
        .into_owned()
    })?;
    ensure_expected_local_identity(name, opened.identity, expected)?;
    let mut children = Vec::new();
    for entry in rustix::fs::Dir::read_from(&handle).map_err(os_error_text)? {
        let entry = entry.map_err(os_error_text)?;
        let entry_name = entry.file_name();
        if entry_name == c"." || entry_name == c".." {
            continue;
        }
        children.push(OsString::from_vec(entry_name.to_bytes().to_vec()));
    }
    Ok(LocalDeleteStep::Directory { handle, children })
}

fn retry_local_open(
    mut open: impl FnMut() -> rustix::io::Result<OwnedFd>,
) -> rustix::io::Result<OwnedFd> {
    for _ in 0..15 {
        match open() {
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
            result => return result,
        }
    }
    open()
}

fn cancelled_local_delete() -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Cancelled, "Delete cancelled")
}

async fn run_local_delete_step<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, glib::Error> {
    gio::spawn_blocking(work)
        .await
        .map_err(|_| io_error("Delete task panicked"))?
        .map_err(io_error)
}

#[derive(Clone)]
struct PinnedEntry {
    parent: Arc<OwnedFd>,
    name: OsString,
    identity: Option<LocalFileIdentity>,
}

struct LocalDeleteGuard {
    parent: Arc<OwnedFd>,
    name: OsString,
    handle: Arc<OwnedFd>,
}

type LocalDeleteQueue = WorkQueue<LocalDeleteJob>;

enum LocalDeleteJob {
    Entry {
        parent: Arc<OwnedFd>,
        name: OsString,
        expected: Option<LocalFileIdentity>,
        guard: Option<Arc<LocalDeleteGuard>>,
        completion: Option<Arc<Group<LocalDeleteJob>>>,
    },
    RemoveDirectory {
        parent: Arc<OwnedFd>,
        name: OsString,
        handle: Arc<OwnedFd>,
        completion: Option<Arc<Group<LocalDeleteJob>>>,
    },
}

impl Job for LocalDeleteJob {
    type Error = String;

    fn run(self, queue: &LocalDeleteQueue) {
        process_local_delete_job(queue, self);
    }

    fn cancelled() -> String {
        "Delete cancelled".to_owned()
    }

    fn panicked() -> String {
        "Delete worker panicked".to_owned()
    }

    fn not_started(error: io::Error) -> String {
        format!("Could not start delete worker: {error}")
    }
}

fn process_local_delete_job(queue: &LocalDeleteQueue, job: LocalDeleteJob) {
    match job {
        LocalDeleteJob::Entry {
            parent,
            name,
            expected,
            guard,
            completion,
        } => {
            if queue.is_stopped() {
                return;
            }
            if let Some(guard) = guard
                && let Err(error) = ensure_local_delete_target_unchanged(
                    guard.parent.as_ref(),
                    &guard.name,
                    guard.handle.as_ref(),
                )
            {
                queue.fail(error);
                return;
            }
            let step = match open_local_delete_target(parent.as_ref(), &name, expected) {
                Ok(step) => step,
                Err(error) => {
                    queue.fail(error);
                    return;
                }
            };
            match step {
                LocalDeleteStep::Removed => queue.complete(completion),
                LocalDeleteStep::Directory { handle, children } => {
                    let handle = Arc::new(handle);
                    if children.is_empty() {
                        remove_local_delete_directory(queue, parent, name, handle, completion);
                        return;
                    }
                    let guard = Arc::new(LocalDeleteGuard {
                        parent: parent.clone(),
                        name: name.clone(),
                        handle: handle.clone(),
                    });
                    let group = Group::new(
                        children.len(),
                        LocalDeleteJob::RemoveDirectory {
                            parent: parent.clone(),
                            name,
                            handle: handle.clone(),
                            completion,
                        },
                    );
                    for child in children {
                        queue.enqueue(LocalDeleteJob::Entry {
                            parent: handle.clone(),
                            name: child,
                            expected: None,
                            guard: Some(guard.clone()),
                            completion: Some(group.clone()),
                        });
                    }
                }
            }
        }
        LocalDeleteJob::RemoveDirectory {
            parent,
            name,
            handle,
            completion,
        } => remove_local_delete_directory(queue, parent, name, handle, completion),
    }
}

fn remove_local_delete_directory(
    queue: &LocalDeleteQueue,
    parent: Arc<OwnedFd>,
    name: OsString,
    handle: Arc<OwnedFd>,
    completion: Option<Arc<Group<LocalDeleteJob>>>,
) {
    if queue.is_stopped() {
        return;
    }
    if let Err(error) =
        ensure_local_delete_target_unchanged(parent.as_ref(), &name, handle.as_ref())
    {
        queue.fail(error);
        return;
    }
    if let Err(error) = rustix::fs::unlinkat(parent.as_ref(), &name, rustix::fs::AtFlags::REMOVEDIR)
    {
        queue.fail(
            rust_i18n::t!(
                "Could not delete %{name}: %{error}",
                name = name.to_string_lossy(),
                error = os_error_detail(error)
            )
            .into_owned(),
        );
        return;
    }
    queue.complete(completion);
}

fn local_device_is_rotational(fd: impl AsFd) -> Option<bool> {
    fs_ops::assert_off_main_thread();
    let stat = rustix::fs::fstat(fd).ok()?;
    let device = PathBuf::from("/sys/dev/block").join(format!(
        "{}:{}",
        rustix::fs::major(stat.st_dev),
        rustix::fs::minor(stat.st_dev)
    ));
    let device = std::fs::canonicalize(device).ok()?;
    rotational_sysfs_node(&device, &mut HashSet::new())
}

fn rotational_sysfs_node(path: &Path, visited: &mut HashSet<PathBuf>) -> Option<bool> {
    let path = std::fs::canonicalize(path).ok()?;
    if !visited.insert(path.clone()) {
        return None;
    }

    let mut has_slave = false;
    let mut unknown_slave = false;
    if let Ok(slaves) = std::fs::read_dir(path.join("slaves")) {
        for slave in slaves.flatten() {
            has_slave = true;
            match rotational_sysfs_node(&slave.path(), visited) {
                Some(true) => return Some(true),
                Some(false) => {}
                None => unknown_slave = true,
            }
        }
    }
    if has_slave {
        return (!unknown_slave).then_some(false);
    }

    let mut current = Some(path.as_path());
    while let Some(node) = current {
        if let Ok(value) = std::fs::read_to_string(node.join("queue/rotational")) {
            return match value.trim() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            };
        }
        current = node.parent();
    }
    None
}

fn bounded_local_worker_count(available: usize, rotational: bool) -> usize {
    let available = available.max(1);
    if rotational {
        1
    } else {
        available.min(MAX_LOCAL_IO_WORKERS)
    }
}

fn local_delete_worker_count(roots: &[PinnedEntry]) -> usize {
    let available = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let rotational = roots
        .iter()
        .any(|root| local_device_is_rotational(root.parent.as_ref()) == Some(true));
    bounded_local_worker_count(available, rotational)
}

fn parallel_delete_local_blocking_with_workers(
    roots: Vec<PinnedEntry>,
    cancelled: Arc<AtomicBool>,
    worker_count: usize,
) -> Result<(), String> {
    let worker_count = worker_count.max(1);
    let queue = Arc::new(LocalDeleteQueue::new(cancelled.clone()));
    for root in roots {
        queue.enqueue(LocalDeleteJob::Entry {
            parent: root.parent,
            name: root.name,
            expected: root.identity,
            guard: None,
            completion: None,
        });
    }

    run_local_delete_workers(&queue, worker_count, |work| {
        thread::Builder::new().spawn(work)
    });

    queue.result()
}

fn run_local_delete_workers(
    queue: &Arc<LocalDeleteQueue>,
    worker_count: usize,
    spawn: impl FnMut(Box<dyn FnOnce() + Send>) -> std::io::Result<thread::JoinHandle<()>>,
) {
    work_queue::run_workers(queue, worker_count, Some(10), spawn);
}

fn parallel_delete_local(
    roots: Vec<PinnedEntry>,
    cancellable: gio::Cancellable,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    parallel_delete_local_with_worker_count(roots, cancellable, None)
}

fn parallel_delete_local_with_worker_count(
    roots: Vec<PinnedEntry>,
    cancellable: gio::Cancellable,
    worker_count: Option<usize>,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    parallel_delete_local_with(roots, cancellable, move |roots, cancelled| {
        let worker_count = worker_count.unwrap_or_else(|| local_delete_worker_count(&roots));
        parallel_delete_local_blocking_with_workers(roots, cancelled, worker_count)
    })
}

fn parallel_delete_local_with(
    roots: Vec<PinnedEntry>,
    cancellable: gio::Cancellable,
    delete: impl FnOnce(Vec<PinnedEntry>, Arc<AtomicBool>) -> Result<(), String> + Send + 'static,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    Box::pin(async move {
        if roots.is_empty() {
            return Ok(());
        }
        if cancellable.is_cancelled() {
            return Err(cancelled_local_delete());
        }
        let result = run_cancellable(&cancellable, move |cancelled| delete(roots, cancelled)).await;
        match result.map_err(|_| io_error("Delete task panicked"))? {
            Ok(()) => Ok(()),
            Err(error) if error == "Delete cancelled" => Err(cancelled_local_delete()),
            Err(error) => Err(io_error(error)),
        }
    })
}

/// Recursively and permanently deletes the entry named `name` inside
/// `parent`, walking descriptor-relative to each already-open directory
/// rather than re-resolving paths, so a component swapped out from under an
/// in-progress delete cannot redirect it outside the tree it started in.
fn permanently_delete_local(
    parent: OwnedFd,
    name: OsString,
    expected: Option<LocalFileIdentity>,
    cancellable: gio::Cancellable,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    parallel_delete_local(
        vec![PinnedEntry {
            parent: Arc::new(parent),
            name,
            identity: expected,
        }],
        cancellable,
    )
}

/// Resolves ordinary parent aliases in one kernel lookup and pins the directory.
/// This is the operation's starting point, not a recursive traversal: selected
/// entries and their children retain their separate no-follow policy. Rooting at
/// `/` permits absolute symlink targets without allowing procfs magic links.
fn open_local_parent_directory(parent_path: &Path) -> Result<OwnedFd, String> {
    if !parent_path.is_absolute() {
        return Err(crate::i18n::tr(
            "A local operation target must use an absolute path",
        ));
    }
    let root = rustix::fs::open(
        c"/",
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| {
        rust_i18n::t!(
            "Could not open the filesystem root: %{error}",
            error = os_error_detail(error)
        )
        .into_owned()
    })?;
    let relative = parent_path
        .strip_prefix(Path::new("/"))
        .map_err(|_| crate::i18n::tr("A local operation target must use an absolute path"))?;
    if relative.as_os_str().is_empty() {
        return Ok(root);
    }
    retry_local_open(|| {
        rustix::fs::openat2(
            &root,
            relative,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::IN_ROOT | rustix::fs::ResolveFlags::NO_MAGICLINKS,
        )
    })
    .map_err(|error| {
        rust_i18n::t!(
            "Could not safely open %{path}: %{error}",
            path = parent_path.display(),
            error = os_error_detail(error)
        )
        .into_owned()
    })
}

fn open_local_parent_beneath(parent_path: &Path, allowed_root: &Path) -> Result<OwnedFd, String> {
    if !parent_path.is_absolute() || !allowed_root.is_absolute() {
        return Err(crate::i18n::tr(
            "A restore destination must use an absolute path",
        ));
    }
    let root = rustix::fs::open(
        allowed_root,
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| {
        rust_i18n::t!(
            "Could not open the restore area: %{error}",
            error = os_error_detail(error)
        )
        .into_owned()
    })?;
    if parent_path == allowed_root {
        return Ok(root);
    }
    let relative = parent_path
        .strip_prefix(allowed_root)
        .map_err(|_| crate::i18n::tr("The restore destination is outside the trash volume"))?;
    retry_local_open(|| {
        rustix::fs::openat2(
            &root,
            relative,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::BENEATH
                | rustix::fs::ResolveFlags::NO_MAGICLINKS
                | rustix::fs::ResolveFlags::NO_XDEV,
        )
    })
    .map_err(|error| {
        rust_i18n::t!(
            "Could not safely open restore destination %{path}: %{error}",
            path = parent_path.display(),
            error = os_error_detail(error)
        )
        .into_owned()
    })
}

/// Entry point for permanently deleting a local path: opens the target's
/// parent directory once, then hands off to the descriptor-relative walk in
/// [`permanently_delete_local`] for everything below it.
fn permanently_delete_local_path_if_unchanged(
    path: PathBuf,
    expected: Option<LocalFileIdentity>,
    cancellable: gio::Cancellable,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    permanently_delete_local_path_if_unchanged_with_worker_count(path, expected, cancellable, None)
}

fn permanently_delete_local_path_if_unchanged_with_worker_count(
    path: PathBuf,
    expected: Option<LocalFileIdentity>,
    cancellable: gio::Cancellable,
    worker_count: Option<usize>,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    Box::pin(async move {
        let Some(parent_path) = path.parent().map(Path::to_path_buf) else {
            return Err(translated_io_error(
                "Cannot permanently delete the filesystem root",
            ));
        };
        let Some(name) = path.file_name().map(OsStr::to_os_string) else {
            return Err(translated_io_error("Invalid delete target"));
        };
        let parent =
            run_local_delete_step(move || open_local_parent_directory(&parent_path)).await?;
        if let Some(worker_count) = worker_count {
            parallel_delete_local_with_worker_count(
                vec![PinnedEntry {
                    parent: Arc::new(parent),
                    name,
                    identity: expected,
                }],
                cancellable,
                Some(worker_count),
            )
            .await
        } else {
            permanently_delete_local(parent, name, expected, cancellable).await
        }
    })
}

async fn local_file_identity(file: &gio::File) -> Result<Option<LocalFileIdentity>, glib::Error> {
    if !file.is_native() {
        return Ok(None);
    }
    let Some(path) = file.path() else {
        return Ok(None);
    };
    run_local_delete_step(move || {
        let parent_path = path
            .parent()
            .ok_or_else(|| crate::i18n::tr("Cannot inspect the filesystem root"))?;
        let name = path
            .file_name()
            .ok_or_else(|| crate::i18n::tr("Invalid local filesystem target"))?;
        let parent = open_local_parent_directory(parent_path)?;
        let stat = fs()
            .stat(Entry::new(&parent, name), false)
            .map_err(|error| {
                rust_i18n::t!(
                    "Could not inspect %{path}: %{error}",
                    path = path.display(),
                    error = os_error_detail(error)
                )
                .into_owned()
            })?;
        Ok(stat.identity)
    })
    .await
    .map(Some)
}

/// Deletes `file` through the descriptor-relative local walk when it names a
/// local path, or through the path-based GIO delete otherwise. Used for
/// every permanent delete this module performs on the caller's behalf --
/// not just the user-requested ones -- so that cleaning up a staged
/// replacement's old target, or a move's now-copied source, gets the same
/// race safety and doesn't act on whatever type that entry was earlier in
/// the operation rather than what it actually is right before deletion.
fn permanently_delete_maybe_local(
    file: gio::File,
    directory: bool,
    cancellable: gio::Cancellable,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    permanently_delete_maybe_local_if_unchanged(file, directory, None, cancellable)
}

fn permanently_delete_maybe_local_with_worker_count(
    file: gio::File,
    directory: bool,
    cancellable: gio::Cancellable,
    worker_count: Option<usize>,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    if file.is_native()
        && let Some(path) = file.path()
    {
        return permanently_delete_local_path_if_unchanged_with_worker_count(
            path,
            None,
            cancellable,
            worker_count,
        );
    }
    permanently_delete(file, directory, cancellable)
}

fn permanently_delete_maybe_local_if_unchanged(
    file: gio::File,
    directory: bool,
    expected: Option<LocalFileIdentity>,
    cancellable: gio::Cancellable,
) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
    if file.is_native()
        && let Some(path) = file.path()
    {
        return permanently_delete_local_path_if_unchanged(path, expected, cancellable);
    }
    permanently_delete(file, directory, cancellable)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailedAction {
    Delete,
    Trash,
    Restore,
}

/// `OperationEvent` carries display text, so the summary is localized here.
fn operation_error_summary(errors: &[String], action: FailedAction) -> String {
    let items = crate::i18n::count("items", errors.len());
    let mut summary = match action {
        FailedAction::Delete => rust_i18n::t!(
            "%{items} could not be deleted. The remaining items were processed.",
            items = items
        ),
        FailedAction::Trash => rust_i18n::t!(
            "%{items} could not be moved to Trash. The remaining items were processed.",
            items = items
        ),
        FailedAction::Restore => rust_i18n::t!(
            "%{items} could not be restored. The remaining items were processed.",
            items = items
        ),
    }
    .into_owned();
    for error in errors.iter().take(8) {
        summary.push_str("\n\n• ");
        summary.push_str(error);
    }
    if errors.len() > 8 {
        summary.push_str("\n\n");
        summary.push_str(&crate::i18n::count("more_items", errors.len() - 8));
    }
    summary
}

fn deletion_error_summary(errors: &[String], permanent: bool) -> String {
    operation_error_summary(
        errors,
        if permanent {
            FailedAction::Delete
        } else {
            FailedAction::Trash
        },
    )
}

/// Backends without Trash support (most remote filesystems, including SMB)
/// fail a move-to-trash with `NOT_SUPPORTED`. Give an actionable message for
/// that specific case instead of the raw GIO error text.
fn deletion_error_message(name: &str, permanent: bool, error: &glib::Error) -> String {
    if !permanent && error.matches(gio::IOErrorEnum::NotSupported) {
        rust_i18n::t!(
            "%{name}: This location doesn't support Trash. Delete permanently instead.",
            name = name
        )
        .into_owned()
    } else {
        item_error(name, crate::services::gio_error_detail(error))
    }
}

fn item_error(name: &str, error: impl std::fmt::Display) -> String {
    rust_i18n::t!("%{name}: %{error}", name = name, error = error).into_owned()
}

impl TrashedOriginal {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    async fn matches_path(self, path: PathBuf) -> bool {
        gio::spawn_blocking(move || {
            std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| self == Self::from_metadata(&metadata))
        })
        .await
        .unwrap_or(false)
    }
}

#[derive(Clone)]
struct RestoreEntry {
    source: Location,
    display_name: String,
    original_target: Option<Location>,
    trash_info: Option<PathBuf>,
    confirmed_destination: Option<PathBuf>,
    physical_path: Option<PathBuf>,
}

async fn trashed_entries_for_originals(
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
fn restored_destination(entry: &RestoreEntry) -> Location {
    entry
        .confirmed_destination
        .as_ref()
        .map(|path| Location::local(path.clone()))
        .or_else(|| entry.original_target.clone())
        .unwrap_or_else(|| entry.source.clone())
}

async fn local_parent_worker_count(parents: impl IntoIterator<Item = PathBuf>) -> usize {
    let parents = parents.into_iter().collect::<HashSet<_>>();
    gio::spawn_blocking(move || {
        let available = thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let rotational = parents.iter().any(|parent| {
            open_local_parent_directory(parent)
                .ok()
                .and_then(local_device_is_rotational)
                == Some(true)
        });
        bounded_local_worker_count(available, rotational)
    })
    .await
    .unwrap_or(1)
}

async fn restore_worker_count(entries: &[RestoreEntry]) -> usize {
    local_parent_worker_count(entries.iter().filter_map(|entry| {
        restored_destination(entry)
            .native_path()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    }))
    .await
}

/// Restores `entry`, and returns what to report about it. Its Trash
/// metadata stays while anything of it does.
async fn restore_trash_entry(
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
fn trashed_merge_original(
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

/// Reverts one overwritten merge path: removes the incoming copy so the
/// staged original can move back out of Trash, and returns what to report
/// about the restore.
async fn undo_merged_overwrite(
    location: &Location,
    staged: &RestoreEntry,
    context: &RestoreContext,
    affected_locations: &mut HashSet<Location>,
    unlinks: &Arc<copy_engine::Unlinks>,
    cancellable: &gio::Cancellable,
) -> Result<TransferReport, glib::Error> {
    let file = gio_file_for_location(location);
    let existing_type = await_cancellable(&file, cancellable, |file, cancellable, result| {
        file.query_info_async(
            "standard::type",
            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            glib::Priority::DEFAULT,
            Some(cancellable),
            move |output| result.resolve(output),
        );
    })
    .await
    .map(|info| info.file_type())
    .ok();
    if let Some(file_type) = existing_type {
        permanently_delete_maybe_local(
            file,
            file_type == gio::FileType::Directory,
            cancellable.clone(),
        )
        .await?;
    }
    restore_trash_entry(staged, context, affected_locations, unlinks, cancellable).await
}

/// Resolves where an overwritten original was staged. Production reads Trash;
/// tests hand back fixture entries pointing at a fake trash root.
type StagedOriginalLookup = Rc<
    dyn Fn(
        Location,
        gio::Cancellable,
    ) -> Pin<Box<dyn Future<Output = Result<RestoreEntry, glib::Error>>>>,
>;

async fn run_merge_undo(
    request_id: OperationRequestId,
    created: Vec<Location>,
    overwritten: Vec<Location>,
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
    let mut failed_locations = Vec::new();
    let mut errors = Vec::new();
    let mut completed = 0usize;
    let remaining = |index: usize| pending[index..].to_vec();
    // Undo applies to copies only, so an incoming item at an overwritten path
    // is a duplicate of the surviving source: delete it permanently to free
    // the path, then move the staged original back out of Trash.
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
        let result = match staged_original(location.clone(), cancellable.clone()).await {
            Ok(staged) => {
                undo_merged_overwrite(
                    location,
                    &staged,
                    &context,
                    &mut affected_locations,
                    &unlinks,
                    &cancellable,
                )
                .await
            }
            Err(error) => Err(error),
        };
        let restored_location = match result {
            Ok(report) => {
                if !report.is_empty() {
                    emit(OperationEvent::ItemsReported { request_id, report });
                }
                completed_locations.push(location.clone());
                restored.push(location.clone());
                Some(location.clone())
            }
            Err(error) if was_cancelled(&error) => {
                failed_locations.push(location.clone());
                emit(cancelled_event(
                    request_id,
                    completed_locations,
                    failed_locations,
                    remaining(index + 1),
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
        let existing_type = await_cancellable(&file, &cancellable, |file, cancellable, result| {
            file.query_info_async(
                "standard::type",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(cancellable),
                move |output| result.resolve(output),
            );
        })
        .await
        .map(|info| info.file_type())
        .ok();
        let mut trashed_identity = None;
        let result = match existing_type {
            None => Ok(()),
            Some(_) => match trash::trash_entry(&file, &cancellable).await {
                Err(error) if error.matches(gio::IOErrorEnum::NotSupported) => {
                    permanently_delete_maybe_local(
                        file,
                        existing_type == Some(gio::FileType::Directory),
                        cancellable.clone(),
                    )
                    .await
                }
                result => result.map(|identity| trashed_identity = identity),
            },
        };
        let deleted_locations = match result {
            Ok(()) => {
                if existing_type.is_some() {
                    super::bookmarks::deletion_completed(std::slice::from_ref(location));
                }
                if let Some(identity) = trashed_identity {
                    emit(OperationEvent::ItemTrashed {
                        request_id,
                        location: location.clone(),
                        identity,
                    });
                }
                completed_locations.push(location.clone());
                vec![location.clone()]
            }
            Err(error) if was_cancelled(&error) => {
                failed_locations.push(location.clone());
                emit(cancelled_event(
                    request_id,
                    completed_locations,
                    failed_locations,
                    remaining(overwritten.len() + index + 1),
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
    if errors.is_empty() {
        emit(OperationEvent::Restored {
            request_id,
            locations: Vec::new(),
            restored,
        });
    } else {
        emit(OperationEvent::CompletedWithErrors {
            request_id,
            deleted_locations: completed_locations,
            retryable_locations: Vec::new(),
            has_non_retryable_failures: true,
            message: deletion_error_summary(&errors, true),
        });
    }
}

fn home_trash_entries(
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

fn home_trash_entries_at(
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

fn cancellation_handle(cancellable: gio::Cancellable) -> LoadHandle {
    LoadHandle::new(move || cancellable.cancel())
}

#[derive(Clone)]
struct DeletionTarget {
    location: Location,
    display_name: String,
    is_directory: bool,
}

impl DeletionTarget {
    fn from_entry(entry: &FileEntry) -> Self {
        Self {
            location: entry.location.clone(),
            display_name: entry.display_name.clone(),
            is_directory: entry.is_directory(),
        }
    }

    async fn probed(location: &Location, cancellable: &gio::Cancellable) -> Self {
        let file = gio_file_for_location(location);
        let is_directory = await_cancellable(&file, cancellable, |file, cancellable, result| {
            file.query_info_async(
                "standard::type",
                gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                glib::Priority::DEFAULT,
                Some(cancellable),
                move |output| result.resolve(output),
            );
        })
        .await
        .is_ok_and(|info| info.file_type() == gio::FileType::Directory);
        Self {
            location: location.clone(),
            display_name: location.display_name(),
            is_directory,
        }
    }
}

async fn deletion_worker_count(targets: &[DeletionTarget]) -> usize {
    local_parent_worker_count(targets.iter().filter_map(|target| {
        target
            .location
            .native_path()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    }))
    .await
}

async fn run_deletion(
    request_id: OperationRequestId,
    targets: Vec<DeletionTarget>,
    permanent: bool,
    emit: Rc<dyn Fn(OperationEvent)>,
    cancellable: gio::Cancellable,
) {
    let mut errors = Vec::new();
    let mut deleted_locations = Vec::new();
    let mut failed_locations = Vec::new();
    let mut retryable_locations = Vec::new();
    let mut affected_locations = HashSet::new();
    for target in &targets {
        if let Some(parent) = target.location.parent() {
            affected_locations.insert(parent);
        }
        if target.is_directory {
            affected_locations.insert(target.location.clone());
        }
    }
    if !permanent {
        affected_locations.insert(Location::uri("trash:///"));
    }
    let total = targets.len();
    let mut last_progress_emit = std::time::Instant::now();
    let batch_size = if total > 32 {
        deletion_worker_count(&targets).await
    } else {
        1
    };
    let local_tree_worker_count = (batch_size > 1).then_some(1);
    let mut completed_count = 0;
    let mut pending_progress_locations = Vec::new();

    for chunk in targets.chunks(batch_size) {
        if cancellable.is_cancelled() {
            emit(cancelled_event(
                request_id,
                deleted_locations,
                failed_locations,
                targets[completed_count..]
                    .iter()
                    .map(|target| target.location.clone())
                    .collect(),
                affected_locations,
            ));
            return;
        }

        let context = glib::MainContext::default();
        let tasks = chunk
            .iter()
            .map(|target| {
                let target = target.clone();
                let operation_target = target.clone();
                let cancellable = cancellable.clone();
                let task = context.spawn_local(async move {
                    let file = gio_file_for_location(&operation_target.location);
                    if !permanent {
                        return trash::trash_entry(&file, &cancellable).await;
                    }
                    if operation_target
                        .location
                        .uri_value()
                        .is_some_and(|uri| uri.starts_with("trash:"))
                    {
                        await_cancellable(&file, &cancellable, |file, cancellable, result| {
                            file.delete_async(
                                glib::Priority::DEFAULT,
                                Some(cancellable),
                                move |output| result.resolve(output),
                            );
                        })
                        .await
                    } else {
                        permanently_delete_maybe_local_with_worker_count(
                            file,
                            operation_target.is_directory,
                            cancellable.clone(),
                            local_tree_worker_count,
                        )
                        .await
                    }
                    .map(|()| None)
                });
                (target, task)
            })
            .collect();

        let mut chunk_cancelled = false;
        for (target, result) in join_local_tasks(tasks).await {
            completed_count += 1;
            match result.unwrap_or_else(|_| Err(io_error("Delete worker failed"))) {
                Err(error) if was_cancelled(&error) => {
                    chunk_cancelled = true;
                    failed_locations.push(target.location.clone());
                }
                Err(error) => {
                    if is_trash_unsupported_failure(permanent, &error) {
                        retryable_locations.push(target.location.clone());
                    }
                    errors.push(deletion_error_message(
                        &target.display_name,
                        permanent,
                        &error,
                    ));
                    failed_locations.push(target.location.clone());
                }
                Ok(identity) => {
                    super::bookmarks::deletion_completed(std::slice::from_ref(&target.location));
                    if let Some(identity) = identity {
                        emit(OperationEvent::ItemTrashed {
                            request_id,
                            location: target.location.clone(),
                            identity,
                        });
                    }
                    deleted_locations.push(target.location.clone());
                    pending_progress_locations.push(target.location.clone());
                }
            }
            let is_last = completed_count == total;
            let now = std::time::Instant::now();
            if total <= 32
                || is_last
                || now.duration_since(last_progress_emit) >= std::time::Duration::from_millis(33)
            {
                last_progress_emit = now;
                emit(OperationEvent::DeleteProgress {
                    request_id,
                    completed: completed_count,
                    total,
                    deleted_locations: std::mem::take(&mut pending_progress_locations),
                });
            }
        }
        if chunk_cancelled {
            emit(cancelled_event(
                request_id,
                deleted_locations,
                failed_locations,
                targets[completed_count..]
                    .iter()
                    .map(|target| target.location.clone())
                    .collect(),
                affected_locations,
            ));
            return;
        }
    }
    if errors.is_empty() {
        emit(OperationEvent::Deleted {
            request_id,
            locations: deleted_locations,
        });
    } else {
        let has_non_retryable_failures = errors.len() > retryable_locations.len();
        emit(OperationEvent::CompletedWithErrors {
            request_id,
            deleted_locations,
            retryable_locations,
            has_non_retryable_failures,
            message: deletion_error_summary(&errors, permanent),
        });
    }
}

fn cancelled_event(
    request_id: crate::services::OperationRequestId,
    completed: Vec<Location>,
    failed: Vec<Location>,
    not_attempted: Vec<Location>,
    affected_locations: HashSet<Location>,
) -> OperationEvent {
    OperationEvent::Cancelled {
        request_id,
        result: CancelledOperation {
            completed,
            failed,
            not_attempted,
            affected_locations,
        },
    }
}

#[derive(Default)]
pub struct LocalOperationProvider;

impl OperationProvider for LocalOperationProvider {
    fn rename(&self, request: RenameRequest, emit: Rc<dyn Fn(OperationEvent)>) -> LoadHandle {
        let cancellable = gio::Cancellable::new();
        let operation_cancellable = cancellable.clone();
        let _task = glib::MainContext::default().spawn_local(async move {
            if let Err(message) = validate_basename(&request.new_name) {
                emit(OperationEvent::Failed {
                    request_id: request.id,
                    message: crate::i18n::tr(message),
                    password_failure: None,
                });
                return;
            }
            let file = request
                .entry
                .location
                .native_path()
                .map(gio::File::for_path)
                .unwrap_or_else(|| {
                    gio::File::for_uri(request.entry.location.uri_value().unwrap_or_default())
                });
            let item = request.entry.location.clone();
            let new_name = request.new_name.clone();
            let affected_locations = item.parent().into_iter().collect();
            if operation_cancellable.is_cancelled() {
                emit(cancelled_event(
                    request.id,
                    Vec::new(),
                    Vec::new(),
                    vec![item],
                    affected_locations,
                ));
                return;
            }
            let result = match request.entry.location.native_path() {
                Some(path) => match glib::filename_from_utf8(&request.new_name) {
                    Ok((name, _)) => {
                        rename_native(path.to_path_buf(), path.with_file_name(name)).await
                    }
                    Err(error) => Err(glib::Error::new(
                        gio::IOErrorEnum::InvalidFilename,
                        &error.to_string(),
                    )),
                },
                None => await_cancellable(
                    &file,
                    &operation_cancellable,
                    move |file, cancellable, result| {
                        file.set_display_name_async(
                            &request.new_name,
                            glib::Priority::DEFAULT,
                            Some(cancellable),
                            move |output| result.resolve(output),
                        );
                    },
                )
                .await
                .map(drop),
            };
            match result {
                Ok(()) => emit(OperationEvent::Renamed {
                    request_id: request.id,
                }),
                Err(error) if was_cancelled(&error) => {
                    emit(cancelled_event(
                        request.id,
                        Vec::new(),
                        vec![item],
                        Vec::new(),
                        affected_locations,
                    ));
                }
                Err(error) => emit(OperationEvent::Failed {
                    request_id: request.id,
                    message: if error.matches(gio::IOErrorEnum::Exists) {
                        rust_i18n::t!("“%{name}” already exists", name = new_name).into_owned()
                    } else {
                        crate::services::gio_error_message(&error)
                    },
                    password_failure: None,
                }),
            }
        });
        cancellation_handle(cancellable)
    }

    fn create_directory(
        &self,
        request: CreateDirectoryRequest,
        emit: Rc<dyn Fn(OperationEvent)>,
    ) -> LoadHandle {
        create_entry::start(
            request.id,
            request.parent,
            request.name,
            request.unique_name,
            true,
            emit,
        )
    }

    fn create_file(
        &self,
        request: CreateFileRequest,
        emit: Rc<dyn Fn(OperationEvent)>,
    ) -> LoadHandle {
        create_entry::start(
            request.id,
            request.parent,
            request.name,
            request.unique_name,
            false,
            emit,
        )
    }

    fn paste(&self, request: PasteRequest, emit: Rc<dyn Fn(OperationEvent)>) -> LoadHandle {
        let cancellable = gio::Cancellable::new();
        let operation_cancellable = cancellable.clone();
        let _task = glib::MainContext::default().spawn_local(async move {
            let destination = gio_file_for_location(&request.destination);
            let mounts = MountTable::load().await;
            let fat_family = target_is_fat_family(&destination, &mounts);
            let max_file_size = destination
                .path()
                .and_then(|path| fat32_file_size_limit(mounts.fs_type_for(&path)));
            let mut used_names = HashSet::new();
            let mut affected_locations = HashSet::from([request.destination.clone()]);
            for parent in request.items.iter().filter_map(|item| item.source.parent()) {
                affected_locations.insert(parent);
            }
            let sources = request
                .items
                .iter()
                .map(|item| gio_file_for_location(&item.source))
                .collect::<Vec<_>>();
            let (item_sizes, total_bytes, item_files, total_files, regular_files) =
                match transfer_sizes(&sources, &operation_cancellable, max_file_size).await {
                    Ok(sizes) => sizes,
                    Err(error) if was_cancelled(&error) => {
                        emit(cancelled_event(
                            request.id,
                            Vec::new(),
                            Vec::new(),
                            request
                                .items
                                .iter()
                                .map(|item| item.source.clone())
                                .collect(),
                            affected_locations,
                        ));
                        return;
                    }
                    Err(error) => {
                        emit(OperationEvent::TransferFailed {
                            request_id: request.id,
                            completed_locations: Vec::new(),
                            message: crate::services::gio_error_message(&error),
                        });
                        return;
                    }
                };
            let progress =
                TransferProgressTracker::new(request.id, total_bytes, total_files, emit.clone());
            progress.emit();
            let mut settling = Settling::paste(&request, emit.clone(), progress.clone());
            let copying = Copying {
                progress: Some(progress.clone()),
                fat_family,
            };
            if let Some((plans, worker_count)) =
                parallel_copy_plans(&request, &sources, &destination, regular_files, fat_family)
                    .await
            {
                for plan in &plans {
                    affected_locations.insert(request.items[plan.index].source.clone());
                    affected_locations.insert(plan.target_location.clone());
                }
                let outcome = execute_parallel_copy_plans(
                    &plans,
                    (&item_sizes, &item_files),
                    (worker_count, request.move_sources),
                    &operation_cancellable,
                    &copying,
                    &mut settling,
                )
                .await;
                settling.finish().await;
                if !outcome.failed.is_empty()
                    || outcome.next < plans.len()
                    || settling.failure().is_some()
                {
                    let mut failed = settling.failed();
                    failed.extend(
                        outcome
                            .failed
                            .iter()
                            .map(|(index, _)| request.items[*index].source.clone()),
                    );
                    let failure_error = outcome
                        .failed
                        .iter()
                        .find(|(_, error)| !was_cancelled(error))
                        .map(|(_, error)| error);
                    let (failure, skip_flush) = failure_error.map_or((None, false), |error| {
                        copy_failure_on_fat32(error, max_file_size.is_some())
                    });
                    stop_transfer(
                        &outcome.written_paths,
                        &emit,
                        request.id,
                        TransferStop {
                            completed: settling.completed(),
                            failed,
                            not_attempted: request.items[outcome.next..]
                                .iter()
                                .map(|item| item.source.clone())
                                .collect(),
                            affected_locations,
                            failure: joined_failures(failure, settling.failure()),
                            skip_flush,
                        },
                    )
                    .await;
                    return;
                }
                if !flush_removable_writes(
                    outcome.written_paths,
                    &operation_cancellable,
                    &emit,
                    request.id,
                    &settling.completed(),
                    affected_locations,
                )
                .await
                {
                    return;
                }
                emit(OperationEvent::Pasted {
                    request_id: request.id,
                    locations: settling.completed(),
                });
                return;
            }
            let mut written_paths = Vec::new();
            for (index, item) in request.items.iter().enumerate() {
                if operation_cancellable.is_cancelled() || settling.failure().is_some() {
                    settling.finish().await;
                    stop_transfer(
                        &written_paths,
                        &emit,
                        request.id,
                        TransferStop {
                            completed: settling.completed(),
                            failed: settling.failed(),
                            not_attempted: request.items[index..]
                                .iter()
                                .map(|item| item.source.clone())
                                .collect(),
                            affected_locations,
                            failure: settling.failure(),
                            skip_flush: false,
                        },
                    )
                    .await;
                    return;
                }
                let source = sources[index].clone();
                let started = progress.reported();
                let Some(target) =
                    default_transfer_target(&source, &destination, fat_family, &mut used_names)
                else {
                    settling.finish().await;
                    let flush_error = flush_written_roots(&written_paths, &emit, request.id)
                        .await
                        .err();
                    let flush_error = joined_failures(settling.failure(), flush_error);
                    emit(OperationEvent::Failed {
                        request_id: request.id,
                        message: match flush_error {
                            Some(error) => rust_i18n::t!(
                                "A clipboard item has no file name. %{error}",
                                error = error
                            )
                            .into_owned(),
                            None => crate::i18n::tr("A clipboard item has no file name"),
                        },
                        password_failure: None,
                    });
                    return;
                };
                let is_duplicate = !request.move_sources && source.equal(&target);
                let needs_unique_target =
                    is_duplicate || item.conflict == TransferConflict::KeepBoth;
                let is_noop = if is_duplicate {
                    item.conflict == TransferConflict::ReplaceExisting
                } else {
                    let (source, destination, target) =
                        (source.clone(), destination.clone(), target.clone());
                    gio::spawn_blocking(move || transfer_is_noop(&source, &destination, &target))
                        .await
                        .unwrap_or(false)
                };
                let last = index + 1 == request.items.len();
                if is_noop {
                    let done = Done {
                        index,
                        moved_to: None,
                    };
                    settling.owe(done, Owed::default(), 0, last).await;
                    progress.finish_item(
                        progress.reported().since(started),
                        item_sizes[index],
                        item_files[index],
                        None,
                    );
                    continue;
                }
                let taken = if needs_unique_target {
                    Taken::Number
                } else {
                    Taken::Fail
                };
                affected_locations.insert(item.source.clone());
                let default_location = location_for_file(&target);
                if let Some(target) = default_location.clone() {
                    affected_locations.insert(target);
                }
                if let Some(path) = target.path() {
                    written_paths.push(path);
                }
                let replaced_permanently = Rc::new(Cell::new(false));
                let result = if is_duplicate {
                    copy_new_recursively_on_filesystem(
                        source,
                        target,
                        taken,
                        operation_cancellable.clone(),
                        Some(progress.clone()),
                        fat_family,
                    )
                    .await
                    .map(|target| (target, Owed::default()))
                } else if item.conflict == TransferConflict::Merge {
                    merge_owing(
                        source,
                        target.clone(),
                        request.move_sources,
                        &operation_cancellable,
                        Some(&mut affected_locations),
                        progress_merge_hooks(Some(progress.clone()), fat_family, &|plan| {
                            emit(OperationEvent::Merged {
                                request_id: request.id,
                                source: item.source.clone(),
                                created: plan.created,
                                overwritten: plan.overwritten,
                            });
                        }),
                    )
                    .await
                    .map(|owed| (target, owed))
                } else if item.conflict == TransferConflict::ReplaceExisting {
                    let replaced_target = default_location;
                    let emit = emit.clone();
                    let replaced_permanently = replaced_permanently.clone();
                    replace_owing(
                        source,
                        target.clone(),
                        request.move_sources,
                        &operation_cancellable,
                        Some(&mut affected_locations),
                        copying.clone(),
                        &move |original| {
                            let identity = match original {
                                ReplacedOriginal::Trashed(identity) => identity,
                                ReplacedOriginal::Deleted => {
                                    replaced_permanently.set(true);
                                    return;
                                }
                            };
                            if let Some(target) = &replaced_target {
                                emit(OperationEvent::ItemTrashed {
                                    request_id: request.id,
                                    location: target.clone(),
                                    identity,
                                });
                            }
                            // Only copies get the restore-the-original undo
                            // entry; a replaced move keeps the move-back
                            // record and leaves the original in Trash.
                            if request.move_sources {
                                return;
                            }
                            if let Some(target) = &replaced_target {
                                emit(OperationEvent::Merged {
                                    request_id: request.id,
                                    source: item.source.clone(),
                                    created: Vec::new(),
                                    overwritten: vec![target.clone()],
                                });
                            }
                        },
                    )
                    .await
                    .map(|owed| (target, owed))
                } else if request.move_sources {
                    move_owing(
                        source,
                        target,
                        taken,
                        operation_cancellable.clone(),
                        copying.clone(),
                    )
                    .await
                    .map(|moved| (moved.target, moved.owed))
                } else {
                    copy_new_recursively_on_filesystem(
                        source,
                        target,
                        taken,
                        operation_cancellable.clone(),
                        Some(progress.clone()),
                        fat_family,
                    )
                    .await
                    .map(|target| (target, Owed::default()))
                };
                let (target, owed) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        settling.finish().await;
                        let (failure, skip_flush) =
                            copy_failure_on_fat32(&error, max_file_size.is_some());
                        let mut failed = settling.failed();
                        failed.push(item.source.clone());
                        stop_transfer(
                            &written_paths,
                            &emit,
                            request.id,
                            TransferStop {
                                completed: settling.completed(),
                                failed,
                                not_attempted: request.items[index + 1..]
                                    .iter()
                                    .map(|item| item.source.clone())
                                    .collect(),
                                affected_locations,
                                failure: joined_failures(failure, settling.failure()),
                                skip_flush,
                            },
                        )
                        .await;
                        return;
                    }
                };
                let target_location = location_for_file(&target);
                if let Some(target) = target_location.clone() {
                    affected_locations.insert(target);
                }
                // A merge reports its written paths through Merged instead:
                // copy undo trashes recorded locations, and the merged folder
                // held pre-existing contents that must not be trashed.
                // Undoing a copy deletes it, which must never remove the only
                // copy left after an original was replaced without Trash.
                let created_location = target_location.clone().filter(|_| {
                    !request.move_sources
                        && item.conflict != TransferConflict::Merge
                        && !replaced_permanently.get()
                });
                let done = Done {
                    index,
                    moved_to: target_location,
                };
                let reported = progress.reported().since(started);
                progress.finish_item(
                    reported,
                    item_sizes[index],
                    item_files[index],
                    created_location,
                );
                settling.owe(done, owed, reported.bytes, last).await;
            }
            settling.finish().await;
            if let Some(failure) = settling.failure() {
                stop_transfer(
                    &written_paths,
                    &emit,
                    request.id,
                    TransferStop {
                        completed: settling.completed(),
                        failed: settling.failed(),
                        not_attempted: Vec::new(),
                        affected_locations,
                        failure: Some(failure),
                        skip_flush: false,
                    },
                )
                .await;
                return;
            }
            let completed = settling.completed();
            if !flush_removable_writes(
                written_paths,
                &operation_cancellable,
                &emit,
                request.id,
                &completed,
                affected_locations,
            )
            .await
            {
                return;
            }
            emit(OperationEvent::Pasted {
                request_id: request.id,
                locations: completed,
            });
        });
        cancellation_handle(cancellable)
    }

    fn undo_move(&self, request: UndoMoveRequest, emit: Rc<dyn Fn(OperationEvent)>) -> LoadHandle {
        let cancellable = gio::Cancellable::new();
        let operation_cancellable = cancellable.clone();
        let _task = glib::MainContext::default().spawn_local(async move {
            let mut affected_locations = HashSet::new();
            for item in &request.items {
                for location in [&item.record.current, &item.record.original] {
                    affected_locations.insert(location.clone());
                    if let Some(parent) = location.parent() {
                        affected_locations.insert(parent);
                    }
                }
            }
            for location in &request.cleanup_locations {
                affected_locations.insert(location.clone());
                if let Some(parent) = location.parent() {
                    affected_locations.insert(parent);
                }
            }
            let sources = request
                .items
                .iter()
                .map(|item| gio_file_for_location(&item.record.current))
                .collect::<Vec<_>>();
            let (item_sizes, total_bytes, item_files, total_files, _) =
                match transfer_sizes(&sources, &operation_cancellable, None).await {
                    Ok(sizes) => sizes,
                    Err(error) if was_cancelled(&error) => {
                        emit(cancelled_event(
                            request.id,
                            Vec::new(),
                            Vec::new(),
                            request
                                .items
                                .iter()
                                .map(|item| item.record.current.clone())
                                .collect(),
                            affected_locations,
                        ));
                        return;
                    }
                    Err(error) => {
                        emit(OperationEvent::TransferFailed {
                            request_id: request.id,
                            completed_locations: Vec::new(),
                            message: crate::services::gio_error_message(&error),
                        });
                        return;
                    }
                };
            let mounts = MountTable::load().await;
            let progress =
                TransferProgressTracker::new(request.id, total_bytes, total_files, emit.clone());
            progress.emit();
            let undone = request
                .items
                .iter()
                .map(|item| owed::Source {
                    location: item.record.current.clone(),
                    merged: false,
                })
                .collect();
            let mut settling =
                Settling::new(request.id, (undone, true), emit.clone(), progress.clone());
            let mut restored_paths = Vec::new();
            for (index, item) in request.items.iter().enumerate() {
                let remaining = || {
                    request.items[index..]
                        .iter()
                        .map(|item| item.record.current.clone())
                        .collect::<Vec<_>>()
                };
                if operation_cancellable.is_cancelled() || settling.failure().is_some() {
                    settling.finish().await;
                    stop_transfer(
                        &restored_paths,
                        &emit,
                        request.id,
                        TransferStop {
                            completed: settling.completed(),
                            failed: settling.failed(),
                            not_attempted: remaining(),
                            affected_locations,
                            failure: settling.failure(),
                            skip_flush: false,
                        },
                    )
                    .await;
                    return;
                }
                let source = sources[index].clone();
                let started = progress.reported();
                let restored_path = item.record.original.native_path().map(Path::to_path_buf);
                if let Some(path) = restored_path {
                    restored_paths.push(path);
                }
                let target = gio_file_for_location(&item.record.original);
                let copying = Copying {
                    progress: Some(progress.clone()),
                    fat_family: target_is_fat_family(&target, &mounts),
                };
                let result = if item.conflict == TransferConflict::ReplaceExisting {
                    replace_owing(
                        source,
                        target,
                        true,
                        &operation_cancellable,
                        Some(&mut affected_locations),
                        copying,
                        &|original| {
                            if let ReplacedOriginal::Trashed(identity) = original {
                                emit(OperationEvent::ItemTrashed {
                                    request_id: request.id,
                                    location: item.record.original.clone(),
                                    identity,
                                });
                            }
                        },
                    )
                    .await
                } else {
                    move_owing(
                        source,
                        target,
                        Taken::Fail,
                        operation_cancellable.clone(),
                        copying,
                    )
                    .await
                    .map(|moved| moved.owed)
                };
                let owed = match result {
                    Ok(owed) => owed,
                    Err(error) => {
                        settling.finish().await;
                        let failure = (!was_cancelled(&error))
                            .then(|| crate::services::gio_error_message(&error));
                        let mut failed = settling.failed();
                        failed.push(item.record.current.clone());
                        stop_transfer(
                            &restored_paths,
                            &emit,
                            request.id,
                            TransferStop {
                                completed: settling.completed(),
                                failed,
                                not_attempted: request.items[index + 1..]
                                    .iter()
                                    .map(|item| item.record.current.clone())
                                    .collect(),
                                affected_locations,
                                failure: joined_failures(failure, settling.failure()),
                                skip_flush: false,
                            },
                        )
                        .await;
                        return;
                    }
                };
                restore_renamed(&item.record, &progress).await;
                let reported = progress.reported().since(started);
                progress.finish_item(reported, item_sizes[index], item_files[index], None);
                let done = Done {
                    index,
                    moved_to: Some(item.record.original.clone()),
                };
                let last = index + 1 == request.items.len();
                settling.owe(done, owed, reported.bytes, last).await;
            }
            settling.finish().await;
            if let Some(failure) = settling.failure() {
                stop_transfer(
                    &restored_paths,
                    &emit,
                    request.id,
                    TransferStop {
                        completed: settling.completed(),
                        failed: settling.failed(),
                        not_attempted: Vec::new(),
                        affected_locations,
                        failure: Some(failure),
                        skip_flush: false,
                    },
                )
                .await;
                return;
            }
            let completed = settling.completed();
            if !flush_removable_writes(
                restored_paths,
                &operation_cancellable,
                &emit,
                request.id,
                &completed,
                affected_locations.clone(),
            )
            .await
            {
                return;
            }
            for location in &request.cleanup_locations {
                let file = gio_file_for_location(location);
                let identity = match trash::trash_entry(&file, &operation_cancellable).await {
                    Ok(identity) => identity,
                    Err(error) => {
                        if was_cancelled(&error) {
                            emit(cancelled_event(
                                request.id,
                                completed,
                                Vec::new(),
                                vec![location.clone()],
                                affected_locations,
                            ));
                        } else {
                            emit(OperationEvent::TransferFailed {
                                request_id: request.id,
                                completed_locations: completed,
                                message: crate::services::gio_error_message(&error),
                            });
                        }
                        return;
                    }
                };
                if let Some(identity) = identity {
                    emit(OperationEvent::ItemTrashed {
                        request_id: request.id,
                        location: location.clone(),
                        identity,
                    });
                }
            }
            emit(OperationEvent::Pasted {
                request_id: request.id,
                locations: completed,
            });
        });
        cancellation_handle(cancellable)
    }

    fn undo_rename(
        &self,
        request: UndoRenameRequest,
        emit: Rc<dyn Fn(OperationEvent)>,
    ) -> LoadHandle {
        let cancellable = gio::Cancellable::new();
        let operation_cancellable = cancellable.clone();
        let _task = glib::MainContext::default().spawn_local(async move {
            let current = request.current.clone();
            let original = request.original.clone();
            let mut affected_locations = HashSet::new();
            for location in [&current, &original] {
                affected_locations.insert(location.clone());
                if let Some(parent) = location.parent() {
                    affected_locations.insert(parent);
                }
            }
            let result = if operation_cancellable.is_cancelled() {
                Err(cancelled_local_operation())
            } else if let (Some(source), Some(target)) =
                (current.native_path(), original.native_path())
            {
                rename_native(source.to_path_buf(), target.to_path_buf()).await
            } else {
                move_file_with_progress(
                    gio_file_for_location(&current),
                    gio_file_for_location(&original),
                    gio::FileCopyFlags::NOFOLLOW_SYMLINKS
                        | gio::FileCopyFlags::ALL_METADATA
                        | gio::FileCopyFlags::NO_FALLBACK_FOR_MOVE,
                    operation_cancellable,
                    None,
                )
                .await
            };
            match result {
                Ok(()) => emit(OperationEvent::Renamed {
                    request_id: request.id,
                }),
                Err(error) if was_cancelled(&error) => emit(cancelled_event(
                    request.id,
                    Vec::new(),
                    Vec::new(),
                    vec![current],
                    affected_locations,
                )),
                Err(error) => emit(OperationEvent::Failed {
                    request_id: request.id,
                    message: crate::services::gio_error_message(&error),
                    password_failure: None,
                }),
            }
        });
        cancellation_handle(cancellable)
    }

    fn delete(&self, request: DeleteRequest, emit: Rc<dyn Fn(OperationEvent)>) -> LoadHandle {
        let cancellable = gio::Cancellable::new();
        let operation_cancellable = cancellable.clone();
        let _task = glib::MainContext::default().spawn_local(async move {
            let targets = request
                .entries
                .iter()
                .map(DeletionTarget::from_entry)
                .collect();
            run_deletion(
                request.id,
                targets,
                request.permanent,
                emit,
                operation_cancellable,
            )
            .await;
        });
        cancellation_handle(cancellable)
    }

    fn undo_copy(&self, request: UndoCopyRequest, emit: Rc<dyn Fn(OperationEvent)>) -> LoadHandle {
        let cancellable = gio::Cancellable::new();
        let operation_cancellable = cancellable.clone();
        let _task = glib::MainContext::default().spawn_local(async move {
            let mut targets = Vec::with_capacity(request.locations.len());
            for location in &request.locations {
                targets.push(DeletionTarget::probed(location, &operation_cancellable).await);
            }
            run_deletion(request.id, targets, false, emit, operation_cancellable).await;
        });
        cancellation_handle(cancellable)
    }

    fn undo_merge(
        &self,
        request: UndoMergeRequest,
        emit: Rc<dyn Fn(OperationEvent)>,
    ) -> LoadHandle {
        let cancellable = gio::Cancellable::new();
        let operation_cancellable = cancellable.clone();
        let _task = glib::MainContext::default().spawn_local(async move {
            run_merge_undo(
                request.id,
                request.created,
                request.overwritten,
                emit,
                operation_cancellable,
                Rc::new(move |location, cancellable| {
                    trashed_merge_original(location, cancellable, request.originals.clone())
                }),
            )
            .await;
        });
        cancellation_handle(cancellable)
    }

    fn restore(&self, request: RestoreRequest, emit: Rc<dyn Fn(OperationEvent)>) -> LoadHandle {
        let cancellable = gio::Cancellable::new();
        let operation_cancellable = cancellable.clone();
        let _task = glib::MainContext::default().spawn_local(async move {
            let entries = match request.source {
                RestoreSource::TrashEntries(items) => items
                    .into_iter()
                    .map(|item| RestoreEntry {
                        source: item.entry.location,
                        display_name: item.entry.display_name,
                        original_target: None,
                        trash_info: None,
                        confirmed_destination: Some(item.destination),
                        physical_path: item.entry.thumbnail_path,
                    })
                    .collect(),
                RestoreSource::OriginalLocations(locations) => match trashed_entries_for_originals(
                    &locations,
                    &operation_cancellable,
                    &HashMap::new(),
                )
                .await
                {
                    Ok(entries) => entries,
                    Err(error) if was_cancelled(&error) => {
                        emit(cancelled_event(
                            request.id,
                            Vec::new(),
                            Vec::new(),
                            locations,
                            HashSet::from([Location::uri("trash:///")]),
                        ));
                        return;
                    }
                    Err(error) => {
                        emit(OperationEvent::Failed {
                            request_id: request.id,
                            message: rust_i18n::t!(
                                "Unable to find items in Trash: %{error}",
                                error = error
                            )
                            .into_owned(),
                            password_failure: None,
                        });
                        return;
                    }
                },
            };
            let total = entries.len();
            let mut errors = Vec::new();
            let mut restored_locations = Vec::new();
            let mut restored = Vec::new();
            let mut failed_locations = Vec::new();
            let mut affected_locations = HashSet::from([Location::uri("trash:///")]);
            let context = RestoreContext::current().await;
            let unlinks = Arc::new(copy_engine::Unlinks::default());
            let worker_count = restore_worker_count(&entries).await;
            let mut completed = 0;
            let mut cancelled = false;
            for chunk in entries.chunks(worker_count) {
                if operation_cancellable.is_cancelled() {
                    emit(cancelled_event(
                        request.id,
                        restored,
                        failed_locations,
                        entries[completed..]
                            .iter()
                            .map(|entry| entry.source.clone())
                            .collect(),
                        affected_locations,
                    ));
                    return;
                }
                let main_context = glib::MainContext::default();
                let tasks = chunk
                    .iter()
                    .cloned()
                    .map(|entry| {
                        let (context, unlinks) = (context.clone(), unlinks.clone());
                        let cancellable = operation_cancellable.clone();
                        main_context.spawn_local(async move {
                            let mut affected = HashSet::new();
                            let result = restore_trash_entry(
                                &entry,
                                &context,
                                &mut affected,
                                &unlinks,
                                &cancellable,
                            )
                            .await;
                            (entry, result, affected)
                        })
                    })
                    .collect::<Vec<_>>();
                for task in tasks {
                    let (entry, result, affected) = match task.await {
                        Ok(outcome) => outcome,
                        Err(_) => {
                            let entry = entries[completed].clone();
                            (
                                entry,
                                Err(io_error("Restore worker failed")),
                                HashSet::new(),
                            )
                        }
                    };
                    affected_locations.extend(affected);
                    completed += 1;
                    let restored_location = match result {
                        Err(error) => {
                            if was_cancelled(&error) {
                                cancelled = true;
                            } else {
                                errors.push(item_error(
                                    &entry.display_name,
                                    crate::services::gio_error_detail(&error),
                                ));
                            }
                            failed_locations.push(entry.source.clone());
                            None
                        }
                        Ok(report) => {
                            if !report.is_empty() {
                                emit(OperationEvent::ItemsReported {
                                    request_id: request.id,
                                    report,
                                });
                            }
                            restored_locations.push(entry.source.clone());
                            let destination = restored_destination(&entry);
                            restored.push(destination.clone());
                            Some(destination)
                        }
                    };
                    emit(OperationEvent::RestoreProgress {
                        request_id: request.id,
                        completed,
                        total,
                        restored_location,
                    });
                }
                if cancelled || operation_cancellable.is_cancelled() && completed < total {
                    emit(cancelled_event(
                        request.id,
                        restored,
                        failed_locations,
                        entries[completed..]
                            .iter()
                            .map(|entry| entry.source.clone())
                            .collect(),
                        affected_locations,
                    ));
                    return;
                }
            }
            if errors.is_empty() {
                emit(OperationEvent::Restored {
                    request_id: request.id,
                    locations: restored_locations,
                    restored,
                });
            } else {
                emit(OperationEvent::RestoreCompletedWithErrors {
                    request_id: request.id,
                    restored_locations,
                    restored,
                    message: operation_error_summary(&errors, FailedAction::Restore),
                });
            }
        });
        cancellation_handle(cancellable)
    }

    fn compress(&self, request: CompressRequest, emit: Rc<dyn Fn(OperationEvent)>) -> LoadHandle {
        archive::compress(request, emit)
    }

    fn extract(&self, request: ExtractRequest, emit: Rc<dyn Fn(OperationEvent)>) -> LoadHandle {
        archive::extract(request, emit)
    }
}
