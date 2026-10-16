// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use gtk::prelude::*;

use super::super::{
    browser_for_window, build_sidebar, move_pinned_place, pin_gap_index, pin_locations_at,
};
use crate::model::Location;
use crate::test_support::gtk_test_with_env;
use crate::ui::preferences::PreferenceManager;

fn wait_until(condition: impl Fn() -> bool, waiting_for: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {waiting_for}"
        );
        gtk::glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn shows_label(widget: &gtk::Widget, text: &str) -> bool {
    if !widget.is_visible() {
        return false;
    }
    if widget
        .downcast_ref::<gtk::Label>()
        .is_some_and(|label| label.label() == text)
    {
        return true;
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if shows_label(&current, text) {
            return true;
        }
        child = current.next_sibling();
    }
    false
}

#[test]
fn a_rejected_place_click_returns_the_highlight_to_the_open_location() {
    // Without a GVfs backend, network:// is rejected rather than opened.
    gtk_test_with_env(
        "ui::window::tests::sidebar::a_rejected_place_click_returns_the_highlight_to_the_open_location",
        [("GIO_USE_VFS", "local")],
        || {
            PreferenceManager::seed_saved_preferences_for_test();
            let preferences = PreferenceManager::shared();
            preferences.set_sidebar_show_home(true);
            preferences.set_sidebar_show_network(true);
            let view = browser_for_window();
            let sidebar = build_sidebar(view.clone(), preferences, false);
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            content.append(&sidebar.widget);
            content.append(&view.widget());
            let window = gtk::Window::builder()
                .child(&content)
                .default_width(1000)
                .default_height(600)
                .build();
            window.present();
            let row = |location: &Location| {
                sidebar
                    .state
                    .place_rows
                    .borrow()
                    .iter()
                    .find(|(candidate, _)| candidate == location)
                    .map(|(_, row)| row.clone())
                    .expect("sidebar place row")
            };
            let home = Location::local(gtk::glib::home_dir());
            let network = Location::uri("network:///");

            row(&home).emit_clicked();
            wait_until(
                || view.browser().active_location() == Some(home.clone()),
                "Home to open",
            );
            row(&network).emit_clicked();
            wait_until(
                || shows_label(&view.widget(), "Unable to open location"),
                "the Network rejection",
            );

            assert_eq!(view.browser().active_location(), Some(home.clone()));
            assert!(
                row(&home).has_css_class("active"),
                "Home stays open, so its row must keep the highlight"
            );
            assert!(
                !row(&network).has_css_class("active"),
                "the rejected Network row must not stay highlighted"
            );
            view.browser().clear_observer();
            sidebar.disconnect();
            window.destroy();
        },
    );
}

fn pin(path: &str, name: &str) -> (Location, String) {
    (Location::local(path), name.to_owned())
}

#[test]
fn pin_locations_at_inserts_at_specified_indices_with_their_display_names() {
    let mut places = vec![pin("/pinned/a", "A"), pin("/pinned/b", "B")];
    for (folder, index) in [
        (pin("/pinned/first", "First"), 0),
        (pin("/pinned/middle", "Middle"), 2),
        (pin("/pinned/last", "Last"), 99),
    ] {
        assert!(pin_locations_at(&mut places, &[folder], index));
    }
    assert_eq!(
        places,
        vec![
            pin("/pinned/first", "First"),
            pin("/pinned/a", "A"),
            pin("/pinned/middle", "Middle"),
            pin("/pinned/b", "B"),
            pin("/pinned/last", "Last"),
        ]
    );
}

#[test]
fn pin_locations_at_skips_existing_and_standard_places_without_reordering() {
    let original = vec![
        pin("/pinned/a", "A"),
        pin("/pinned/b", "B"),
        pin("/pinned/c", "C"),
    ];
    let home = (Location::local(gtk::glib::home_dir()), "Home".to_owned());
    for sources in [vec![pin("/pinned/b", "Renamed")], vec![home]] {
        let mut places = original.clone();
        assert!(!pin_locations_at(&mut places, &sources, 0));
        assert_eq!(places, original);
    }
}

#[test]
fn pin_locations_at_multi_folder_drag_preserves_selection_order_and_skips_duplicates() {
    let mut places = vec![pin("/pinned/a", "A"), pin("/pinned/b", "B")];

    let changed = pin_locations_at(
        &mut places,
        &[
            pin("/pinned/new_1", "New 1"),
            pin("/pinned/a", "A"),
            pin("/pinned/new_1", "New 1"),
            pin("/pinned/new_2", "New 2"),
        ],
        1,
    );
    assert!(changed);
    assert_eq!(
        places,
        vec![
            pin("/pinned/a", "A"),
            pin("/pinned/new_1", "New 1"),
            pin("/pinned/new_2", "New 2"),
            pin("/pinned/b", "B"),
        ]
    );
}

#[test]
fn a_gap_falls_back_to_its_surviving_neighbour_and_never_jumps_to_the_end() {
    let places = vec![pin("/pinned/a", "A"), pin("/pinned/b", "B")];
    let a = Location::local("/pinned/a");
    let b = Location::local("/pinned/b");
    let gone = Location::local("/pinned/unpinned-elsewhere");
    for (gap, expected) in [
        ((Some(&a), Some(&b)), Some(1)),
        ((Some(&a), Some(&gone)), Some(1)),
        ((Some(&gone), Some(&b)), Some(1)),
        ((None, Some(&gone)), None),
        ((Some(&gone), Some(&gone)), None),
        ((Some(&gone), None), Some(2)),
        ((None, None), Some(2)),
    ] {
        assert_eq!(pin_gap_index(&places, gap), expected, "{gap:?}");
    }
}

#[test]
fn moving_a_pin_into_an_adjacent_gap_is_a_noop() {
    let original = vec![pin("/a", "A"), pin("/b", "B"), pin("/c", "C")];
    for (source, index, expected) in [
        (1, 1, None),
        (1, 2, None),
        (1, 0, Some(["B", "A", "C"])),
        (0, 3, Some(["B", "C", "A"])),
        (2, 1, Some(["A", "C", "B"])),
    ] {
        let mut places = original.clone();
        assert_eq!(
            move_pinned_place(&mut places, source, index),
            expected.is_some()
        );
        let names = places
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            expected.unwrap_or(["A", "B", "C"]),
            "{source} -> {index}"
        );
    }
}

#[test]
fn pinning_merges_latest_bookmarks_and_persists_aliases_in_selection_order() {
    crate::test_support::gtk_test(
        "ui::window::tests::sidebar::pinning_merges_latest_bookmarks_and_persists_aliases_in_selection_order",
        || {
            use super::super::{load_pinned_places, save_pinned_places};
            PreferenceManager::seed_saved_preferences_for_test();
            let initial = vec![
                (Location::uri("sftp://host/anchor"), "Anchor".to_owned()),
                (Location::local("/pinned/last"), "Last".to_owned()),
            ];
            save_pinned_places(&initial).expect("seed bookmarks");
            let view = browser_for_window();
            let sidebar = build_sidebar(view.clone(), PreferenceManager::shared(), false);
            let mut external = initial.clone();
            external.insert(0, (Location::local("/external/pin"), "External".to_owned()));
            save_pinned_places(&external).expect("external edit");
            let folders = [
                (Location::local("/mount/folder"), "Mounted".to_owned()),
                (Location::local("/aliases/folder"), "Alias".to_owned()),
                initial[0].clone(),
                (Location::local("/mount/folder"), "Mounted".to_owned()),
            ];
            let locations = folders.clone().map(|(location, _)| location);
            assert!(
                sidebar
                    .state
                    .pin_folders_into(&folders, (None, Some(&initial[0].0)))
            );
            external.splice(1..1, folders[..2].iter().cloned());
            assert_eq!(load_pinned_places().expect("saved pins"), external);
            assert_eq!(*sidebar.state.pinned_places.borrow(), external);
            assert!(
                !sidebar
                    .state
                    .pin_folders_into(&folders, (Some(&initial[1].0), None))
            );
            assert_eq!(load_pinned_places().expect("unchanged pins"), external);
            let later = (Location::local("/external/later"), "Later".to_owned());
            external.insert(0, later);
            save_pinned_places(&external).expect("another external edit");
            assert!(
                sidebar
                    .state
                    .reorder_pinned_into(&locations[0], (Some(&initial[0].0), Some(&initial[1].0)))
            );
            let moved = external.remove(2);
            external.insert(4, moved);
            assert_eq!(load_pinned_places().expect("reordered pins"), external);
            external.retain(|(location, _)| location != &locations[0]);
            save_pinned_places(&external).expect("external unpin");
            assert!(
                !sidebar
                    .state
                    .reorder_pinned_into(&locations[0], (Some(&initial[1].0), None))
            );
            assert_eq!(load_pinned_places().expect("no resurrected pin"), external);
            view.browser().clear_observer();
            sidebar.disconnect();
        },
    );
}
