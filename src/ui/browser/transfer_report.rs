// SPDX-License-Identifier: MIT

//! Tells the user, once a transfer ends, about items inside the ones it was
//! given that it saved under other names.

use std::time::Duration;

use crate::model::Location;
use crate::services::{RenameCause, RenamedItem, TransferReport};
use crate::ui::browser::ViewState;

#[cfg(test)]
mod tests;

const NOTICE_DURATION: Duration = Duration::from_secs(6);
/// Items a dialog lists before it counts the rest.
const LISTED_ITEMS: usize = 8;
/// Items a notice lists; more go to a dialog.
const NOTICE_ITEMS: usize = 2;

#[derive(Debug, Eq, PartialEq)]
enum Presentation {
    Notice {
        heading: String,
        text: String,
    },
    Information {
        message: String,
        summary: String,
        detail: String,
    },
}

fn display_name(location: &Location) -> String {
    location
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn heading(cause: RenameCause) -> String {
    crate::i18n::tr(match cause {
        RenameCause::Alias => "The destination can\u{2019}t tell these names apart",
    })
}

fn renamed_line(item: &RenamedItem) -> String {
    rust_i18n::t!(
        "“%{name}” was saved as “%{new_name}”",
        name = display_name(&item.requested),
        new_name = display_name(&item.saved_as)
    )
    .into_owned()
}

fn presentation(report: &TransferReport) -> Option<Presentation> {
    let renamed = &report.renamed;
    let heading = heading(renamed.first()?.cause);
    if renamed.len() <= NOTICE_ITEMS {
        let lines = renamed.iter().map(renamed_line).collect::<Vec<_>>();
        return Some(Presentation::Notice {
            heading,
            text: lines.join("\n"),
        });
    }
    let mut detail = renamed
        .iter()
        .take(LISTED_ITEMS)
        .map(|item| format!("• {}", renamed_line(item)))
        .collect::<Vec<_>>()
        .join("\n\n");
    if renamed.len() > LISTED_ITEMS {
        detail.push_str("\n\n");
        detail.push_str(&crate::i18n::count(
            "more_items",
            renamed.len() - LISTED_ITEMS,
        ));
    }
    Some(Presentation::Information {
        message: crate::i18n::count("items_renamed", renamed.len()),
        summary: heading,
        detail,
    })
}

impl ViewState {
    pub(super) fn show_transfer_report(&self, report: &TransferReport) {
        match presentation(report) {
            Some(Presentation::Notice { heading, text }) => crate::ui::notice::show_notice(
                &self.overlay,
                Some(&heading),
                &text,
                NOTICE_DURATION,
            ),
            Some(Presentation::Information {
                message,
                summary,
                detail,
            }) => crate::ui::modal::show_information_dialog_with_summary(
                &self.overlay,
                &message,
                &summary,
                &detail,
            ),
            None => {}
        }
    }
}
