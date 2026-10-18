// SPDX-License-Identifier: MIT

use std::{
    cell::{Cell, RefCell},
    fs,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use super::*;
use crate::services::integration_catalog::parse;

const MANIFEST: &str = r#"
schema_version = 1
id = "example"
name = "Example Cloud"
description = "Share links for Example Cloud."
category = "cloud"
maintainer = "octocat"
runtime = "sh"
entry = "provider.sh"

[[requirements]]
command = "example-daemon"
"#;

fn example(script: &[u8]) -> Entry {
    let files = [
        ("catalog.toml", MANIFEST.as_bytes().to_vec()),
        ("provider.sh", script.to_vec()),
    ]
    .into_iter()
    .map(|(name, bytes)| (name.to_owned(), bytes))
    .collect();
    parse("example", files).expect("valid entry")
}

fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut widgets = vec![widget.clone()];
    let mut child = widget.first_child();
    while let Some(current) = child {
        child = current.next_sibling();
        widgets.extend(descendants(&current));
    }
    widgets
}

fn named(page: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
    descendants(page)
        .into_iter()
        .find(|widget| widget.widget_name() == name && widget.is_visible())
}

fn status_text(page: &gtk::Widget) -> Option<String> {
    descendants(page)
        .into_iter()
        .filter(|widget| widget.has_css_class("integration-status"))
        .filter_map(|widget| widget.downcast::<gtk::Label>().ok())
        .map(|label| label.text().to_string())
        .next()
}

fn click(page: &gtk::Widget, name: &str) {
    let button: gtk::Button = named(page, name)
        .and_downcast()
        .unwrap_or_else(|| panic!("button {name}"));
    assert!(button.is_sensitive(), "{name} is sensitive");
    button.emit_clicked();
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn remap(window: &gtk::Window, page: &gtk::Widget) {
    page.set_visible(false);
    wait_until("page unmapped", || !page.is_mapped());
    page.set_visible(true);
    wait_until("page mapped", || page.is_mapped() && window.is_mapped());
}

#[test]
fn the_page_installs_updates_and_removes_catalog_entries() {
    crate::test_support::gtk_test(
        "ui::settings::integrations::tests::the_page_installs_updates_and_removes_catalog_entries",
        || {
            let config = tempfile::tempdir().expect("config");
            let root = config.path().join("strata/providers");
            let catalog = Rc::new(RefCell::new(vec![example(b"#!/bin/sh\ncat\n")]));
            let reloads = Rc::new(Cell::new(0));
            let checks = Arc::new(AtomicUsize::new(0));
            let page = integrations_page_with(Source {
                catalog: Box::new({
                    let catalog = catalog.clone();
                    move || catalog.borrow().clone()
                }),
                root: Some(root.clone()),
                check: Arc::new({
                    let checks = checks.clone();
                    move |entry| {
                        checks.fetch_add(1, Ordering::SeqCst);
                        entry
                            .requirements()
                            .into_iter()
                            .map(|requirement| RequirementResult {
                                satisfied: requirement.command.as_deref() == Some("sh"),
                                requirement,
                            })
                            .collect()
                    }
                }),
                open_folder: None,
                reload: Rc::new({
                    let reloads = reloads.clone();
                    move || reloads.set(reloads.get() + 1)
                }),
            });
            let window = gtk::Window::builder()
                .child(&page)
                .default_width(720)
                .default_height(600)
                .build();
            window.present();
            wait_until("catalog row", || {
                named(&page, "integration-example").is_some()
            });
            assert_eq!(status_text(&page).as_deref(), Some("Not installed"));
            wait_until("requirement results", || {
                descendants(&page)
                    .iter()
                    .any(|widget| widget.has_css_class("integration-requirement"))
            });
            let chips: Vec<(bool, bool)> = descendants(&page)
                .iter()
                .filter(|widget| widget.has_css_class("integration-requirement"))
                .map(|chip| {
                    (
                        chip.has_css_class("available"),
                        chip.has_css_class("missing"),
                    )
                })
                .collect();
            assert_eq!(
                chips,
                [(true, false), (false, true)],
                "the runtime passes and the daemon is missing"
            );
            assert_eq!(reloads.get(), 0, "no reload before a change");
            remap(&window, &page);
            wait_until("rescanned", || {
                named(&page, "integration-example").is_some()
            });
            assert_eq!(
                checks.load(Ordering::SeqCst),
                1,
                "revisiting the page reuses requirement results"
            );

            click(&page, "integration-example-install");
            wait_until("installed", || {
                status_text(&page).as_deref() == Some("Installed")
            });
            assert!(root.join("example/provider.json").exists());
            assert_eq!(reloads.get(), 1, "a change reloads the running providers");
            assert!(named(&page, "integration-example-install").is_none());
            assert!(named(&page, "integration-example-update").is_none());

            let switch = |page: &gtk::Widget| {
                named(page, "integration-example-enabled")
                    .and_downcast::<gtk::Switch>()
                    .expect("enable switch")
            };
            assert!(switch(&page).is_active());
            switch(&page).set_active(false);
            wait_until("disabled", || {
                status_text(&page).as_deref() == Some("Disabled")
            });
            assert!(root.join("example/disabled").exists());
            assert_eq!(reloads.get(), 2, "disabling reloads the running providers");
            assert!(!switch(&page).is_active());
            switch(&page).set_active(true);
            wait_until("enabled again", || {
                status_text(&page).as_deref() == Some("Installed")
            });
            assert!(!root.join("example/disabled").exists());

            catalog.replace(vec![example(b"#!/bin/sh\nexec cat\n")]);
            remap(&window, &page);
            wait_until("update offered", || {
                status_text(&page).as_deref() == Some("Update available")
            });
            fs::write(root.join("example/provider.sh"), b"edited").expect("edit");
            remap(&window, &page);
            wait_until("local edit detected", || {
                status_text(&page).as_deref() == Some("Update available · Modified locally")
            });
            click(&page, "integration-example-update");
            wait_until("confirmation", || {
                named(&page, "integration-example-confirm").is_some()
            });
            assert_eq!(
                fs::read(root.join("example/provider.sh")).expect("script"),
                b"edited",
                "a local edit is confirmed before it is replaced"
            );
            click(&page, "integration-example-confirm");
            wait_until("updated", || {
                status_text(&page).as_deref() == Some("Installed")
            });
            assert_eq!(
                fs::read(root.join("example/provider.sh")).expect("script"),
                b"#!/bin/sh\nexec cat\n"
            );

            click(&page, "integration-example-remove");
            wait_until("removed", || {
                status_text(&page).as_deref() == Some("Not installed")
            });
            assert!(!root.join("example").exists());

            fs::create_dir_all(root.join("handmade")).expect("manual provider");
            fs::write(
                root.join("handmade/provider.json"),
                br#"{"version":1,"id":"handmade","name":"Handmade","command":["/bin/sh"]}"#,
            )
            .expect("manifest");
            let before = reloads.get();
            click(&page, "integrations-refresh");
            wait_until("manual row", || {
                named(&page, "integration-handmade").is_some()
            });
            assert_eq!(reloads.get(), before + 1, "Refresh reloads and rescans");
            wait_until("requirements checked again", || {
                checks.load(Ordering::SeqCst) == 2
            });
            assert!(
                descendants(&page)
                    .iter()
                    .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                    .any(|label| label.text() == "Handmade"),
                "manual installations show their registered name"
            );
            click(&page, "integration-handmade-remove");
            wait_until("confirmation", || {
                named(&page, "integration-handmade-confirm").is_some()
            });
            assert!(
                root.join("handmade").exists(),
                "nothing is deleted before confirming"
            );
            click(&page, "integration-handmade-confirm");
            wait_until("manual row removed", || {
                named(&page, "integration-handmade").is_none()
            });
            assert!(!root.join("handmade").exists());
            window.destroy();
        },
    );
}

#[test]
fn an_empty_catalog_explains_itself() {
    crate::test_support::gtk_test(
        "ui::settings::integrations::tests::an_empty_catalog_explains_itself",
        || {
            let config = tempfile::tempdir().expect("config");
            let page = integrations_page_with(Source {
                catalog: Box::new(Vec::new),
                root: Some(config.path().join("providers")),
                check: Arc::new(|_| Vec::new()),
                open_folder: None,
                reload: Rc::new(|| {}),
            });
            let window = gtk::Window::builder()
                .child(&page)
                .default_width(720)
                .default_height(600)
                .build();
            window.present();
            let message = crate::i18n::tr("No integrations are bundled with this build.");
            wait_until("empty state", || {
                descendants(&page)
                    .iter()
                    .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                    .any(|label| label.text() == message)
            });
            assert!(named(&page, "integrations-refresh").is_some());
            window.destroy();
        },
    );
}
