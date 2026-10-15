// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use gtk::prelude::*;

use super::super::{browser_for_window, build_sidebar};
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
