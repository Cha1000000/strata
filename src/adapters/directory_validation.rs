// SPDX-License-Identifier: MIT

#[cfg(test)]
mod tests;

use gtk::{gio, glib, prelude::*};

use crate::model::Location;

/// Queries metadata only, follows directory symlinks, and never probes on the GTK thread.
/// One query at a time bounds speculative work even for very large foreign selections.
pub(crate) async fn all_directories(locations: &[Location]) -> bool {
    if locations.is_empty() {
        return false;
    }
    for location in locations {
        let result = super::gio_file_for_location(location)
            .query_info_future(
                gio::FILE_ATTRIBUTE_STANDARD_TYPE,
                gio::FileQueryInfoFlags::NONE,
                glib::Priority::DEFAULT,
            )
            .await;
        if !result.is_ok_and(|info| info.file_type() == gio::FileType::Directory) {
            return false;
        }
    }
    true
}
