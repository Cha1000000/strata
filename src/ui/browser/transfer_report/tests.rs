// SPDX-License-Identifier: MIT

use std::rc::Rc;

use gtk::prelude::*;

use super::*;
use crate::app::BrowserEvent;
use crate::services::OperationRequestId;
use crate::ui::browser::{BrowserView, PeekBehavior};

fn renamed(count: usize) -> TransferReport {
    TransferReport {
        renamed: (0..count)
            .map(|index| RenamedItem {
                requested: Location::local(format!("/copy/item {index}")),
                saved_as: Location::local(format!("/copy/item {index} (1)")),
                cause: RenameCause::Alias,
            })
            .collect(),
    }
}

#[test]
fn a_few_renamed_items_get_a_notice_and_more_get_a_dialog() {
    assert_eq!(presentation(&TransferReport::default()), None);
    for (count, listed) in [(1, 1), (2, 2), (3, 3), (9, LISTED_ITEMS)] {
        match presentation(&renamed(count)) {
            Some(Presentation::Notice { text, .. }) => {
                assert!(count <= NOTICE_ITEMS, "{count}");
                assert_eq!(text.lines().count(), listed);
            }
            Some(Presentation::Information {
                message, detail, ..
            }) => {
                assert!(count > NOTICE_ITEMS, "{count}");
                assert_eq!(message, crate::i18n::count("items_renamed", count));
                assert_eq!(detail.matches('•').count(), listed, "{count}");
                assert_eq!(
                    detail.ends_with(&crate::i18n::count("more_items", count - listed)),
                    count > listed,
                    "{count}"
                );
            }
            None => panic!("{count} renamed items are reported"),
        }
    }
}

fn shows(overlay: &gtk::Overlay, class: &str) -> bool {
    let mut stack = vec![overlay.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if widget.has_css_class(class) {
            return true;
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            child = next.next_sibling();
            stack.push(next);
        }
    }
    false
}

#[test]
fn a_report_after_a_transfer_shows_its_notice_or_dialog() {
    crate::test_support::gtk_test(
        "ui::browser::transfer_report::tests::a_report_after_a_transfer_shows_its_notice_or_dialog",
        || {
            for (count, background, dialog) in [
                (1, false, false),
                (3, false, true),
                (2, true, false),
                (3, true, true),
            ] {
                let view = BrowserView::new(
                    Rc::new(crate::adapters::LocalFileSource),
                    PeekBehavior::default(),
                );
                let overlay = view.overlay();
                let window = gtk::Window::builder().child(&overlay).build();
                window.present();
                let event = BrowserEvent::ItemsReported {
                    report: renamed(count),
                };
                let event = if background {
                    BrowserEvent::BackgroundOperation {
                        request_id: OperationRequestId(1),
                        event: Box::new(event),
                    }
                } else {
                    event
                };

                view.state.handle(&event);

                let context = gtk::glib::MainContext::default();
                while context.pending() {
                    context.iteration(false);
                }
                let row = format!("{count} items, background: {background}");
                assert_eq!(shows(&overlay, "overlay-notice"), !dialog, "{row}");
                assert_eq!(shows(&overlay, "app-modal-layer"), dialog, "{row}");
                window.destroy();
            }
        },
    );
}
