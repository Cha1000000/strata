// SPDX-License-Identifier: MIT

use std::time::{Duration, SystemTime};

use super::copy_engine::{now, sleep};

/// How long after `closetimeo` the SMB client may take to close the handle.
const MARGIN: Duration = Duration::from_millis(500);

/// The longest a wait lasts, as `closetimeo` may be set to days.
const LIMIT: Duration = Duration::from_secs(10);

const FIRST_PAUSE: Duration = Duration::from_millis(10);
const LONGEST_PAUSE: Duration = Duration::from_millis(50);

pub(super) enum Pause {
    Retry,
    Spent,
    Cancelled,
}

pub(super) struct Wait {
    started: SystemTime,
    budget: Duration,
    pause: Duration,
}

impl Wait {
    pub(super) fn after(closetimeo: Duration) -> Self {
        Self::since(now(), closing(closetimeo))
    }

    pub(super) fn since(started: SystemTime, budget: Duration) -> Self {
        Self {
            started,
            budget: budget.min(LIMIT),
            pause: FIRST_PAUSE,
        }
    }

    pub(super) fn pause(&mut self, stopped: &dyn Fn() -> bool) -> Pause {
        // A clock set back ends the wait rather than stretch it.
        let Ok(spent) = now().duration_since(self.started) else {
            return Pause::Spent;
        };
        let left = self.budget.saturating_sub(spent);
        if left.is_zero() {
            return Pause::Spent;
        }
        sleep(self.pause.min(left));
        self.pause = (self.pause * 2).min(LONGEST_PAUSE);
        if stopped() {
            Pause::Cancelled
        } else {
            Pause::Retry
        }
    }
}

pub(super) fn closing(closetimeo: Duration) -> Duration {
    closetimeo.saturating_add(MARGIN).min(LIMIT)
}
