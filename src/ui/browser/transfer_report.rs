// SPDX-License-Identifier: MIT

//! Tells the user, once a transfer ends, about items inside the ones it was
//! given that it saved under other names or left out.

use std::time::Duration;

use crate::model::Location;
use crate::services::{RenameCause, RenamedItem, SkippedItem, SpecialKind, TransferReport};
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
    /// Items were left out, so the transfer completed with errors.
    Warning {
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
        RenameCause::Taken => "These names were already taken",
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

fn skipped_line(item: &SkippedItem) -> String {
    let name = display_name(&item.location);
    match item.kind {
        SpecialKind::Pipe => rust_i18n::t!("“%{name}” (named pipe)", name = name),
        SpecialKind::Socket => rust_i18n::t!("“%{name}” (socket)", name = name),
        SpecialKind::Device => rust_i18n::t!("“%{name}” (device file)", name = name),
    }
    .into_owned()
}

/// The first [`LISTED_ITEMS`] lines as bullets, then how many more there are.
fn bullets(lines: &[String]) -> String {
    let mut text = lines
        .iter()
        .take(LISTED_ITEMS)
        .map(|line| format!("• {line}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    if lines.len() > LISTED_ITEMS {
        text.push_str("\n\n");
        text.push_str(&crate::i18n::count(
            "more_items",
            lines.len() - LISTED_ITEMS,
        ));
    }
    text
}

/// The renamed items' lines under the heading of their cause, in the
/// order the causes first appear.
fn renamed_sections(renamed: &[RenamedItem]) -> Vec<(String, Vec<String>)> {
    let mut sections = Vec::<(RenameCause, Vec<String>)>::new();
    for item in renamed {
        match sections.iter_mut().find(|(cause, _)| *cause == item.cause) {
            Some((_, lines)) => lines.push(renamed_line(item)),
            None => sections.push((item.cause, vec![renamed_line(item)])),
        }
    }
    sections
        .into_iter()
        .map(|(cause, lines)| (heading(cause), lines))
        .collect()
}

fn sections(sections: Vec<(String, Vec<String>)>) -> String {
    sections
        .into_iter()
        .map(|(heading, lines)| format!("{heading}\n\n{}", bullets(&lines)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn presentation(report: &TransferReport) -> Option<Presentation> {
    let mut renamed = renamed_sections(&report.renamed);
    if !report.skipped.is_empty() {
        let skipped = report.skipped.iter().map(skipped_line).collect::<Vec<_>>();
        let heading =
            crate::i18n::tr("These special files can\u{2019}t be copied and were left out");
        renamed.push((heading, skipped));
        return Some(Presentation::Warning {
            detail: sections(renamed),
        });
    }
    let message = crate::i18n::count("items_renamed", report.renamed.len());
    if renamed.len() > 1 {
        return Some(Presentation::Information {
            message,
            summary: crate::i18n::tr("Some names changed during the transfer"),
            detail: sections(renamed),
        });
    }
    let (heading, lines) = renamed.pop()?;
    if lines.len() <= NOTICE_ITEMS {
        return Some(Presentation::Notice {
            heading,
            text: lines.join("\n"),
        });
    }
    Some(Presentation::Information {
        message,
        summary: heading,
        detail: bullets(&lines),
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
            Some(Presentation::Warning { detail }) => {
                crate::ui::modal::show_partial_failure_dialog(&self.overlay, &detail);
            }
            None => {}
        }
    }
}
