// SPDX-License-Identifier: MIT

use super::*;
use std::time::{Duration, Instant};

fn has_label(root: &gtk::Widget, text: &str) -> bool {
    root.downcast_ref::<gtk::Label>()
        .is_some_and(|label| label.text().contains(text))
        || std::iter::successors(root.first_child(), |child| child.next_sibling())
            .any(|child| has_label(&child, text))
}

#[test]
fn launches_report_a_missing_terminal_after_the_ui_thread_returns() {
    crate::test_support::gtk_test_with_env(
        "ui::browser::desktop::tests::launches_report_a_missing_terminal_after_the_ui_thread_returns",
        [("PATH", "/nonexistent/strata-test-path"), ("TERMINAL", "")],
        || {
            let open_terminal: fn(&gtk::Box) =
                |parent| launch_terminal(&Location::local("/tmp"), parent);
            let run_script: fn(&gtk::Box) =
                |parent| run_program(&Location::local("/tmp/tool.sh"), parent);
            for (title, launch) in [
                ("Unable to open terminal", open_terminal),
                ("Unable to run program", run_script),
            ] {
                let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
                let overlay = gtk::Overlay::new();
                overlay.set_child(Some(&content));
                let window = gtk::Window::builder().child(&overlay).build();

                launch(&content);
                assert!(
                    !has_label(overlay.upcast_ref(), title),
                    "{title} must wait for the launch thread"
                );
                let deadline = Instant::now() + Duration::from_secs(5);
                while !has_label(overlay.upcast_ref(), title) {
                    assert!(Instant::now() < deadline, "timed out waiting for {title}");
                    glib::MainContext::default().iteration(false);
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(has_label(
                    overlay.upcast_ref(),
                    "No terminal emulator was found"
                ));
                window.destroy();
            }
        },
    );
}
