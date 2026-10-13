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
        ..TransferReport::default()
    }
}

#[test]
fn a_few_renamed_items_get_a_notice_and_more_get_a_dialog() {
    assert_eq!(presentation(&TransferReport::default(), false), None);
    for (count, listed) in [(1, 1), (2, 2), (3, 3), (9, LISTED_ITEMS)] {
        match presentation(&renamed(count), false) {
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
            other => panic!("{count} renamed items are reported: {other:?}"),
        }
    }
}

#[test]
fn renamed_items_are_grouped_by_why_they_were_renamed() {
    let mut report = renamed(2);
    report.renamed[1].cause = RenameCause::Taken;

    let Some(Presentation::Information { detail, .. }) = presentation(&report, false) else {
        panic!("two causes need a dialog");
    };

    for cause in [RenameCause::Alias, RenameCause::Taken] {
        assert_eq!(detail.matches(&heading(cause)).count(), 1, "{cause:?}");
    }
    let only_taken = TransferReport {
        renamed: report.renamed[1..].to_vec(),
        ..TransferReport::default()
    };
    let Some(Presentation::Notice { heading: shown, .. }) = presentation(&only_taken, false) else {
        panic!("one renamed item gets a notice");
    };
    assert_eq!(shown, heading(RenameCause::Taken));
}

#[test]
fn items_left_out_or_left_behind_make_the_report_a_warning() {
    let skipped = |kind| SkippedItem {
        location: Location::local("/copy/run/socket"),
        kind,
    };
    let kept = |cause| KeptItem {
        location: Location::local("/source/run/data"),
        cause,
    };
    let reason = "Permission denied";
    let left_behind = vec![
        kept(KeptCause::Changed),
        kept(KeptCause::Undeletable(reason.to_owned())),
        kept(KeptCause::Skipped(SpecialKind::Socket)),
    ];
    // A transfer that failed or was cancelled afterwards is not called
    // complete.
    for (renamed_count, skipped_count, kept_count, stopped) in [
        (0, 2, 0, false),
        (1, 2, 0, false),
        (3, 2, 0, false),
        (0, 0, 3, false),
        (1, 2, 3, false),
        (1, 2, 3, true),
    ] {
        let row = format!("{renamed_count}, {skipped_count}, {kept_count}, {stopped}");
        let report = TransferReport {
            skipped: [skipped(SpecialKind::Socket), skipped(SpecialKind::Pipe)][..skipped_count]
                .to_vec(),
            kept: left_behind[..kept_count].to_vec(),
            ..renamed(renamed_count)
        };
        let Some(Presentation::Warning { title, detail }) = presentation(&report, stopped) else {
            panic!("{row}");
        };
        let expected = if stopped {
            crate::i18n::tr("Some items were left out or left behind")
        } else {
            crate::i18n::tr("Completed with errors")
        };
        assert_eq!(title, expected, "{row}");
        assert_eq!(
            detail.matches('•').count(),
            renamed_count + skipped_count + kept_count,
            "{row}"
        );
        let heading = crate::i18n::count("items_kept", kept_count);
        assert_eq!(detail.contains(&heading), kept_count > 0, "{row}");
        assert_eq!(detail.contains(reason), kept_count > 0, "{row}");
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
            let left_out = TransferReport {
                skipped: vec![SkippedItem {
                    location: Location::local("/copy/run/socket"),
                    kind: SpecialKind::Socket,
                }],
                ..TransferReport::default()
            };
            for (report, background, dialog) in [
                (renamed(1), false, false),
                (renamed(3), false, true),
                (renamed(2), true, false),
                (renamed(3), true, true),
                (left_out, false, true),
            ] {
                let row = format!("{report:?}, background: {background}");
                let view = BrowserView::new(
                    Rc::new(crate::adapters::LocalFileSource),
                    PeekBehavior::default(),
                );
                let overlay = view.overlay();
                let window = gtk::Window::builder().child(&overlay).build();
                window.present();
                let event = BrowserEvent::ItemsReported {
                    report,
                    stopped: false,
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
                assert_eq!(shows(&overlay, "overlay-notice"), !dialog, "{row}");
                assert_eq!(shows(&overlay, "app-modal-layer"), dialog, "{row}");
                window.destroy();
            }
        },
    );
}
