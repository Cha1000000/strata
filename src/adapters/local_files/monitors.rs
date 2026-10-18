// SPDX-License-Identifier: MIT

use std::{
    path::PathBuf,
    sync::{Mutex, OnceLock, mpsc},
};

use futures_channel::oneshot;
use gtk::{gio, glib, prelude::*};

use crate::{
    adapters::{
        gio_file_for_location,
        mount_lanes::{self, Class},
    },
    model::Location,
    services::LoadHandle,
};

pub(super) struct BareMonitor {
    monitor: Option<gio::FileMonitor>,
    file: gio::File,
}

#[expect(
    unsafe_code,
    reason = "a monitor without handlers moves between the UI thread and the monitors worker"
)]
// SAFETY: a bare monitor carries no signal handler, so no closure, and no `Rc` one captured, can be
// freed with it on another thread. GObject reference counts and `g_file_monitor_cancel` are
// thread-safe, and the monitor's source stays attached to the main context.
unsafe impl Send for BareMonitor {}

impl BareMonitor {
    pub(super) fn monitor(&self) -> &gio::FileMonitor {
        self.monitor.as_ref().expect("held until dropped")
    }
}

impl Drop for BareMonitor {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            let release = BareMonitor {
                monitor: Some(monitor),
                file: self.file.clone(),
            };
            if let Err(mpsc::SendError(Job::Release(mut unsent))) = send(Job::Release(release)) {
                // Never cancel on the UI thread: a creation stuck on a dead mount holds GLib's
                // inotify lock.
                std::mem::forget(unsent.monitor.take());
            }
        }
    }
}

enum Job {
    Start {
        file: gio::File,
        lane: Lane,
        flags: gio::FileMonitorFlags,
        reply: oneshot::Sender<BareMonitor>,
    },
    Release(BareMonitor),
}

enum Lane {
    Native(PathBuf),
    Remote(String),
}

fn send(job: Job) -> Result<(), mpsc::SendError<Job>> {
    static JOBS: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    let jobs = JOBS.get_or_init(|| {
        let (jobs, received) = mpsc::channel();
        std::thread::Builder::new()
            .name("strata-monitors".to_owned())
            .spawn(move || received.into_iter().for_each(run))
            .expect("spawn the monitors worker");
        Mutex::new(jobs)
    });
    jobs.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .send(job)
}

fn run(job: Job) {
    match job {
        Job::Start {
            file,
            lane,
            flags,
            reply,
        } => {
            if reply.is_canceled() {
                return;
            }
            let admitted = match &lane {
                Lane::Native(path) => mount_lanes::admit(path, Class::Interactive),
                Lane::Remote(authority) => {
                    mount_lanes::admit_remote(authority.clone(), Class::Interactive)
                }
            };
            let Ok(admitted) = admitted else {
                return;
            };
            let created = super::super::local_operations::watch_folder(&file, flags);
            admitted.progress().answered(&created);
            drop(admitted);
            match created {
                Ok(monitor) => {
                    let _ = reply.send(BareMonitor {
                        monitor: Some(monitor),
                        file,
                    });
                }
                Err(error) => tracing::warn!(
                    error_domain = ?error.domain(),
                    error_code = error.code(),
                    "directory monitoring unavailable"
                ),
            }
        }
        Job::Release(mut bare) => {
            if let Some(monitor) = bare.monitor.take() {
                super::super::local_operations::unwatch_folder(&bare.file, monitor);
            }
        }
    }
}

pub(super) fn start(
    location: &Location,
    flags: gio::FileMonitorFlags,
) -> impl std::future::Future<Output = Option<BareMonitor>> + use<> {
    let file = gio_file_for_location(location);
    let lane = match location.native_path() {
        Some(path) => Lane::Native(path.to_path_buf()),
        None => Lane::Remote(mount_lanes::authority(
            location.uri_value().unwrap_or_default(),
        )),
    };
    let (reply, started) = oneshot::channel();
    let sent = send(Job::Start {
        file,
        lane,
        flags,
        reply,
    })
    .is_ok();
    async move {
        if !sent {
            return None;
        }
        started.await.ok()
    }
}

pub(crate) fn watch_folder(location: &Location, changed: impl Fn() + 'static) -> LoadHandle {
    let started = start(location, gio::FileMonitorFlags::NONE);
    let held = std::rc::Rc::new(std::cell::RefCell::new(
        None::<(BareMonitor, glib::SignalHandlerId)>,
    ));
    let slot = held.clone();
    let task = glib::MainContext::default().spawn_local(async move {
        if let Some(bare) = started.await {
            let handler = bare.monitor().connect_changed(move |_, _, _, _| changed());
            slot.replace(Some((bare, handler)));
        }
    });
    LoadHandle::new(move || {
        task.abort();
        if let Some((bare, handler)) = held.take() {
            bare.monitor().disconnect(handler);
        }
    })
}
