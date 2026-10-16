// SPDX-License-Identifier: MIT

use super::*;
use std::time::{Duration, Instant};
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn reap_in_background_collects_an_exited_child() -> std::io::Result<()> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn()?;
    let pid = child.id();

    reap_in_background(child, command.get_program().to_owned());

    let parent = format!("PPid:\t{}\n", std::process::id());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return Ok(());
        };
        if !status.contains(&parent) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let state = status
                .lines()
                .find(|line| line.starts_with("State:"))
                .unwrap_or("State: unknown");
            panic!("child {pid} was not reaped; last {state}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn omarchy_presentation_command_runs_the_launcher_only_when_installed() {
    let directory = tempfile::tempdir().expect("PATH fixture");
    let path = Some(directory.path().as_os_str());
    assert!(omarchy_presentation_command(path, "omarchy-update").is_none());

    let launcher = directory.path().join(OMARCHY_PRESENTATION_LAUNCHER);
    fs::write(&launcher, "#!/bin/sh\n").expect("launcher");
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).expect("executable");

    let command = omarchy_presentation_command(path, "omarchy-update").expect("installed launcher");
    assert_eq!(command.get_program(), launcher.as_os_str());
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [OsStr::new("omarchy-update")]
    );
}
