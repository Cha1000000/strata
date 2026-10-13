// SPDX-License-Identifier: MIT

//! Tells the user, once a transfer ends, about items inside the ones it was
//! given that it saved under other names or left out, and what a move left
//! in its original folder.

use std::time::Duration;

use crate::model::Location;
use crate::services::{
    KeptCause, KeptItem, RenameCause, RenamedItem, SkippedItem, SpecialKind, TransferReport,
};
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
    /// Items were left out or left behind.
    Warning {
        title: String,
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

fn special_line(name: &str, kind: SpecialKind) -> String {
    match kind {
        SpecialKind::Pipe => rust_i18n::t!("“%{name}” (named pipe)", name = name),
        SpecialKind::Socket => rust_i18n::t!("“%{name}” (socket)", name = name),
        SpecialKind::Device => rust_i18n::t!("“%{name}” (device file)", name = name),
    }
    .into_owned()
}

fn skipped_line(item: &SkippedItem) -> String {
    special_line(&display_name(&item.location), item.kind)
}

fn kept_line(item: &KeptItem) -> String {
    let name = display_name(&item.location);
    match &item.cause {
        KeptCause::Changed => rust_i18n::t!(
            "“%{name}” changed during the move and was left in “%{folder}”",
            name = name,
            folder = item
                .location
                .parent()
                .map(|folder| display_name(&folder))
                .unwrap_or_default()
        )
        .into_owned(),
        KeptCause::Undeletable(error) => rust_i18n::t!(
            "Could not delete %{name}: %{error}",
            name = name,
            error = error
        )
        .into_owned(),
        KeptCause::Skipped(kind) => special_line(&name, *kind),
    }
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

/// The renamed items, where one was saved again under another name as one:
/// as an alias numbered inside a merge stage meets a taken name when it is
/// placed. Each keeps the cause of the name first asked for.
fn followed(renamed: &[RenamedItem]) -> Vec<RenamedItem> {
    let mut items = Vec::<RenamedItem>::new();
    for item in renamed {
        if let Some(earlier) = items
            .iter_mut()
            .find(|earlier| earlier.saved_as == item.requested)
        {
            earlier.saved_as = item.saved_as.clone();
        } else if let Some(later) = items
            .iter_mut()
            .find(|later| later.requested == item.saved_as)
        {
            later.requested = item.requested.clone();
            later.cause = item.cause;
        } else {
            items.push(item.clone());
        }
    }
    items
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

/// How `report` is shown, once its transfer completed or `stopped`.
fn presentation(report: &TransferReport, stopped: bool) -> Option<Presentation> {
    let followed = followed(&report.renamed);
    let mut renamed = renamed_sections(&followed);
    if !report.skipped.is_empty() || !report.kept.is_empty() {
        if !report.skipped.is_empty() {
            let skipped = report.skipped.iter().map(skipped_line).collect::<Vec<_>>();
            let heading =
                crate::i18n::tr("These special files can\u{2019}t be copied and were left out");
            renamed.push((heading, skipped));
        }
        if !report.kept.is_empty() {
            let kept = report.kept.iter().map(kept_line).collect::<Vec<_>>();
            renamed.push((crate::i18n::count("items_kept", kept.len()), kept));
        }
        // A transfer that failed or was cancelled did not complete.
        let title = if stopped {
            crate::i18n::tr("Some items were left out or left behind")
        } else {
            crate::i18n::tr("Completed with errors")
        };
        return Some(Presentation::Warning {
            title,
            detail: sections(renamed),
        });
    }
    let message = crate::i18n::count("items_renamed", followed.len());
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
    pub(super) fn show_transfer_report(&self, report: &TransferReport, stopped: bool) {
        match presentation(report, stopped) {
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
            Some(Presentation::Warning { title, detail }) => {
                crate::ui::modal::show_warning_dialog(&self.overlay, &title, &detail);
            }
            None => {}
        }
    }
}
