// SPDX-License-Identifier: MIT

use super::*;

fn item_name(item: &glib::Object) -> String {
    item.downcast_ref::<gtk::StringObject>()
        .map(|item| item.string().into())
        .unwrap_or_default()
}

#[test]
fn view_positions_map_to_the_source_items_they_show() {
    crate::test_support::gtk_test(
        "ui::browser_modes::tests::view_positions_map_to_the_source_items_they_show",
        || {
            let source = gtk::StringList::new(&["alpha", ".hidden", "bravo", "charlie"]);
            let source_index = SourceIndexMap::watch(&source);
            let show_hidden = Rc::new(Cell::new(false));
            let filter = gtk::CustomFilter::new({
                let show_hidden = show_hidden.clone();
                move |item| show_hidden.get() || !item_name(item).starts_with('.')
            });
            let filtered = gtk::FilterListModel::new(Some(source.clone()), Some(filter.clone()));
            let descending =
                gtk::CustomSorter::new(|left, right| item_name(right).cmp(&item_name(left)).into());
            let view = gtk::SortListModel::new(Some(filtered), Some(descending))
                .upcast::<gio::ListModel>();
            let view_index = SourceIndexMap::watch(&view);
            let shown = |positions: std::ops::Range<u32>| -> Vec<String> {
                view_index
                    .sources_of(&source_index, &view, positions)
                    .into_iter()
                    .map(|position| {
                        let position = position.expect("every shown item has a source");
                        source.string(position as u32).unwrap_or_default().into()
                    })
                    .collect()
            };

            assert_eq!(shown(0..3), ["charlie", "bravo", "alpha"]);
            // A filtered-out arrival moves source positions without touching the view.
            source.splice(0, 0, &[".cache"]);
            assert_eq!(shown(0..3), ["charlie", "bravo", "alpha"]);
            source.append("delta");
            assert_eq!(shown(0..4), ["delta", "charlie", "bravo", "alpha"]);
            show_hidden.set(true);
            filter.changed(gtk::FilterChange::LessStrict);
            assert_eq!(
                shown(0..6),
                ["delta", "charlie", "bravo", "alpha", ".hidden", ".cache"]
            );
        },
    );
}

#[test]
fn scroll_refresh_waits_for_scrolling_to_stop() {
    crate::test_support::gtk_test(
        "ui::browser_modes::tests::scroll_refresh_waits_for_scrolling_to_stop",
        || {
            let context = glib::MainContext::default();
            let scroll = gtk::ScrolledWindow::new();
            let adjustment = gtk::Adjustment::new(0.0, 0.0, 10_000.0, 1.0, 10.0, 100.0);
            scroll.set_vadjustment(Some(&adjustment));
            let scrolling = Rc::new(Cell::new(false));
            let last_scroll = Rc::new(Cell::new(std::time::Instant::now()));
            let refreshes = Rc::new(RefCell::new(Vec::new()));
            install_scroll_refresh(&scroll, scrolling.clone(), {
                let last_scroll = last_scroll.clone();
                let refreshes = refreshes.clone();
                move || refreshes.borrow_mut().push(last_scroll.get().elapsed())
            });

            for step in 1..=20 {
                last_scroll.set(std::time::Instant::now());
                adjustment.set_value(f64::from(step) * 10.0);
                assert!(scrolling.get());
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while scrolling.get() {
                assert!(std::time::Instant::now() < deadline, "scroll never settled");
                context.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }

            assert!(!refreshes.borrow().is_empty());
            assert!(
                refreshes
                    .borrow()
                    .iter()
                    .all(|still| *still >= ScrollSettle::DELAY),
                "refreshed before scrolling was still: {:?}",
                refreshes.borrow()
            );
        },
    );
}
