# SPDX-License-Identifier: MIT

import sys
from pathlib import Path

import pytest

from harness import tree
from harness.environment import process_environment
from harness.process import ManagedProcess, terminate
from harness.modes import ALL_MODES


def bookmarks(strata):
    path = strata.environment.config_home / "gtk-3.0" / "bookmarks"
    return path.read_text().splitlines() if path.exists() else []


def pin_gap(strata, index=0):
    return strata.wait(
        lambda: strata.window.find_all(role="grouping", name="PINNED") or None,
        "the Pinned insertion targets",
    )[index]


def drag_to_pin(strata, names, index=0, *, modifiers=()):
    directory = strata.fixture.root.name
    strata.select_entry(names[0], directory)
    for name in names[1:]:
        strata.pointer.click(strata.entry(name, directory), modifiers=["ctrl"])
    strata.wait(
        lambda: set(strata.selected_names(directory)) == set(names),
        "the drag selection in its source pane",
    )
    strata.pointer.drag_points(
        strata.pointer.drag_origin(strata.entry(names[0], directory)),
        pin_gap(strata, index).screen_bounds().center,
        release=False,
        modifiers=modifiers,
    )
    strata.pointer.connection.button(1, False)


@pytest.mark.parametrize("mode", ALL_MODES)
@pytest.mark.parametrize("move_only", [False, True], ids=["copy-or-move", "move-only"])
def test_folder_pins_keep_selection_order_skip_duplicates_and_survive_restart(strata, mode, move_only):
    modifiers = ["shift"] if move_only else []
    names = ["pin-a", "pin-b", "pin-c", "pin-d", "pin-e"]
    for name in names:
        strata.fixture.path(name).mkdir()
        strata.entry(name)
    drag_to_pin(strata, [names[1]], modifiers=modifiers)
    strata.wait(lambda: len(bookmarks(strata)) == 1, "the first folder pin")
    drag_to_pin(strata, [names[0], names[1], names[2]], modifiers=modifiers)
    expected = [
        f"{strata.fixture.path(name).as_uri()} {name}"
        for name in [names[0], names[2], names[1]]
    ]
    strata.wait(lambda: bookmarks(strata) == expected, "selection-order insertion without duplicates")
    drag_to_pin(strata, [names[3]], index=1, modifiers=modifiers)
    expected.insert(1, f"{strata.fixture.path(names[3]).as_uri()} {names[3]}")
    strata.wait(lambda: bookmarks(strata) == expected, "insertion between pinned rows")
    drag_to_pin(strata, [names[4]], index=-1, modifiers=modifiers)
    expected.append(f"{strata.fixture.path(names[4]).as_uri()} {names[4]}")
    strata.wait(lambda: bookmarks(strata) == expected, "insertion after the last pin")
    drag_to_pin(strata, [names[0]], index=-1, modifiers=modifiers)
    for name in names:
        assert strata.fixture.path(name).is_dir(), "pinning must not move the source"
    strata.application.stop()
    strata.application.start()
    assert bookmarks(strata) == expected
    for name in names:
        strata.sidebar_button(name)


@pytest.mark.parametrize("mode", ALL_MODES)
@pytest.mark.parametrize("mixed", [False, True])
def test_files_and_mixed_selections_cannot_pin_or_transfer_through_a_gap(strata, mode, mixed):
    folder = strata.fixture.path("pin-folder")
    folder.mkdir()
    file = strata.fixture.path("pin-file.txt")
    file.write_text("unchanged")
    names = [file.name, folder.name] if mixed else [file.name]
    for name in names:
        strata.entry(name)
    drag_to_pin(strata, names)
    strata.pointer.click(strata.sidebar_button("Home"))
    strata.wait_for_directory(strata.environment.home.name)
    assert bookmarks(strata) == []
    assert folder.is_dir()
    assert file.read_text() == "unchanged"


@pytest.mark.parametrize(("mixed", "move_only"), [(False, False), (True, False), (False, True)])
def test_foreign_folder_drags_validate_the_entire_selection_before_pinning(strata, mixed, move_only):
    folder = strata.fixture.path("foreign-folder")
    folder.mkdir()
    alias = strata.fixture.path("foreign-alias")
    alias.symlink_to(folder, target_is_directory=True)
    paths = [folder, alias]
    if mixed:
        file = strata.fixture.path("foreign-file.txt")
        file.write_text("not a folder")
        paths.append(file)
    environment = {
        **process_environment(),
        **strata.environment.variables(),
        **strata.display.environment,
    }
    source = ManagedProcess.spawn(
        "foreign-drag-source",
        [
            sys.executable,
            str(Path(__file__).parents[1] / "fixtures/file_drag_source.py"),
            *(["--move-only"] if move_only else []),
            *map(str, paths),
        ],
        log_dir=strata.environment.root,
        env=environment,
    )
    try:
        button = strata.wait(
            lambda: tree.desktop().find(role="button", name="Foreign file selection"),
            "the foreign drag source",
        )
        strata.pointer.drag(button, pin_gap(strata))
        strata.wait(
            lambda: tree.desktop().find(role="button", name="Foreign drag finished"),
            "external validation and the drop protocol to finish",
        )
        expected = [] if mixed or move_only else [f"{path.as_uri()} {path.name}" for path in paths]
        assert bookmarks(strata) == expected
        assert folder.is_dir() and alias.is_symlink()
    finally:
        terminate(source.popen)


def test_pinned_folder_rows_remain_transfer_targets_and_remote_pins_have_gaps(strata):
    path = strata.environment.config_home / "gtk-3.0" / "bookmarks"
    destination = strata.fixture.path("pin-destination")
    destination.mkdir()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"sftp://host/unavailable Remote\n{destination.as_uri()} Destination\n")
    strata.application.stop()
    strata.application.start()
    source = strata.fixture.path("pin-source")
    source.mkdir()
    strata.entry(source.name)
    drag_to_pin(strata, [source.name], index=1)
    strata.wait(
        lambda: bookmarks(strata) == [
            "sftp://host/unavailable Remote",
            f"{source.as_uri()} {source.name}",
            f"{destination.as_uri()} Destination",
        ],
        "a folder to pin after a remote row",
    )
    file = strata.fixture.path("transfer.txt")
    file.write_text("row transfer")
    strata.entry(file.name, strata.fixture.root.name)
    strata.pointer.drag(strata.entry(file.name, strata.fixture.root.name), strata.sidebar_button("Destination"))
    strata.wait(lambda: (destination / file.name).exists(), "the row transfer")
    assert not file.exists()
    assert (destination / file.name).read_text() == "row transfer"
    assert source.is_dir()
    remote = strata.sidebar_button("Remote")
    target = strata.sidebar_button("Destination").screen_bounds()
    strata.pointer.drag_points(
        remote.screen_bounds().center,
        (target.center[0], target.y + target.height - max(1, target.height // 5)),
    )
    strata.wait(
        lambda: bookmarks(strata)[-1] == "sftp://host/unavailable Remote",
        "existing pinned-row reordering to remain available",
    )
    expected = [
        f"{source.as_uri()} {source.name}",
        f"{destination.as_uri()} Destination",
        "sftp://host/unavailable Remote",
    ]
    for gap_index, insert_index in [(0, 0), (-1, 2), (1, 1)]:
        strata.pointer.drag(strata.sidebar_button("Remote"), pin_gap(strata, gap_index))
        expected.remove("sftp://host/unavailable Remote")
        expected.insert(insert_index, "sftp://host/unavailable Remote")
        strata.wait(lambda: bookmarks(strata) == expected, "the pin to reorder through its insertion gap")
    strata.pointer.drag(strata.sidebar_button("Destination"), pin_gap(strata, 0))
    expected.insert(0, expected.pop())
    strata.wait(lambda: bookmarks(strata) == expected, "the local pin to reorder without transferring its contents")
    strata.application.stop()
    strata.application.start()
    assert bookmarks(strata) == expected
    assert source.is_dir() and destination.is_dir()
    assert (destination / file.name).read_text() == "row transfer"
