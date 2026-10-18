// SPDX-License-Identifier: MIT

use std::{cell::Cell, rc::Rc};

use gtk::{gdk, glib, prelude::*};

pub(super) fn wrap(scroll: &gtk::ScrolledWindow) -> gtk::Box {
    let (_, bar) = StableScrollbar::new(scroll);
    scroll.set_policy(scroll.hscrollbar_policy(), gtk::PolicyType::External);
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    container.set_vexpand(true);
    container.append(scroll);
    container.append(&bar);
    container
}

struct StableScrollbar {
    document: gtk::Adjustment,
    thumb: gtk::Adjustment,
    bar: glib::WeakRef<gtk::Scrollbar>,
    dragging: Cell<bool>,
    previous_thumb_value: Cell<f64>,
    drag_generation: Cell<u64>,
    pending_edge: Cell<Option<u64>>,
    syncing: Cell<bool>,
}

impl StableScrollbar {
    fn new(scroll: &gtk::ScrolledWindow) -> (Rc<Self>, gtk::Scrollbar) {
        let thumb = gtk::Adjustment::default();
        let bar = gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&thumb));
        let state = Rc::new(Self {
            document: scroll.vadjustment(),
            bar: bar.downgrade(),
            thumb,
            dragging: Cell::new(false),
            previous_thumb_value: Cell::new(0.0),
            drag_generation: Cell::new(0),
            pending_edge: Cell::new(None),
            syncing: Cell::new(false),
        });
        let weak = Rc::downgrade(&state);
        state.document.connect_changed(move |_| {
            if let Some(state) = weak.upgrade() {
                state.document_changed();
            }
        });
        let weak = Rc::downgrade(&state);
        state.document.connect_value_changed(move |_| {
            if let Some(state) = weak.upgrade() {
                state.document_changed();
            }
        });
        let weak = Rc::downgrade(&state);
        state.thumb.connect_value_changed(move |_| {
            if let Some(state) = weak.upgrade() {
                state.scroll();
            }
        });

        let events = gtk::EventControllerLegacy::new();
        events.set_propagation_phase(gtk::PropagationPhase::Capture);
        let for_events = state.clone();
        events.connect_event(move |_, event| {
            match event.event_type() {
                gdk::EventType::ButtonPress | gdk::EventType::TouchBegin => {
                    for_events.begin_drag();
                }
                gdk::EventType::ButtonRelease
                | gdk::EventType::TouchEnd
                | gdk::EventType::TouchCancel
                | gdk::EventType::GrabBroken => {
                    for_events.finish_after_event();
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        bar.add_controller(events);
        let weak = Rc::downgrade(&state);
        bar.connect_unmap(move |_| {
            if let Some(state) = weak.upgrade() {
                state.finish_drag();
            }
        });
        state.sync();
        (state, bar)
    }

    fn sync(&self) {
        // ListView re-estimates unbound row heights and re-anchors its value.
        // Feeding those corrections back into a held thumb repeats the scroll.
        if self.dragging.get() || self.syncing.replace(true) {
            return;
        }
        self.thumb.configure(
            self.document.value(),
            self.document.lower(),
            self.document.upper(),
            self.document.step_increment(),
            self.document.page_increment(),
            self.document.page_size(),
        );
        self.syncing.set(false);
        if let Some(bar) = self.bar.upgrade() {
            bar.set_visible(scrollable_range(&self.document) > 0.0);
        }
    }

    fn document_changed(self: &Rc<Self>) {
        self.sync();
        if !self.dragging.get()
            || self
                .edge_target()
                .is_none_or(|target| self.document.value() == target)
        {
            return;
        }
        let generation = self.drag_generation.get();
        if self.pending_edge.replace(Some(generation)) == Some(generation) {
            return;
        }
        let weak = Rc::downgrade(self);
        // Re-anchoring can pull an edge request into the document. Correct it
        // after allocation, but never reapply an interior thumb position.
        glib::idle_add_local_once(move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if state.pending_edge.get() != Some(generation) {
                return;
            }
            state.pending_edge.set(None);
            if state.dragging.get()
                && state.drag_generation.get() == generation
                && let Some(target) = state.edge_target()
            {
                state.document.set_value(target);
            }
        });
    }

    fn edge_target(&self) -> Option<f64> {
        if self.thumb.value() == self.thumb.lower() {
            Some(self.document.lower())
        } else if self.thumb.value() == self.thumb.lower() + scrollable_range(&self.thumb) {
            Some(self.document.lower() + scrollable_range(&self.document))
        } else {
            None
        }
    }

    fn scroll(&self) {
        if self.syncing.get() {
            return;
        }
        let previous = self.previous_thumb_value.replace(self.thumb.value());
        let value = if self.dragging.get() {
            let range = scrollable_range(&self.thumb);
            let fraction = if range > 0.0 {
                (self.thumb.value() - self.thumb.lower()) / range
            } else {
                0.0
            };
            let target = self.document.lower() + fraction * scrollable_range(&self.document);
            // A height re-estimate can put the target behind GTK's visible-row
            // anchor. Never scroll against the user's actual thumb movement.
            if self.thumb.value() > previous {
                target.max(self.document.value())
            } else {
                target.min(self.document.value())
            }
        } else {
            self.thumb.value()
        };
        self.document.set_value(value);
    }

    fn begin_drag(&self) {
        self.previous_thumb_value.set(self.thumb.value());
        self.drag_generation
            .set(self.drag_generation.get().wrapping_add(1));
        self.dragging.set(true);
    }

    fn finish_after_event(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let generation = self.drag_generation.get();
        glib::idle_add_local_once(move || {
            if let Some(state) = weak.upgrade()
                && state.drag_generation.get() == generation
            {
                state.finish_drag();
            }
        });
    }

    fn finish_drag(&self) {
        self.pending_edge.set(None);
        if self.dragging.replace(false) {
            self.sync();
        }
    }
}

fn scrollable_range(adjustment: &gtk::Adjustment) -> f64 {
    (adjustment.upper() - adjustment.lower() - adjustment.page_size()).max(0.0)
}

#[cfg(test)]
mod tests;
