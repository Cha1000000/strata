// SPDX-License-Identifier: MIT

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use super::Writer;

#[test]
fn jobs_run_one_at_a_time_in_order_and_survive_a_panicking_job() {
    let writer = Writer::start("strata-settings-test");
    let ran = Arc::new(Mutex::new(Vec::new()));
    let held = writer.hold();
    let answers: Vec<_> = (0..3)
        .map(|index| {
            let ran = ran.clone();
            writer.run(move || {
                assert_ne!(index, 1, "an injected failure");
                ran.lock()
                    .expect("ran")
                    .push((index, std::thread::current().id()));
                index
            })
        })
        .collect();
    assert!(!writer.flush(Duration::from_millis(50)));
    assert!(ran.lock().expect("ran").is_empty());
    drop(held);
    assert!(writer.flush(Duration::from_secs(10)));
    let ran = ran.lock().expect("ran").clone();
    assert_eq!(
        ran.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        [0, 2]
    );
    assert!(
        ran.iter()
            .all(|(_, thread)| *thread != std::thread::current().id())
    );
    let answers: Vec<_> = answers
        .into_iter()
        .map(|mut answer| answer.try_recv().ok().flatten())
        .collect();
    assert_eq!(answers, [Some(0), None, Some(2)]);
}
