// SPDX-License-Identifier: MIT

use std::{cell::RefCell, collections::HashSet, ffi::OsString, rc::Weak};

use super::{Browser, BrowserEvent};
use crate::{
    app::navigation::LoadState,
    model::{FileEntry, Location, hidden_names::Leftover},
    services::file_activity::{self, Notice, Subscription},
};

thread_local! {
    static ANNOUNCED: RefCell<HashSet<Location>> = RefCell::default();
}

fn lists_leftovers(location: &Location) -> bool {
    location.native_path().is_some()
        || location.uri_value().is_some_and(|uri| {
            !uri.get(..6)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("trash:"))
                && !location.is_recent_location()
        })
}

fn holds_original(kind: Leftover) -> bool {
    matches!(
        kind,
        Leftover::Originals
            | Leftover::MergeOriginals
            | Leftover::Original
            | Leftover::MaybeOriginal
    )
}

impl Browser {
    pub(super) fn follow_ended_operations(browser: Weak<Self>) -> Subscription {
        file_activity::observe(move |notice| {
            if let Notice::Ended(_) = notice
                && let Some(browser) = browser.upgrade()
            {
                browser.sync_all_leftovers();
            }
        })
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the leftover review lists them")
    )]
    pub(crate) fn column_leftovers(&self, depth: usize) -> Vec<(FileEntry, Leftover)> {
        let Some(positions) = self.surfaced_positions(depth) else {
            return Vec::new();
        };
        let state = self.state.borrow();
        let Some(column) = state.columns.get(depth) else {
            return Vec::new();
        };
        positions
            .into_iter()
            .filter_map(|(position, kind)| Some((column.entries.get(position)?.clone(), kind)))
            .collect()
    }

    fn surfaced_positions(&self, depth: usize) -> Option<Vec<(usize, Leftover)>> {
        let (location, listed) = {
            let state = self.state.borrow();
            let column = state.columns.get(depth)?;
            if column.load_state == LoadState::Loading {
                return None;
            }
            let surfaces = !self.chooser_mode.get()
                && matches!(column.load_state, LoadState::Ready | LoadState::Empty)
                && lists_leftovers(&column.location);
            if !surfaces {
                return Some(Vec::new());
            }
            (column.location.clone(), state.column_leftover_names(depth))
        };
        if Self::operation_touches(&location) {
            return Some(Vec::new());
        }
        Some(
            listed
                .iter()
                .copied()
                .filter(|(_, kind)| *kind != Leftover::Probe)
                .collect(),
        )
    }

    pub(super) fn follow_leftovers(&self, event: &BrowserEvent) {
        match event {
            BrowserEvent::LoadFinished { depth, .. } => {
                self.sweep_listed_probes(*depth);
                self.sync_leftovers(*depth);
            }
            BrowserEvent::EntriesInserted { depth, .. }
            | BrowserEvent::EntriesReplaced { depth, .. }
            | BrowserEvent::EntriesPublished { depth, .. }
            | BrowserEvent::EntriesSpliced { depth, .. }
            | BrowserEvent::ColumnReloaded { depth }
            | BrowserEvent::LoadFailed { depth, .. } => self.sync_leftovers(*depth),
            _ => {}
        }
    }

    fn sync_all_leftovers(&self) {
        let columns = self.state.borrow().columns.len();
        for depth in 0..columns {
            self.sync_leftovers(depth);
        }
    }

    fn sync_leftovers(&self, depth: usize) {
        let Some(surfaced) = self.surfaced_positions(depth) else {
            return;
        };
        let announce = surfaced.iter().any(|(_, kind)| holds_original(*kind))
            && self
                .location_at(depth)
                .is_some_and(|folder| ANNOUNCED.with_borrow_mut(|seen| seen.insert(folder)));
        let previous = self
            .state
            .borrow()
            .columns
            .get(depth)
            .map(|column| column.surfaced_leftovers.replace(surfaced.len()));
        if previous.is_some_and(|previous| previous != surfaced.len()) || announce {
            self.emit(BrowserEvent::LeftoversChanged { depth, announce });
        }
    }

    fn sweep_listed_probes(&self, depth: usize) {
        if self.chooser_mode.get() {
            return;
        }
        let Some(provider) = self.operation_provider.borrow().clone() else {
            return;
        };
        let (location, names): (Location, Vec<OsString>) = {
            let state = self.state.borrow();
            let Some(column) = state.columns.get(depth) else {
                return;
            };
            let names = state
                .column_leftover_names(depth)
                .iter()
                .filter(|(_, kind)| *kind == Leftover::Probe)
                .filter_map(|&(position, _)| {
                    Some(column.entries.get(position)?.native_name.clone())
                })
                .collect();
            (column.location.clone(), names)
        };
        if !names.is_empty() && location.native_path().is_some() {
            provider.sweep_probes(&location, &names);
        }
    }
}
