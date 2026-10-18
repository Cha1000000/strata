# SPDX-License-Identifier: MIT
"""Opt-in strace capture of the UI thread's file calls for scripts/trace-main-io.py."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import time
from dataclasses import dataclass
from pathlib import Path

RUN_VARIABLE = "STRATA_E2E_MAIN_IO"
STALL_VARIABLE = "STRATA_UI_STALL_MS"
LONGEST_NAME = 120
TRACED_CALLS = (
    "%file", "getdents64", "close", "fsync", "fdatasync", "syncfs", "flock", "fstatfs", "fstat",
    "lseek", "read", "pread64", "preadv", "write", "pwrite64", "pwritev", "ftruncate",
    "fallocate", "fchmod", "fchown", "fgetxattr", "flistxattr", "fsetxattr", "fremovexattr",
    "copy_file_range", "sendfile",
)
UNSTACKED_CALLS = ("read", "write")


def run_directory() -> Path | None:
    configured = os.environ.get(RUN_VARIABLE)
    if not configured:
        return None
    return Path(__file__).resolve().parents[3] / configured


def _slug(name: str) -> str:
    slug = "".join(
        character if character.isalnum() or character in "-_." else "-" for character in name
    )
    if len(slug) <= LONGEST_NAME:
        return slug
    return f"{slug[:LONGEST_NAME]}-{hashlib.sha256(name.encode()).hexdigest()[:12]}"


def traced_child(tracer: int) -> int:
    for status in Path("/proc").glob("[0-9]*/status"):
        try:
            text = status.read_text()
        except OSError:
            continue
        if f"\nPPid:\t{tracer}\n" in text:
            return int(status.parent.name)
    raise ProcessLookupError(f"strace {tracer} has no traced child")


@dataclass
class Capture:
    directory: Path
    meta: dict[str, object]
    stacks: bool = False

    @property
    def trace(self) -> Path:
        return self.directory / "trace"

    def wrap(self, command: list) -> list:
        stacks = ["-k", "--stack-trace-frame-limit=64"] if self.stacks else []
        calls = [call for call in TRACED_CALLS if not (self.stacks and call in UNSTACKED_CALLS)]
        return [
            shutil.which("strace") or "strace", "-o", str(self.trace), "-qq", "-y", "-ttt",
            "-s", "0", *stacks, "-e", f"trace={','.join(calls)}", "-e", "signal=none", *command,
        ]

    def ready(self) -> None:
        self.meta["ready"] = time.time()
        self._write()

    def keep_log(self, log: Path) -> None:
        if log.exists():
            shutil.copyfile(log, self.directory / "strata.log")

    def _write(self) -> None:
        (self.directory / "meta.json").write_text(json.dumps(self.meta, indent=2) + "\n")


def launch(
    environment, location: Path, command: list, variables: dict[str, str]
) -> tuple[list, Capture | None]:
    limit = os.environ.get(STALL_VARIABLE)
    if limit:
        variables[STALL_VARIABLE] = limit
    traced = capture(environment, location, Path(os.fsdecode(command[0])), variables)
    return (command if traced is None else traced.wrap(command)), traced


def capture(
    environment, location: Path, binary: Path, variables: dict[str, str]
) -> Capture | None:
    root = run_directory()
    if root is None:
        return None
    options_path = root / "options.json"
    options = json.loads(options_path.read_text()) if options_path.exists() else {}
    scenario = os.environ.get("PYTEST_CURRENT_TEST", "unnamed").rsplit(" (", 1)[0]
    scenario_directory = root / _slug(scenario)
    scenario_directory.mkdir(parents=True, exist_ok=True)
    launch = 1
    while (scenario_directory / f"launch-{launch}").exists():
        launch += 1
    directory = scenario_directory / f"launch-{launch}"
    directory.mkdir()
    meta: dict[str, object] = {
        "scenario": scenario,
        "fixture": str(location),
        "environment": str(environment.root),
        "home": str(environment.home),
        "config": str(environment.config_home),
        "data": str(environment.data_home),
        "cache": str(environment.cache_home),
        "state": str(environment.state_home),
        "runtime": variables.get("XDG_RUNTIME_DIR"),
        "data_dirs": [folder for folder in variables.get("XDG_DATA_DIRS", "").split(":") if folder],
        "binary": str(binary),
        "started": time.time(),
        "ready": None,
    }
    result = Capture(directory=directory, meta=meta, stacks=bool(options.get("stacks")))
    result._write()
    return result
