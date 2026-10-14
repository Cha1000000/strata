// SPDX-License-Identifier: MIT

use std::rc::Rc;

use gtk::prelude::*;

use super::*;
use crate::app::BrowserEvent;
use crate::services::{OperationEvent, OperationRequestId, PasteItem, TransferConflict};
use crate::test_support::operations::HeldOperations;
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
    assert_eq!(presentation(&TransferReport::default()), None);
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
        match presentation(&report) {
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

    let Some(Presentation::Information { detail, .. }) = presentation(&report) else {
        panic!("two causes need a dialog");
    };

    for cause in [RenameCause::Alias, RenameCause::Taken] {
        assert_eq!(detail.matches(&heading(cause)).count(), 1, "{cause:?}");
    }
    let only_taken = TransferReport {
        renamed: report.renamed[1..].to_vec(),
        ..TransferReport::default()
    };
    let Some(Presentation::Notice { heading: shown, .. }) = presentation(&only_taken) else {
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
        }) = presentation(&report)
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
fn items_left_out_or_left_behind_make_the_report_a_warning_or_follow_a_stop() {
    let skipped = |kind| SkippedItem {
        location: Location::local("/copy/run/socket"),
        kind,
    };
    let kept = |cause| KeptItem {
        location: Location::local("/source/run/data"),
        cause,
    };
    let in_trash = |cause| KeptItem {
        location: Location::local("/trash/files/run/data"),
        cause,
    };
    let denied = rustix::io::Errno::ACCESS.raw_os_error();
    let reason = crate::services::io_error_detail(&std::io::Error::from_raw_os_error(denied));
    let left_behind = [
        kept(KeptCause::Changed),
        kept(KeptCause::Undeletable(denied)),
        kept(KeptCause::Skipped(SpecialKind::Socket)),
        kept(KeptCause::CopyGone),
    ];
    let left_in_trash = [
        in_trash(KeptCause::Changed),
        in_trash(KeptCause::Undeletable(denied)),
    ];
    for (renamed_count, skipped_count, kept_count, in_trash_count) in [
        (0, 2, 0, 0),
        (1, 2, 0, 0),
        (3, 2, 0, 0),
        (0, 0, 4, 0),
        (1, 2, 4, 0),
        (0, 0, 0, 2),
    ] {
        let row = format!("{renamed_count}, {skipped_count}, {kept_count}, {in_trash_count}");
        let report = TransferReport {
            skipped: [skipped(SpecialKind::Socket), skipped(SpecialKind::Pipe)][..skipped_count]
                .to_vec(),
            kept: left_behind[..kept_count].to_vec(),
            kept_in_trash: left_in_trash[..in_trash_count].to_vec(),
            ..renamed(renamed_count)
        };
        let Some(Presentation::Warning { detail }) = presentation(&report) else {
            panic!("{row}");
        };
        let outcome = "Could not copy keys: Permission denied";
        assert_eq!(
            stopped_detail(outcome.to_owned(), &report),
            format!("{outcome}\n\n{detail}"),
            "{row}"
        );
        assert_eq!(
            detail.matches('•').count(),
            renamed_count + skipped_count + kept_count + in_trash_count,
            "{row}"
        );
        let heading = crate::i18n::count("items_kept", kept_count);
        assert_eq!(detail.contains(&heading), kept_count > 0, "{row}");
        let changed = rust_i18n::t!(
            "“%{name}” changed during the move and was left in “%{folder}”",
            name = "data",
            folder = "run"
        );
        assert_eq!(detail.contains(&*changed), kept_count > 0, "{row}");
        let gone = rust_i18n::t!(
            "“%{name}” was left in “%{folder}”, as its copy was no longer where the move put it",
            name = "data",
            folder = "run"
        );
        assert_eq!(detail.contains(&*gone), kept_count > 0, "{row}");
        let heading = crate::i18n::count("items_kept_in_trash", in_trash_count);
        assert_eq!(detail.contains(&heading), in_trash_count > 0, "{row}");
        let changed = rust_i18n::t!(
            "“%{name}” changed during the restore and was left in the Trash",
            name = "data"
        );
        assert_eq!(detail.contains(&*changed), in_trash_count > 0, "{row}");
        assert_eq!(
            detail.contains(&reason),
            kept_count + in_trash_count > 0,
            "{row}"
        );
    }
}

#[test]
fn a_stop_lists_renamed_items_by_cause_under_its_outcome() {
    let outcome = "Could not copy keys: Permission denied";
    let mut report = renamed(LISTED_ITEMS + 2);
    report.renamed[0].cause = RenameCause::Taken;

    let detail = stopped_detail(outcome.to_owned(), &report);

    assert!(detail.starts_with(&format!("{outcome}\n\n")), "{detail}");
    for cause in [RenameCause::Alias, RenameCause::Taken] {
        assert_eq!(detail.matches(&heading(cause)).count(), 1, "{cause:?}");
    }
    assert!(detail.contains(&renamed_line(&report.renamed[0])));
    assert_eq!(detail.matches('•').count(), 1 + LISTED_ITEMS);
    assert!(detail.ends_with(&crate::i18n::count("more_items", 1)));
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
                let event = BrowserEvent::ItemsReported { report };
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

fn modal_layers(window: &gtk::Window) -> Vec<gtk::Widget> {
    let mut found = Vec::new();
    let mut stack = vec![window.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = stack.pop() {
        if widget.has_css_class("app-modal-layer") {
            found.push(widget);
            continue;
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            child = next.next_sibling();
            stack.push(next);
        }
    }
    found
}

fn dialogs(window: &gtk::Window) -> Vec<Vec<String>> {
    modal_layers(window)
        .iter()
        .filter(|layer| !crate::ui::modal::is_closing_layer(layer))
        .map(texts)
        .collect()
}

fn texts(root: &gtk::Widget) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            found.push(label.label().to_string());
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            child = next.next_sibling();
            stack.push(next);
        }
    }
    found
}

fn pump_until(done: impl Fn() -> bool) {
    let context = gtk::glib::MainContext::default();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        while context.pending() {
            context.iteration(false);
        }
        if done() {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_stopped_transfer_shows_one_dialog_with_its_outcome_and_report() {
    crate::test_support::gtk_test(
        "ui::browser::transfer_report::tests::a_stopped_transfer_shows_one_dialog_with_its_outcome_and_report",
        || {
            super::super::progress::set_file_progress_delay_for_test(Duration::ZERO);
            let items = TransferReport {
                skipped: vec![SkippedItem {
                    location: Location::local("/destination/run/preview.sock"),
                    kind: SpecialKind::Socket,
                }],
                kept: vec![KeptItem {
                    location: Location::local("/source/run/app.sock"),
                    cause: KeptCause::Skipped(SpecialKind::Socket),
                }],
                ..renamed(1)
            };
            let lines = [
                renamed_line(&items.renamed[0]),
                skipped_line(&items.skipped[0]),
                kept_line(&items.kept[0], false),
            ];
            let failure = "Could not copy keys: Permission denied";
            let cancelled = crate::services::CancelledOperation {
                completed: vec![Location::local("/source/run")],
                not_attempted: vec![Location::local("/source/keys")],
                ..Default::default()
            };
            let summary = super::super::progress::cancelled_operation_summary(1, 0, 1);
            type Terminal = Box<dyn Fn(OperationRequestId) -> OperationEvent>;
            let terminals: [(Terminal, &str, Option<&str>); 3] = [
                (
                    Box::new(|request_id| OperationEvent::TransferFailed {
                        request_id,
                        completed_locations: Vec::new(),
                        message: failure.into(),
                    }),
                    "Unable to complete operation",
                    Some(failure),
                ),
                (
                    Box::new(move |request_id| OperationEvent::Cancelled {
                        request_id,
                        result: cancelled.clone(),
                    }),
                    "Operation cancelled",
                    Some(&summary),
                ),
                (
                    Box::new(|request_id| OperationEvent::Pasted {
                        request_id,
                        locations: Vec::new(),
                    }),
                    "Completed with errors",
                    None,
                ),
            ];
            // A move runs in the foreground and a copy in the background.
            for moving in [true, false] {
                for (terminal, title, outcome) in &terminals {
                    for report in [items.clone(), TransferReport::default()] {
                        let row =
                            format!("moving: {moving}, {title}, items: {}", !report.is_empty());
                        let view = BrowserView::new(
                            Rc::new(crate::adapters::LocalFileSource),
                            PeekBehavior::default(),
                        );
                        let operations = Rc::new(HeldOperations::default());
                        view.browser().set_operation_provider(operations.clone());
                        let window = gtk::Window::builder().child(&view.overlay()).build();
                        window.present();
                        view.browser().transfer(
                            Location::local("/destination"),
                            vec![PasteItem {
                                source: Location::local("/source/run"),
                                conflict: TransferConflict::FailIfExists,
                            }],
                            moving,
                            false,
                        );
                        let id = view.browser().last_started_operation().expect("transfer");
                        let docked = view
                            .state
                            .background_file_progress
                            .borrow()
                            .get(&id)
                            .map(|background| background.progress.clone());
                        assert_eq!(docked.is_some(), !moving, "{row}");
                        let progress = docked.unwrap_or_else(|| view.state.file_progress());
                        pump_until(|| progress.file_progress_view.borrow().is_some());
                        if !report.is_empty() {
                            operations.emit(
                                id,
                                OperationEvent::ItemsReported {
                                    request_id: id,
                                    report: report.clone(),
                                },
                            );
                        }

                        operations.emit(id, terminal(id));

                        pump_until(|| {
                            progress.file_progress_view.borrow().is_none()
                                && !modal_layers(&window)
                                    .iter()
                                    .any(crate::ui::modal::is_closing_layer)
                        });
                        let expected = outcome.is_some() || !report.is_empty();
                        let dialogs = dialogs(&window);
                        assert_eq!(dialogs.len(), usize::from(expected), "{row}: {dialogs:?}");
                        if let [texts] = dialogs.as_slice() {
                            let title = crate::i18n::tr(title);
                            assert!(texts.contains(&title), "{row}: {texts:?}");
                            let detail = match outcome {
                                Some(outcome) => texts
                                    .iter()
                                    .find(|text| text.starts_with(outcome))
                                    .expect("the outcome"),
                                None => texts
                                    .iter()
                                    .find(|text| text.contains(lines[0].as_str()))
                                    .expect("the report"),
                            };
                            if report.is_empty() {
                                assert_eq!(Some(detail.as_str()), *outcome, "{row}");
                            }
                            for line in &lines {
                                assert_eq!(
                                    detail.contains(line.as_str()),
                                    !report.is_empty(),
                                    "{row}: {detail}"
                                );
                            }
                        }
                        view.browser().clear_observer();
                        view.dispose_file_progress();
                        window.destroy();
                    }
                }
            }
        },
    );
}
