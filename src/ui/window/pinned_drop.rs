// SPDX-License-Identifier: MIT

#[cfg(test)]
mod tests;

use std::{cell::RefCell, future::Future, rc::Rc, time::Duration};

use gtk::{gdk, glib, prelude::*};

use super::SidebarState;
use crate::{model::Location, ui::browser::active_drag_snapshot};

type FolderResult = Option<Rc<[Location]>>;
type Completion = Box<dyn FnOnce(FolderResult)>;

thread_local! {
    static ACTIVE_REORDER: RefCell<Option<(glib::WeakRef<gdk::ContentProvider>, Location)>> = const { RefCell::new(None) };
}

pub(super) fn start_reorder(content: &gdk::ContentProvider, location: Location) {
    ACTIVE_REORDER.with(|active| active.replace(Some((content.downgrade(), location))));
}

pub(super) fn clear_reorder() {
    ACTIVE_REORDER.with(|active| active.take());
}

fn reorder_source(drop: &gdk::Drop) -> Option<Location> {
    let content = drop.drag()?.content();
    ACTIVE_REORDER.with(|active| {
        active
            .borrow()
            .as_ref()
            .filter(|(provider, _)| provider.upgrade().as_ref() == Some(&content))
            .map(|(_, location)| location.clone())
    })
}

#[derive(Clone)]
enum ValidationResult {
    Pending,
    Complete(FolderResult),
}

struct FolderValidation {
    result: RefCell<ValidationResult>,
    completions: RefCell<Vec<Completion>>,
    job: RefCell<Option<glib::JoinHandle<()>>>,
}

impl FolderValidation {
    fn ready(result: FolderResult) -> Rc<Self> {
        Rc::new(Self {
            result: RefCell::new(ValidationResult::Complete(result)),
            completions: RefCell::new(Vec::new()),
            job: RefCell::new(None),
        })
    }

    fn start(query: impl Future<Output = FolderResult> + 'static, timeout: Duration) -> Rc<Self> {
        let validation = Self::ready(None);
        validation.result.replace(ValidationResult::Pending);
        let weak = Rc::downgrade(&validation);
        let job = glib::MainContext::ref_thread_default().spawn_local(async move {
            let result = futures_lite::future::race(query, async move {
                glib::timeout_future(timeout).await;
                None
            })
            .await;
            if let Some(validation) = weak.upgrade() {
                validation.finish(result);
            }
        });
        validation.job.replace(Some(job));
        validation
    }

    fn finish(&self, result: FolderResult) {
        if matches!(*self.result.borrow(), ValidationResult::Complete(_)) {
            return;
        }
        self.result
            .replace(ValidationResult::Complete(result.clone()));
        let completions = self.completions.take();
        for completion in completions {
            completion(result.clone());
        }
    }

    fn on_ready(&self, completion: impl FnOnce(FolderResult) + 'static) {
        let result = self.result.borrow().clone();
        match result {
            ValidationResult::Pending => self.completions.borrow_mut().push(Box::new(completion)),
            ValidationResult::Complete(result) => completion(result),
        }
    }

    fn action(&self, preferred: gdk::DragAction) -> gdk::DragAction {
        match &*self.result.borrow() {
            ValidationResult::Complete(None) => gdk::DragAction::empty(),
            // Retain the pin intent if released before validation finishes.
            ValidationResult::Pending | ValidationResult::Complete(Some(_)) => preferred,
        }
    }
}

impl Drop for FolderValidation {
    fn drop(&mut self) {
        if let Some(job) = self.job.get_mut().take() {
            job.abort();
        }
        self.finish(None);
    }
}

#[derive(Default)]
pub(super) struct FolderDragCache {
    current: RefCell<Option<(glib::WeakRef<gdk::Drop>, Rc<FolderValidation>)>>,
}

impl FolderDragCache {
    fn validation(&self, drop: &gdk::Drop) -> Rc<FolderValidation> {
        if let Some((cached_drop, validation)) = self.current.borrow().as_ref()
            && cached_drop.upgrade().as_ref() == Some(drop)
        {
            return validation.clone();
        }
        let validation = if let Some(location) = reorder_source(drop) {
            FolderValidation::ready(Some(vec![location].into()))
        } else if let Some(snapshot) = active_drag_snapshot(drop) {
            FolderValidation::ready(
                (snapshot.all_directories && !snapshot.locations.is_empty())
                    .then_some(snapshot.locations),
            )
        } else {
            let drop = drop.clone();
            FolderValidation::start(
                async move {
                    let value = drop
                        .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
                        .await
                        .ok()?;
                    let files = value.get::<gdk::FileList>().ok()?;
                    let locations = files
                        .files()
                        .iter()
                        .map(crate::adapters::location_for_file)
                        .collect::<Option<Vec<_>>>()?;
                    crate::adapters::directory_validation::all_directories(&locations)
                        .await
                        .then(|| locations.into())
                },
                Duration::from_secs(2),
            )
        };
        // Drop the old request outside the borrow: cancellation can complete a pending drop.
        let old = self
            .current
            .replace(Some((drop.downgrade(), validation.clone())));
        std::mem::drop(old);
        validation
    }
}

fn pin_action(drop: &gdk::Drop) -> gdk::DragAction {
    if drop.actions().contains(gdk::DragAction::MOVE) && reorder_source(drop).is_some() {
        gdk::DragAction::MOVE
    } else if drop.actions().contains(gdk::DragAction::COPY) {
        gdk::DragAction::COPY
    } else if drop.actions().contains(gdk::DragAction::MOVE) && active_drag_snapshot(drop).is_some()
    {
        // Our sources do not delete data on drag-end. Foreign MOVE completions
        // may request deletion, so only provider-matched internal drags can use it.
        gdk::DragAction::MOVE
    } else {
        gdk::DragAction::empty()
    }
}

struct PinDropSite {
    widget: glib::WeakRef<gtk::Box>,
    hover_drop: RefCell<Option<gdk::Drop>>,
}

impl PinDropSite {
    fn enter(self: &Rc<Self>, drop: &gdk::Drop, cache: &FolderDragCache) -> gdk::DragAction {
        self.leave();
        self.hover_drop.replace(Some(drop.clone()));
        let validation = cache.validation(drop);
        let action = validation.action(pin_action(drop));
        let site = Rc::downgrade(self);
        let drop = drop.clone();
        validation.on_ready(move |locations| {
            let Some(site) = site.upgrade() else { return };
            if site.hover_drop.borrow().as_ref() != Some(&drop) {
                return;
            }
            if let Some(widget) = site.widget.upgrade()
                && widget.root().is_some()
            {
                let action = if locations.is_some() {
                    pin_action(&drop)
                } else {
                    gdk::DragAction::empty()
                };
                if action.is_empty() {
                    widget.remove_css_class("eligible");
                } else {
                    widget.add_css_class("eligible");
                }
                drop.status(action, action);
            }
        });
        action
    }

    fn leave(&self) {
        self.hover_drop.take();
        if let Some(widget) = self.widget.upgrade() {
            widget.remove_css_class("eligible");
        }
    }
}

pub(super) fn append_gap(
    state: &Rc<SidebarState>,
    before: Option<Location>,
    cache: &Rc<FolderDragCache>,
) {
    let gap = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .accessible_role(gtk::AccessibleRole::Group)
        .build();
    gap.add_css_class("sidebar-pin-gap");
    crate::ui::accessibility::set_label(&gap, &crate::i18n::tr("PINNED"));
    let target = gtk::DropTargetAsync::new(
        Some(
            gdk::ContentFormats::for_type(gdk::FileList::static_type())
                .union(&gdk::ContentFormats::for_type(String::static_type())),
        ),
        gdk::DragAction::COPY | gdk::DragAction::MOVE,
    );
    target.connect_accept(|_, drop| {
        !pin_action(drop).is_empty()
            && (reorder_source(drop).is_some()
                || drop
                    .formats()
                    .union_deserialize_types()
                    .contains_type(gdk::FileList::static_type()))
    });
    let site = Rc::new(PinDropSite {
        widget: gap.downgrade(),
        hover_drop: RefCell::new(None),
    });
    let enter_site = site.clone();
    let enter_cache = cache.clone();
    target.connect_drag_enter(move |_, drop, _, _| enter_site.enter(drop, &enter_cache));
    let motion_cache = cache.clone();
    target.connect_drag_motion(move |_, drop, _, _| {
        motion_cache.validation(drop).action(pin_action(drop))
    });
    let leave_site = site.clone();
    target.connect_drag_leave(move |_, _| leave_site.leave());
    let weak_state = Rc::downgrade(state);
    let drop_cache = cache.clone();
    let widget = gap.downgrade();
    target.connect_drop(move |_, drop, _, _| {
        let action = pin_action(drop);
        if action.is_empty() {
            return false;
        }
        site.leave();
        let validation = drop_cache.validation(drop);
        let state = weak_state.clone();
        let widget = widget.clone();
        let before = before.clone();
        let reorder = reorder_source(drop).is_some();
        let drop = drop.clone();
        validation.on_ready(move |locations| {
            let success = match (locations, state.upgrade(), widget.upgrade()) {
                (Some(locations), Some(state), Some(widget)) if widget.root().is_some() => {
                    if reorder {
                        locations.first().is_some_and(|source| {
                            state.reorder_pinned_before(source, before.as_ref())
                        })
                    } else {
                        state.pin_locations_before(&locations, before.as_ref())
                    }
                }
                _ => false,
            };
            drop.finish(if success {
                action
            } else {
                gdk::DragAction::empty()
            });
        });
        true
    });
    gap.add_controller(target);
    state.widget.append(&gap);
}
