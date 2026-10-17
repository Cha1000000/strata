// SPDX-License-Identifier: MIT

use std::{
    collections::VecDeque,
    panic::AssertUnwindSafe,
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use futures_channel::oneshot;

use crate::util::ui_thread;

const EXIT_FLUSH_WAIT: Duration = Duration::from_secs(10);

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    running: bool,
    submitted: u64,
}

pub(crate) struct Writer {
    queue: Mutex<Queue>,
    wake: Condvar,
    idle: Condvar,
}

impl Writer {
    pub(crate) fn start(name: &str) -> Arc<Self> {
        let writer = Arc::new(Self {
            queue: Mutex::default(),
            wake: Condvar::new(),
            idle: Condvar::new(),
        });
        let worker = writer.clone();
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || worker.work())
            .expect("settings writer thread");
        writer
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn work(&self) {
        let mut queue = self.lock();
        loop {
            match queue.jobs.pop_front() {
                Some(job) => {
                    queue.running = true;
                    drop(queue);
                    if std::panic::catch_unwind(AssertUnwindSafe(job)).is_err() {
                        tracing::error!("a settings file job panicked");
                    }
                    queue = self.lock();
                    queue.running = false;
                }
                None => {
                    self.idle.notify_all();
                    queue = self
                        .wake
                        .wait(queue)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }

    pub(crate) fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> oneshot::Receiver<T> {
        let (send, receive) = oneshot::channel();
        let mut queue = self.lock();
        queue.jobs.push_back(Box::new(move || {
            let _ = send.send(job());
        }));
        queue.submitted += 1;
        drop(queue);
        self.wake.notify_one();
        receive
    }

    pub(crate) fn flush(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut queue = self.lock();
        while queue.running || !queue.jobs.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            queue = self
                .idle
                .wait_timeout(queue, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    #[cfg(test)]
    fn submitted(&self) -> u64 {
        self.lock().submitted
    }

    #[cfg(test)]
    pub(crate) fn hold(&self) -> Held {
        let (started, wait_started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        drop(self.run(move || {
            let _ = started.send(());
            let _ = released.recv();
        }));
        wait_started.recv().expect("held writer");
        Held(release)
    }
}

#[cfg(test)]
pub(crate) struct Held(
    #[expect(dead_code, reason = "dropping it releases the writer")] std::sync::mpsc::Sender<()>,
);

fn shared() -> &'static Writer {
    static SHARED: OnceLock<Arc<Writer>> = OnceLock::new();
    SHARED.get_or_init(|| Writer::start("strata-settings"))
}

pub(crate) fn run<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
) -> oneshot::Receiver<T> {
    shared().run(job)
}

pub(crate) fn flush(wait: Duration) -> bool {
    ui_thread::assert_off();
    shared().flush(wait)
}

pub(crate) fn flush_at_exit() {
    let _exit = ui_thread::allow(ui_thread::Exception::ExitFlush);
    if !flush(EXIT_FLUSH_WAIT) {
        tracing::warn!("quitting before Strata's own files finished saving");
    }
}

#[cfg(test)]
pub(crate) fn hold() -> Held {
    shared().hold()
}

#[cfg(test)]
pub(crate) fn settle() {
    let context = gtk::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let submitted = shared().submitted();
        {
            let _allowed = ui_thread::allow(ui_thread::Exception::TestSetup);
            assert!(
                flush(deadline.saturating_duration_since(Instant::now())),
                "the settings writer did not finish"
            );
        }
        for _ in 0..100 {
            if !context.iteration(false) {
                break;
            }
        }
        if shared().submitted() == submitted && shared().flush(Duration::ZERO) {
            return;
        }
        assert!(Instant::now() < deadline, "the settings writer kept busy");
    }
}

#[cfg(test)]
mod tests;
