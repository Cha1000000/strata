# SPDX-License-Identifier: MIT

from pathlib import Path
from urllib.parse import unquote, urlparse
import os
import sys
import time
import tomllib

import pytest
from gi.repository import GLib

from harness import tree
from harness.browser import ENTRY_ROLES
from harness.environment import process_environment
from harness.portal import open_file_request
from harness.process import ManagedProcess, terminate


def _find_row(chooser, name):
    for role in ENTRY_ROLES:
        node = chooser.find(role=role, name=name)
        if node is not None:
            return node
    return None


def _row(chooser, name):
    return tree.wait_until(lambda: _find_row(chooser, name), message=f"the {name!r} row")


def _selected(chooser, name):
    row = _find_row(chooser, name)
    return row is not None and row.has_state("selected")


@pytest.mark.parametrize("browser_mode", ["list", "columns"])
def test_enter_returns_every_selected_file_in_a_multiple_open_request(
    strata_binary, headless_display, test_environment, fixture_tree, keyboard, pointer, browser_mode
):
    options = {
        "current_folder": GLib.Variant("ay", os.fsencode(fixture_tree.root) + b"\0"),
        "multiple": GLib.Variant("b", True),
    }
    with open_file_request(
        strata_binary, headless_display, test_environment, fixture_tree,
        title="Multiple selection test", options=options, browser_mode=browser_mode,
    ) as (chooser, response):
        pointer.click(_row(chooser, "readme.md"))
        pointer.click(_row(chooser, "todo.txt"), modifiers=["ctrl"])
        tree.wait_until(
            lambda: _selected(chooser, "readme.md") and _selected(chooser, "todo.txt"),
            message="both files selected",
        )
        keyboard.press("Return")

        tree.wait_until(response.done, message="portal response", timeout=20)
        status, values = response.result().unpack()
        assert status == 0
        chosen = {Path(unquote(urlparse(uri).path)).name for uri in values["uris"]}
        assert chosen == {"readme.md", "todo.txt"}


@pytest.mark.parametrize("browser_mode", ["list", "columns", "icons"])
def test_chooser_sidebar_external_drops_survive_shrinking_and_expanding(
    strata_binary, headless_display, test_environment, fixture_tree, keyboard, pointer, browser_mode
):
    bookmarks = test_environment.config_home / "gtk-3.0" / "bookmarks"
    bookmarks.parent.mkdir(parents=True, exist_ok=True)
    destination = fixture_tree.path("pictures")
    remote = "sftp://host/unavailable Remote\n"
    existing = f"{remote}{destination.as_uri()} Destination\n"
    bookmarks.write_text(existing)
    desktop = test_environment.home / "Desktop"
    desktop.mkdir()
    (test_environment.config_home / "user-dirs.dirs").write_text('XDG_DESKTOP_DIR="$HOME/Desktop"\n')
    options = {
        "current_folder": GLib.Variant("ay", os.fsencode(fixture_tree.root) + b"\0"),
    }
    with open_file_request(
        strata_binary, headless_display, test_environment, fixture_tree,
        title="Sidebar drag test", options=options, browser_mode=browser_mode,
    ) as (chooser, response):
        def target():
            return tree.wait_until(
                lambda: chooser.find(role="button", name="Destination"),
                message="chooser sidebar destination",
            )

        def insertion_gap(index):
            return tree.wait_until(
                lambda: chooser.find_all(role="grouping", name="PINNED") or None,
                message="shared chooser pin insertion targets",
            )[index]

        assert chooser.find(role="button", name="Remote") is None
        for name, width, pin in [("readme.md", 400, False), ("todo.txt", 1200, False), ("archive", 400, True)]:
            source = fixture_tree.path(name)
            contents = None if pin else source.read_text()

            def drop_target():
                return insertion_gap(-1) if pin else target()
            environment = {
                **process_environment(), **test_environment.variables(), **headless_display.environment,
            }
            foreign = ManagedProcess.spawn(
                "chooser-drag-source",
                [sys.executable, str(Path(__file__).parents[1] / "fixtures/file_drag_source.py"), str(source)],
                log_dir=test_environment.root, env=environment,
            )
            try:
                button = tree.wait_until(
                    lambda: tree.desktop().find(role="button", name="Foreign file selection"),
                    message="external chooser drag source",
                )
                pointer.drag_points(
                    button.screen_bounds().center,
                    drop_target().screen_bounds().center,
                    release=False,
                )
                try:
                    bounds = chooser.window_bounds()
                    pointer.connection.resize_surface(bounds.width, bounds.height, width, bounds.height)
                    # Let the native configure and sidebar fit update run before retargeting.
                    time.sleep(0.5)
                    pointer.move_to(*drop_target().screen_bounds().center)
                finally:
                    pointer.connection.button(1, False)
                if pin:
                    expected = existing + f"{source.as_uri()} {name}\n"
                    tree.wait_until(lambda: bookmarks.read_text() == expected, message="folder pinned in chooser")
                    assert source.is_dir() and not (destination / name).exists()
                else:
                    tree.wait_until(
                        lambda: (destination / name).exists(),
                        message="file copied by resized external chooser drag",
                    )
                    assert (destination / name).read_text() == contents
                    assert source.read_text() == contents
                    assert bookmarks.read_text() == existing
                tree.wait_until(
                    lambda: tree.desktop().find(role="button", name="Foreign drag finished"),
                    message="external chooser drag protocol completed without deletion",
                )
                assert not response.done(), "a sidebar drop must not accept or close the chooser"
            finally:
                terminate(foreign.popen)

        pointer.drag(chooser.find(role="button", name="archive"), insertion_gap(0))
        expected = f"{remote}{fixture_tree.path('archive').as_uri()} archive\n{destination.as_uri()} Destination\n"
        tree.wait_until(lambda: bookmarks.read_text() == expected, message="shared chooser pin reordering")
        home = chooser.find(role="button", name="Home").screen_bounds()
        pointer.drag_points(
            chooser.find(role="button", name="Desktop").screen_bounds().center,
            (home.center[0], home.y - 1),
        )
        tree.wait_until(
            lambda: tomllib.loads(test_environment.settings_path.read_text())["sidebar_order"][:2] == ["desktop", "home"],
            message="chooser place reordering saved by the shared sidebar",
        )
        assert not response.done()
        pointer.click(target())
        pointer.click(_row(chooser, "readme.md"))
        keyboard.press("Return")
        tree.wait_until(response.done, message="chooser still accepts a file after dragging", timeout=20)
        status, values = response.result().unpack()
        assert status == 0
        assert [Path(unquote(urlparse(uri).path)) for uri in values["uris"]] == [
            destination / "readme.md"
        ]


@pytest.mark.parametrize("tenxer", [False, True], ids=["default", "tenxer"])
def test_pdf_zoom_reset_in_chooser_keeps_interface_text_size(
    strata_binary, headless_display, test_environment, fixture_tree, keyboard, pointer, tenxer
):
    fixture_tree.path("page.pdf").write_bytes(
        (Path(__file__).resolve().parents[2] / "fixtures" / "documents" / "text.pdf").read_bytes()
    )
    options = {
        "current_folder": GLib.Variant("ay", os.fsencode(fixture_tree.root) + b"\0"),
    }
    with open_file_request(
        strata_binary, headless_display, test_environment, fixture_tree,
        title="PDF zoom reset", options=options, browser_mode="list",
    ) as (chooser, response):
        pointer.click(_row(chooser, "page.pdf"))
        keyboard.press("ctrl+=")
        tree.wait_until(
            lambda: test_environment.read_preferences()["text_size"] == "14",
            message="custom chooser interface text size",
        )
        keyboard.press("space")
        page = tree.wait_until(
            lambda: chooser.find(role="image", name="PDF page text"),
            message="PDF page in chooser preview",
        )
        pointer.click(page)
        if tenxer:
            keyboard.press("ctrl+shift+m")
            tree.wait_until(
                lambda: test_environment.read_preferences().get("tenxer_mode") == "true",
                message="10xer enabled in chooser",
            )
        fitted = page.screen_bounds().width
        pointer.scroll(page.screen_bounds().center, clicks=2, down=False, modifiers=["ctrl"])
        tree.wait_until(
            lambda: page.screen_bounds().width > fitted,
            message="Ctrl+wheel over the chooser PDF to enlarge its page",
        )
        keyboard.press("ctrl+0")
        tree.wait_until(
            lambda: page.screen_bounds().width == fitted,
            message="Ctrl+0 on the focused chooser PDF to restore the fitted page width",
        )
        assert test_environment.read_preferences()["text_size"] == "14"
        assert not response.done()
        keyboard.press("Escape")
        tree.wait_until(
            lambda: chooser.find(role="image", name="PDF page text") is None,
            message="preview closed before resetting interface text",
        )
        keyboard.press("ctrl+0")
        tree.wait_until(
            lambda: test_environment.read_preferences()["text_size"] == "13",
            message="interface reset after returning focus to chooser files",
        )
