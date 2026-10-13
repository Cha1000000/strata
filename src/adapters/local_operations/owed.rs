// SPDX-License-Identifier: MIT

//! Deletions a transfer owes once its copies are on disk: the sources of
//! moves that had to copy, Replace originals moved out of the way, and the
//! originals a merge set aside. A paste batches them per destination
//! filesystem, so one barrier serves many items.

#[cfg(test)]
use std::cell::Cell;
use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::{gio, glib};

use super::{
    DisplacedOriginal, TransferProgressTracker,
    copy_engine::{self, Barrier, Manifest},
    delete_moved_sources, device_flush_failure, io_error,
    merge_aside::MergeAside,
    original_not_removed, permanently_delete_maybe_local, removable_roots_for_paths,
    remove_displaced_original,
};
use crate::{
    adapters::volume::MountKey,
    model::Location,
    services::{OperationEvent, PasteRequest, TransferConflict},
};

pub(super) enum Deletion {
    /// What a move's copy holds of its native source.
    Sources(gio::File, Manifest),
    /// A moved source GIO copied.
    Source(gio::File),
    /// A Replace original moved out of the way, with the replaced item's name.
    Displaced(DisplacedOriginal, String),
    Aside(MergeAside),
}

impl Deletion {
    async fn run(
        self,
        progress: Option<&TransferProgressTracker>,
        cancellable: &gio::Cancellable,
    ) -> Result<(), glib::Error> {
        match self {
            Self::Sources(source, manifest) => {
                delete_moved_sources(&source, manifest, progress, cancellable).await
            }
            Self::Source(source) => {
                permanently_delete_maybe_local(source, true, cancellable.clone()).await
            }
            Self::Displaced(original, name) => remove_displaced_original(original, &name).await,
            Self::Aside(aside) => aside.remove().await,
        }
    }

    /// Where what it would have deleted stays, as the barrier before it
    /// failed with `flush`.
    fn kept(&self, flush: &str) -> String {
        match self {
            Self::Sources(..) | Self::Source(_) => flush.to_owned(),
            Self::Displaced(original, name) => {
                original_not_removed(name, &original.name, &io_error(flush))
            }
            Self::Aside(aside) => aside.not_removed(&io_error(flush)).message().to_owned(),
        }
    }
}

/// What a step of a transfer deletes once its copy is on disk.
#[derive(Default)]
pub(super) struct Owed {
    /// Opened before the copy wrote; none where only GIO can reach it.
    pub barrier: Option<Barrier>,
    deletions: Vec<Deletion>,
}

impl Owed {
    pub(super) fn owe(&mut self, deletion: Deletion) {
        self.deletions.push(deletion);
    }

    /// Flushes and deletes at once, as a batch of its own.
    pub(super) async fn settle(
        self,
        progress: Option<&TransferProgressTracker>,
        cancellable: &gio::Cancellable,
    ) -> Result<(), glib::Error> {
        if self.deletions.is_empty() {
            return Ok(());
        }
        let mut batch = Batch::new(None);
        batch.add((), self, 0);
        batch
            .close(progress, cancellable)
            .await
            .pop()
            .map_or(Ok(()), |((), settled)| settled)
    }
}

/// When a batch closes, besides at the end of its paste.
#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub items: usize,
    /// Bytes its entries copied.
    pub bytes: u64,
    /// Time since its first entry, checked as items finish.
    pub age: Duration,
}

const LIMITS: Limits = Limits {
    items: 256,
    bytes: 1 << 30,
    age: Duration::from_secs(5),
};

#[cfg(test)]
thread_local! {
    static TEST_LIMITS: Cell<Option<Limits>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(super) fn set_batch_limits_for_test(limits: Option<Limits>) {
    TEST_LIMITS.set(limits);
}

fn limits() -> Limits {
    #[cfg(test)]
    if let Some(limits) = TEST_LIMITS.get() {
        return limits;
    }
    LIMITS
}

/// Deletions owed on one destination filesystem, behind one barrier.
struct Batch<T> {
    key: Option<MountKey>,
    /// The earliest opened of its entries' barriers, so it reports the
    /// writeback errors of every one of their copies.
    barrier: Option<Barrier>,
    entries: Vec<(T, Vec<Deletion>)>,
    started: Instant,
    copied: u64,
}

impl<T> Batch<T> {
    fn new(key: Option<MountKey>) -> Self {
        Self {
            key,
            barrier: None,
            entries: Vec::new(),
            started: Instant::now(),
            copied: 0,
        }
    }

    fn add(&mut self, tag: T, owed: Owed, copied: u64) {
        self.copied = self.copied.saturating_add(copied);
        if let Some(barrier) = owed.barrier
            && self
                .barrier
                .as_ref()
                .is_none_or(|kept| barrier.precedes(kept))
        {
            self.barrier = Some(barrier);
        }
        self.entries.push((tag, owed.deletions));
    }

    fn due(&self) -> bool {
        let limits = limits();
        self.key.is_none()
            || self.entries.len() >= limits.items
            || self.copied >= limits.bytes
            || self.started.elapsed() >= limits.age
    }

    /// Flushes once, then performs what each entry owes in order. A failed
    /// flush deletes nothing.
    async fn close(
        self,
        progress: Option<&TransferProgressTracker>,
        cancellable: &gio::Cancellable,
    ) -> Vec<(T, Result<(), glib::Error>)> {
        let flushed = match self.barrier {
            Some(barrier) => {
                let removable = !removable_roots_for_paths(std::slice::from_ref(&barrier.path))
                    .await
                    .is_empty();
                copy_engine::reach(barrier, removable)
                    .await
                    .map_err(|error| device_flush_failure(&error))
            }
            None => Ok(()),
        };
        let mut settled = Vec::with_capacity(self.entries.len());
        for (tag, deletions) in self.entries {
            let result = match &flushed {
                Err(flush) => {
                    // Each other message names where an original stays, and
                    // includes `flush`.
                    let message = deletions
                        .iter()
                        .map(|deletion| deletion.kept(flush))
                        .filter(|message| message != flush)
                        .reduce(|first, second| crate::i18n::join_sentences(&first, &second))
                        .unwrap_or_else(|| flush.clone());
                    Err(io_error(message))
                }
                Ok(()) => {
                    let mut result = Ok(());
                    for deletion in deletions {
                        result = deletion.run(progress, cancellable).await;
                        if result.is_err() {
                            break;
                        }
                    }
                    result
                }
            };
            settled.push((tag, result));
        }
        settled
    }
}

/// Open batches, one per destination filesystem.
struct Batches<T> {
    open: Vec<Batch<T>>,
}

impl<T> Batches<T> {
    /// Adds what an item that `copied` bytes owes to its filesystem's batch.
    fn add(&mut self, tag: T, owed: Owed, copied: u64) {
        let key = owed.barrier.as_ref().map(|barrier| barrier.key);
        let index = match self
            .open
            .iter()
            .position(|batch| key.is_some() && batch.key == key)
        {
            Some(index) => index,
            None => {
                self.open.push(Batch::new(key));
                self.open.len() - 1
            }
        };
        self.open[index].add(tag, owed, copied);
    }

    fn take(&mut self, mut close: impl FnMut(&Batch<T>) -> bool) -> Vec<Batch<T>> {
        let (closed, open) = std::mem::take(&mut self.open)
            .into_iter()
            .partition(|batch| close(batch));
        self.open = open;
        closed
    }
}

/// A paste item that is done once what it owes is deleted, with where a
/// move put it.
pub(super) struct Done {
    pub index: usize,
    pub moved_to: Option<Location>,
}

/// A paste's items as they are done: an item that owes deletions is done,
/// and reported moved, only once its batch closes.
pub(super) struct Settling<'a> {
    request: &'a PasteRequest,
    emit: Rc<dyn Fn(OperationEvent)>,
    progress: Rc<TransferProgressTracker>,
    batches: Batches<Done>,
    completed: Vec<usize>,
    failed: Vec<usize>,
    failures: Vec<String>,
}

impl<'a> Settling<'a> {
    pub(super) fn new(
        request: &'a PasteRequest,
        emit: Rc<dyn Fn(OperationEvent)>,
        progress: Rc<TransferProgressTracker>,
    ) -> Self {
        Self {
            request,
            emit,
            progress,
            batches: Batches { open: Vec::new() },
            completed: Vec::new(),
            failed: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn done(&mut self, done: Done) {
        let item = &self.request.items[done.index];
        if self.request.move_sources
            && let Some(to) = done.moved_to
            && to != item.source
        {
            (self.emit)(OperationEvent::ItemMoved {
                request_id: self.request.id,
                from: item.source.clone(),
                to,
                merged: item.conflict == TransferConflict::Merge,
            });
        }
        self.completed.push(done.index);
    }

    /// The item, which copied `copied` bytes, is done once what it owes is
    /// deleted; batches that are due close.
    pub(super) async fn owe(&mut self, done: Done, owed: Owed, copied: u64) {
        if owed.deletions.is_empty() {
            self.done(done);
            return;
        }
        self.batches.add(done, owed, copied);
        let due = self.batches.take(Batch::due);
        self.close(due).await;
    }

    async fn close(&mut self, batches: Vec<Batch<Done>>) {
        // What complete copies owe is finished whatever stops the paste.
        let cancellable = gio::Cancellable::new();
        for batch in batches {
            for (done, result) in batch.close(Some(&self.progress), &cancellable).await {
                match result {
                    Ok(()) => self.done(done),
                    Err(error) => {
                        self.failed.push(done.index);
                        let message = crate::services::gio_error_message(&error);
                        if !self.failures.contains(&message) {
                            self.failures.push(message);
                        }
                    }
                }
            }
        }
    }

    /// Closes every batch; the last barrier shows as writing to the device.
    pub(super) async fn finish(&mut self) {
        let batches = self.batches.take(|_| true);
        if batches.iter().any(|batch| batch.barrier.is_some()) {
            (self.emit)(OperationEvent::FlushingToDevice {
                request_id: self.request.id,
            });
        }
        self.close(batches).await;
    }

    pub(super) fn failure(&self) -> Option<String> {
        self.failures
            .iter()
            .cloned()
            .reduce(|first, second| crate::i18n::join_sentences(&first, &second))
    }

    fn locations(&self, indices: &[usize]) -> Vec<Location> {
        let mut indices = indices.to_vec();
        indices.sort_unstable();
        indices
            .into_iter()
            .map(|index| self.request.items[index].source.clone())
            .collect()
    }

    /// The items done so far, in the paste's order.
    pub(super) fn completed(&self) -> Vec<Location> {
        self.locations(&self.completed)
    }

    pub(super) fn failed(&self) -> Vec<Location> {
        self.locations(&self.failed)
    }
}
