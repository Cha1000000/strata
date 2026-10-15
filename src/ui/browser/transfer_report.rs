// SPDX-License-Identifier: MIT

use std::time::Duration;

use crate::model::Location;
use crate::services::{
    KeptCause, KeptItem, RenameCause, RenamedItem, SkippedItem, SpecialKind, TransferReport,
};
use crate::ui::browser::ViewState;

#[cfg(test)]
mod tests;

const NOTICE_DURATION: Duration = Duration::from_secs(6);
const LISTED_ITEMS: usize = 8;
/// The longest line a notice shows whole, in characters; more items, or a
/// longer line, go to a dialog.
const NOTICE_CHARS: usize = 120;

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
        RenameCause::Invalid => "The destination doesn\u{2019}t allow these names",
        RenameCause::TooLong => "These names were too long for the destination",
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

fn kept_line(item: &KeptItem, in_trash: bool) -> String {
    let name = display_name(&item.location);
    let folder = || {
        item.location
            .parent()
            .map(|folder| display_name(&folder))
            .unwrap_or_default()
    };
    match &item.cause {
        KeptCause::Changed if in_trash => rust_i18n::t!(
            "“%{name}” changed during the restore and was left in the Trash",
            name = name
        )
        .into_owned(),
        KeptCause::Changed => rust_i18n::t!(
            "“%{name}” changed during the move and was left in “%{folder}”",
            name = name,
            folder = folder()
        )
        .into_owned(),
        KeptCause::ReplacementChanged => rust_i18n::t!(
            "“%{name}” was left in the Trash, as what replaced it may have changed since",
            name = name
        )
        .into_owned(),
        KeptCause::CopyGone => rust_i18n::t!(
            "“%{name}” was left in “%{folder}”, as its copy was no longer where the move put it",
            name = name,
            folder = folder()
        )
        .into_owned(),
        KeptCause::Undeletable(error) => rust_i18n::t!(
            "Could not delete %{name}: %{error}",
            name = name,
            error = crate::services::io_error_detail(&std::io::Error::from_raw_os_error(*error))
        )
        .into_owned(),
        KeptCause::Skipped(kind) => special_line(&name, *kind),
        KeptCause::NameNotRestored(error) => rust_i18n::t!(
            "Could not restore %{name}: %{error}",
            name = name,
            error = error
        )
        .into_owned(),
    }
}

/// A section's first lines, the only ones formatted, and how many it has.
struct Lines {
    listed: Vec<String>,
    count: usize,
}

impl Lines {
    fn new() -> Self {
        Self {
            listed: Vec::new(),
            count: 0,
        }
    }

    fn of<T>(items: impl IntoIterator<Item = T>, line: impl Fn(T) -> String) -> Self {
        let mut lines = Self::new();
        for item in items {
            lines.push(|| line(item));
        }
        lines
    }

    fn push(&mut self, line: impl FnOnce() -> String) {
        if self.count < LISTED_ITEMS {
            self.listed.push(line());
        }
        self.count += 1;
    }
}

fn bullets(lines: &Lines) -> String {
    let mut text = lines
        .listed
        .iter()
        .map(|line| format!("• {line}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    if lines.count > LISTED_ITEMS {
        text.push_str("\n\n");
        text.push_str(&crate::i18n::count(
            "more_items",
            lines.count - LISTED_ITEMS,
        ));
    }
    text
}

/// The renamed items by cause, in the order each cause first appears.
fn renamed_sections(renamed: &[RenamedItem]) -> Vec<(String, Lines)> {
    let mut sections = Vec::<(RenameCause, Lines)>::new();
    for item in renamed {
        let index = match sections.iter().position(|(cause, _)| *cause == item.cause) {
            Some(index) => index,
            None => {
                sections.push((item.cause, Lines::new()));
                sections.len() - 1
            }
        };
        sections[index].1.push(|| renamed_line(item));
    }
    sections
        .into_iter()
        .map(|(cause, lines)| (heading(cause), lines))
        .collect()
}

fn sections(sections: Vec<(String, Lines)>) -> String {
    sections
        .into_iter()
        .map(|(heading, lines)| format!("{heading}\n\n{}", bullets(&lines)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn left_sections(report: &TransferReport) -> Vec<(String, Lines)> {
    let mut left = Vec::new();
    if !report.skipped.is_empty() {
        let heading =
            crate::i18n::tr("These special files can\u{2019}t be copied and were left out");
        left.push((heading, Lines::of(&report.skipped, skipped_line)));
    }
    for (kept, heading, in_trash) in [
        (&report.kept, "items_kept", false),
        (&report.kept_in_trash, "items_kept_in_trash", true),
    ] {
        if !kept.is_empty() {
            let lines = Lines::of(kept, |item| kept_line(item, in_trash));
            left.push((crate::i18n::count(heading, lines.count), lines));
        }
    }
    left
}

/// The detail of the dialog of an operation that failed, was cancelled or
/// completed with errors: its `outcome`, then everything it reported.
pub(super) fn outcome_detail(outcome: String, report: &TransferReport) -> String {
    let mut reported = renamed_sections(&report.renamed);
    reported.extend(left_sections(report));
    if reported.is_empty() {
        return outcome;
    }
    format!("{outcome}\n\n{}", sections(reported))
}

fn presentation(report: &TransferReport) -> Option<Presentation> {
    let mut renamed = renamed_sections(&report.renamed);
    let left = left_sections(report);
    if !left.is_empty() {
        renamed.extend(left);
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
    if let [line] = lines.listed.as_slice()
        && line.chars().count() <= NOTICE_CHARS
    {
        return Some(Presentation::Notice {
            heading,
            text: line.clone(),
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
            Some(Presentation::Notice { heading, text }) => {
                crate::ui::notice::add_notice(&self.overlay, Some(&heading), &text, NOTICE_DURATION)
            }
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
