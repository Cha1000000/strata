// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use gtk::prelude::*;

use super::super::{browser_for_window, build_sidebar, pin_locations_at};
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

#[test]
fn pin_locations_at_inserts_at_specified_indices() {
    let mut places = vec![
        (Location::local("/pinned/a"), "A".to_owned()),
        (Location::local("/pinned/b"), "B".to_owned()),
    ];

    let changed = pin_locations_at(&mut places, &[Location::local("/pinned/first")], 0);
    assert!(changed);
    assert_eq!(
        places,
        vec![
            (Location::local("/pinned/first"), "first".to_owned()),
            (Location::local("/pinned/a"), "A".to_owned()),
            (Location::local("/pinned/b"), "B".to_owned()),
        ]
    );

    let changed = pin_locations_at(&mut places, &[Location::local("/pinned/middle")], 2);
    assert!(changed);
    assert_eq!(
        places,
        vec![
            (Location::local("/pinned/first"), "first".to_owned()),
            (Location::local("/pinned/a"), "A".to_owned()),
            (Location::local("/pinned/middle"), "middle".to_owned()),
            (Location::local("/pinned/b"), "B".to_owned()),
        ]
    );

    let changed = pin_locations_at(&mut places, &[Location::local("/pinned/last")], 99);
    assert!(changed);
    assert_eq!(
        places,
        vec![
            (Location::local("/pinned/first"), "first".to_owned()),
            (Location::local("/pinned/a"), "A".to_owned()),
            (Location::local("/pinned/middle"), "middle".to_owned()),
            (Location::local("/pinned/b"), "B".to_owned()),
            (Location::local("/pinned/last"), "last".to_owned()),
        ]
    );
}

#[test]
fn pin_locations_at_already_pinned_folder_is_a_noop() {
    let mut places = vec![
        (Location::local("/pinned/a"), "A".to_owned()),
        (Location::local("/pinned/b"), "B".to_owned()),
        (Location::local("/pinned/c"), "C".to_owned()),
    ];
    let original = places.clone();

    let changed = pin_locations_at(&mut places, &[Location::local("/pinned/b")], 0);
    assert!(!changed);
    assert_eq!(places, original);
}

#[test]
fn pin_locations_at_multi_folder_drag_preserves_selection_order_and_skips_duplicates() {
    let mut places = vec![
        (Location::local("/pinned/a"), "A".to_owned()),
        (Location::local("/pinned/b"), "B".to_owned()),
    ];

    let changed = pin_locations_at(
        &mut places,
        &[
            Location::local("/pinned/new_1"),
            Location::local("/pinned/a"),
            Location::local("/pinned/new_1"),
            Location::local("/pinned/new_2"),
        ],
        1,
    );
    assert!(changed);
    assert_eq!(
        places,
        vec![
            (Location::local("/pinned/a"), "A".to_owned()),
            (Location::local("/pinned/new_1"), "new_1".to_owned()),
            (Location::local("/pinned/new_2"), "new_2".to_owned()),
            (Location::local("/pinned/b"), "B".to_owned()),
        ]
    );
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
            let locations = [
                Location::local("/mount/folder"),
                Location::local("/aliases/folder"),
                initial[0].0.clone(),
                Location::local("/mount/folder"),
            ];
            assert!(
                sidebar
                    .state
                    .pin_locations_before(&locations, Some(&initial[0].0))
            );
            external.splice(
                1..1,
                [
                    (locations[0].clone(), "folder".to_owned()),
                    (locations[1].clone(), "folder".to_owned()),
                ],
            );
            assert_eq!(load_pinned_places().expect("saved pins"), external);
            assert_eq!(*sidebar.state.pinned_places.borrow(), external);
            assert!(sidebar.state.pin_locations_before(&locations, None));
            assert_eq!(load_pinned_places().expect("unchanged pins"), external);
            let later = (Location::local("/external/later"), "Later".to_owned());
            external.insert(0, later);
            save_pinned_places(&external).expect("another external edit");
            assert!(
                sidebar
                    .state
                    .reorder_pinned_before(&locations[0], Some(&initial[1].0))
            );
            let moved = external.remove(2);
            external.insert(4, moved);
            assert_eq!(load_pinned_places().expect("reordered pins"), external);
            external.retain(|(location, _)| location != &locations[0]);
            save_pinned_places(&external).expect("external unpin");
            assert!(sidebar.state.reorder_pinned_before(&locations[0], None));
            assert_eq!(load_pinned_places().expect("no resurrected pin"), external);
            view.browser().clear_observer();
            sidebar.disconnect();
        },
    );
}

#[test]
fn pin_locations_at_ignores_standard_places() {
    let mut places = vec![(Location::local("/pinned/a"), "A".to_owned())];
    let home = Location::local(gtk::glib::home_dir());

    let changed = pin_locations_at(&mut places, &[home], 0);
    assert!(!changed);
    assert_eq!(places, vec![(Location::local("/pinned/a"), "A".to_owned())]);
}
