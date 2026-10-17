// SPDX-License-Identifier: MIT

use std::{
    ffi::OsStr,
    os::fd::AsFd,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc,
    thread,
    time::Duration,
};

use rustix::fs::{Mode, OFlags};

use super::{Entry, FsOps, OffMain, System};
use crate::util::ui_thread::{self, Exception};

struct ReportsDrop(mpsc::Sender<thread::ThreadId>);

impl Drop for ReportsDrop {
    fn drop(&mut self) {
        let _ = self.0.send(thread::current().id());
    }
}

#[test]
fn what_the_ui_thread_drops_outside_dispatch_is_dropped_on_a_worker() {
    thread::spawn(|| {
        crate::util::ui_thread::mark();
        let (sender, dropped) = mpsc::channel();
        drop(OffMain::new(ReportsDrop(sender)));
        let dropped_on = dropped
            .recv_timeout(Duration::from_secs(10))
            .expect("dropped");
        assert_ne!(dropped_on, thread::current().id());
    })
    .join()
    .expect("marked thread");
}

fn refused(call: impl FnOnce()) -> bool {
    catch_unwind(AssertUnwindSafe(call)).is_err()
}

#[test]
fn path_walks_through_the_seam_are_refused_on_the_ui_thread() {
    let ran = thread::spawn(|| {
        ui_thread::mark();
        let folder = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let root = {
            let _setup = ui_thread::allow(Exception::TestSetup);
            System
                .open_path(OsStr::new("/"), folder, Mode::empty())
                .expect("root")
        };
        [
            (
                "open_path",
                refused(|| drop(System.open_path(OsStr::new("/"), folder, Mode::empty()))),
            ),
            (
                "open_at",
                refused(|| {
                    drop(System.open_at(
                        Entry::new(&root, OsStr::new(".")),
                        folder,
                        Mode::empty(),
                        None,
                    ));
                }),
            ),
            ("stat_vfs", refused(|| drop(System.stat_vfs(root.as_fd())))),
        ]
        .into_iter()
        .filter(|(_, refused)| !refused)
        .map(|(call, _)| call)
        .collect::<Vec<_>>()
    })
    .join()
    .expect("marked thread");
    assert_eq!(ran, Vec::<&str>::new());
}
