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
fn one_short_renamed_line_gets_a_notice_and_anything_more_a_dialog() {
    assert_eq!(presentation(&TransferReport::default(), false), None);
    let long = "a name long enough that the notice would cut its line short";
    let long = TransferReport {
        renamed: vec![RenamedItem {
            requested: Location::local(format!("/copy/{long}")),
            saved_as: Location::local(format!("/copy/{long} (1)")),
            cause: RenameCause::Alias,
        }],
        ..TransferReport::default()
    };
    for (report, notice, listed) in [
        (renamed(1), true, 1),
        (long, false, 1),
        (renamed(2), false, 2),
        (renamed(9), false, LISTED_ITEMS),
    ] {
        let count = report.renamed.len();
        match presentation(&report, false) {
            Some(Presentation::Notice { text, .. }) => {
                assert!(notice, "{count}");
                assert_eq!(text, renamed_line(&report.renamed[0]));
            }
            Some(Presentation::Information {
                message, detail, ..
            }) => {
                assert!(!notice, "{count}");
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
fn an_item_saved_under_two_names_in_turn_is_reported_once() {
    let item = |requested: &str, saved_as: &str, cause| RenamedItem {
        requested: Location::local(format!("/copy/{requested}")),
        saved_as: Location::local(format!("/copy/{saved_as}")),
        cause,
    };
    // An alias numbered in a merge stage, whose number was taken in the
    // merged folder, reported in either order.
    let alias = item("makefile", "makefile (1)", RenameCause::Alias);
    let taken = item("makefile (1)", "makefile (2)", RenameCause::Taken);
    for renamed in [vec![alias.clone(), taken.clone()], vec![taken, alias]] {
        let report = TransferReport {
            renamed,
            ..TransferReport::default()
        };

        let Some(Presentation::Notice {
            heading: shown,
            text,
        }) = presentation(&report, false)
        else {
            panic!("one renamed item gets a notice");
        };

        assert_eq!(shown, heading(RenameCause::Alias));
        assert_eq!(
            text,
            renamed_line(&item("makefile", "makefile (2)", RenameCause::Alias))
        );
    }
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
    let denied = rustix::io::Errno::ACCESS.raw_os_error();
    let reason = crate::services::io_error_detail(&std::io::Error::from_raw_os_error(denied));
    let left_behind = vec![
        kept(KeptCause::Changed),
        kept(KeptCause::Undeletable(denied)),
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
        assert_eq!(detail.contains(&reason), kept_count > 0, "{row}");
    }
}

fn shows(overlay: &gtk::Overlay, class: &str) -> bool {
    shows_widget(overlay, |widget| widget.has_css_class(class))
}

fn shows_text(overlay: &gtk::Overlay, text: &str) -> bool {
    shows_widget(overlay, |widget| {
        widget
            .downcast_ref::<gtk::Label>()
            .is_some_and(|label| label.label() == text)
    })
}

fn shows_widget(overlay: &gtk::Overlay, found: impl Fn(&gtk::Widget) -> bool) -> bool {
    let mut stack = vec![overlay.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if found(&widget) {
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
                (renamed(1), true, false),
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
                // The notice a Send to that finished quickly shows.
                let sent = "Copied to USB";
                crate::ui::notice::show_notice(&overlay, None, sent, NOTICE_DURATION);
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
                let report_notice = shows_text(&overlay, &renamed_line(&renamed(1).renamed[0]));
                assert_eq!(report_notice, !dialog, "{row}");
                assert_eq!(shows(&overlay, "app-modal-layer"), dialog, "{row}");
                assert!(shows_text(&overlay, sent), "{row}");
                window.destroy();
            }
        },
    );
}
