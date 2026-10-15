# SPDX-License-Identifier: MIT
"""Switching between the Columns, Icons, and List presentations."""

from __future__ import annotations

import math
import time

import pytest
from harness.interaction import SEPARATE_CLICK_GAP

from harness.keyboard_layout import AZERTY, US

MODES = ["Columns", "Icons", "List"]
MODE_SHORTCUTS = {"Columns": "ctrl+1", "Icons": "ctrl+2", "List": "ctrl+3"}
STORED_MODES = {"Columns": '"columns"', "Icons": '"icons"', "List": '"list"'}


@pytest.mark.parametrize("mode", ["Icons", "List"])
def test_appearance_menu_switches_presentation(strata, mode):
    assert strata.view_mode() == "Columns"
    directory = strata.fixture.root.name

    strata.switch_view(mode)

    assert strata.view_mode() == mode
    assert strata.pane().name == directory
    assert "documents" in strata.entry_names()
    strata.wait(
        lambda: strata.environment.read_preferences().get("browser_mode")
        == STORED_MODES[mode],
        "the chosen view to be written to settings.toml",
    )
    if mode == "List":
        strata.pointer.click(strata.header_button("Size"))
        expected_files = ["todo.txt", "readme.md"]
        strata.wait(
            lambda: strata.entry_names()[-2:] == expected_files,
            "the files to be sorted by size rather than name",
        )
        strata.open_appearance_menu()
        customize = strata.wait(
            lambda: strata.window.find(role="menu item", name="Customize columns…"),
            "the List column customization action",
        )
        assert customize.has_state("sensitive")
        strata.pointer.click(customize)
        size = strata.wait(
            lambda: strata.window.find(role="check menu item", name="Size"),
            "the List column visibility menu",
        )
        assert size.has_state("checked")
        for visible in [False, True, False]:
            strata.pointer.click(size)
            strata.wait(
                lambda: size.has_state("checked") == visible
                and (
                    '"size"'
                    in strata.environment.read_preferences().get("list_hidden_columns", "")
                ) == (not visible),
                "Size visibility to be saved without changing the sort",
            )
            assert strata.entry_names()[-2:] == expected_files
        reset = strata.window.find(role="menu item", name="Reset to default columns")
        assert reset is not None
        strata.pointer.click(reset)
        strata.wait(
            lambda: size.has_state("checked")
            and "list_hidden_columns" not in strata.environment.read_preferences(),
            "default column visibility to be restored",
        )
        assert strata.entry_names()[-2:] == expected_files


# AZERTY types & é " on the digit row without Shift; the row still switches views.
@pytest.mark.parametrize("layout", [US, AZERTY], ids=lambda layout: layout.name)
def test_shortcut_round_trip_preserves_selection_and_updates_the_appearance_menu(
    strata, keyboard_layout, layout
):
    strata.select_entry("todo.txt")
    for mode in ["Icons", "List", "Columns"]:
        with keyboard_layout(layout):
            strata.keyboard.press(MODE_SHORTCUTS[mode])
            strata.wait_for_view(mode)
        assert strata.pane_names()[0] == strata.fixture.root.name
        assert "documents" in strata.entry_names()
        strata.wait(
            lambda: "todo.txt" in strata.all_selected_names(),
            f"the selection to survive the switch to {mode}",
        )

        strata.open_appearance_menu()
        for candidate in MODES:
            option = strata.window.find(role="radio menu item", name=candidate)
            assert option is not None
            assert option.has_state("checked") == (candidate == mode), (mode, candidate)
        grouping = strata.window.find(role="check menu item", name="Group by file type")
        assert grouping is not None
        assert ("sensitive" in grouping.states) == (mode == "List")
        strata.keyboard.press("Escape")
        strata.wait(
            lambda: strata.window.find(role="radio menu item", name="Columns") is None,
            "the appearance menu to close",
        )


def test_switching_preserves_directory_selection_and_sort(strata):
    strata.select_entry("pictures")
    ascending = strata.entry_names("pictures")
    assert ascending == ["diagram.txt", "photo.txt"]

    strata.pointer.move_to(*strata.pane("pictures").screen_bounds().center)
    reverse_sort = strata.wait(
        lambda: strata.pane("pictures").find(
            role="button", name="Ascending — click to reverse"
        ),
        "the hovered pane's sort action to appear",
    )
    strata.pointer.click(reverse_sort)
    strata.wait(
        lambda: strata.entry_names("pictures") == list(reversed(ascending)),
        "the open pane to sort descending",
    )
    strata.select_entry("diagram.txt", directory="pictures")

    strata.keyboard.press(MODE_SHORTCUTS["List"])
    strata.wait_for_view("List")

    assert strata.pane().name == "pictures", (
        "switching should land on the directory the Columns view had open"
    )
    assert strata.entry_names() == list(reversed(ascending)), (
        "the descending sort should survive the switch"
    )
    strata.wait(
        lambda: strata.selected_names() == ["diagram.txt"],
        "the selection to survive the switch",
    )
    strata.wait_for_focused_entry("diagram.txt")


@pytest.mark.parametrize("start", ["input", "result"])
def test_switching_preserves_the_pane_filter(strata, start):
    strata.keyboard.press("ctrl+f")
    field = strata.editable_field()
    strata.keyboard.type_text("todo")
    strata.wait(lambda: field.text == "todo", "the filter query to be typed")
    strata.wait(
        lambda: strata.matches() == ["todo.txt"],
        "the filter to narrow the listing",
    )
    if start == "result":
        strata.keyboard.press("Down")
        strata.wait_for_focused_entry("todo.txt")

    for mode in ["Icons", "List", "Columns"]:
        strata.keyboard.press(MODE_SHORTCUTS[mode])
        strata.wait_for_view(mode)
        strata.wait(
            lambda: any(
                node.text == "todo"
                for node in strata.window.find_all(role="text", states={"editable"})
            ),
            "the filter query to survive the switch",
        )
        strata.wait(
            lambda: strata.matches() == ["todo.txt"],
            "the narrowed listing to survive the switch",
        )
        if start == "input":
            field = strata.editable_field()
            assert field.text == "todo"
            # Down from the field reaches a result, not the footer.
            strata.keyboard.press("Down")
            strata.wait_for_focused_entry("todo.txt")
            strata.keyboard.press("Up")
            strata.editable_field()
        else:
            strata.wait_for_focused_entry("todo.txt")


def test_list_column_resize_tracks_the_pointer_without_an_initial_jump(strata):
    strata.switch_view("List")
    for label in ["Name", "Mode", "Size", "Type", "Modified"]:
        heading = strata.wait(
            lambda: strata.pane().find(role="button", name=label),
            f"the {label} heading to appear",
        )
        cell = heading.parent
        assert cell is not None
        before = cell.screen_bounds()
        start = (before.x + before.width - 3, before.y + before.height // 2)
        strata.pointer.drag_points(start, (start[0] - 12, start[1]))
        strata.wait(
            lambda: abs(cell.screen_bounds().width - (before.width - 12)) <= 2,
            f"the {label} column to shrink by the pointer's 12-pixel movement",
        )


STANDARD_COLUMN_WIDTH = 300


def _saved_column_width(strata) -> int:
    return int(strata.environment.read_preferences().get("browser_column_width", 0))


def test_column_autofit_and_drag_stay_inside_the_browser_area(strata):
    long_name = "x" * 216
    strata.fixture.path(long_name).write_text("")
    strata.entry(long_name)
    pane = strata.pane()
    column = pane.screen_bounds()
    middle = column.y + column.height // 2
    window = strata.window_bounds()
    # A fresh column has the standard width times the interface scale, and
    # saved widths are unscaled.
    scale = column.width / STANDARD_COLUMN_WIDTH
    # The root column starts at the browser area's left edge; the preview slot
    # yields to a column, so the area runs to the window's right edge.
    area = math.ceil((window.x + window.width - column.x) / scale)

    strata.pointer.double_click(pane, at=(column.x + column.width - 3, middle))
    fitted = strata.wait(
        lambda: (width := _saved_column_width(strata)) > STANDARD_COLUMN_WIDTH and width,
        "autofit to save the fitted width",
    )
    assert fitted <= area, f"autofit saved {fitted}, wider than the browser area ({area} unscaled px)"

    # Narrow the column first so the outward drag has room to save a change.
    time.sleep(SEPARATE_CLICK_GAP)
    edge = strata.pane().screen_bounds()
    start = (edge.x + edge.width - 3, middle)
    strata.pointer.drag_points(start, (start[0] - 200, middle))
    narrowed = strata.wait(
        lambda: (width := _saved_column_width(strata)) < fitted and width,
        "the inward drag to save a narrower width",
    )

    time.sleep(SEPARATE_CLICK_GAP)
    edge = strata.pane().screen_bounds()
    start = (edge.x + edge.width - 3, middle)
    strata.pointer.drag_points(start, (window.x + window.width + 300, middle))
    dragged = strata.wait(
        lambda: (width := _saved_column_width(strata)) > narrowed and width,
        "the outward drag to save a wider width",
    )
    assert dragged <= area, f"the drag saved {dragged}, wider than the browser area ({area} unscaled px)"


@pytest.mark.preferences(browser_column_width=1729)
def test_an_oversized_saved_column_width_opens_inside_the_window(strata):
    pane = strata.pane()
    refresh = strata.wait(
        lambda: pane.find(role="button", name="Refresh (F5)"),
        "the column's header actions",
    )
    window = strata.window_bounds()
    window_right = window.x + window.width
    # Let the startup layout (sidebar budget, preview reservation) settle.
    time.sleep(SEPARATE_CLICK_GAP)
    column = pane.screen_bounds()
    actions = refresh.screen_bounds()
    assert column.x + column.width <= window_right, (
        f"the column's resize edge at {column.x + column.width} is past the window edge {window_right}"
    )
    assert actions.x + actions.width <= window_right, (
        f"the header actions end at {actions.x + actions.width}, past the window edge {window_right}"
    )
