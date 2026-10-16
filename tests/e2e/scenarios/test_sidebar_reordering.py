# SPDX-License-Identifier: MIT

import pytest


def _place_names(strata):
    return [
        row.name for row in strata.sidebar_button("Home").parent.children
        if row.role == "button"
    ]


@pytest.mark.parametrize("edge", ["before", "after"])
@pytest.mark.parametrize("target_route", ["row", "gap"])
def test_dragging_builtin_places_reorders_the_sidebar_and_survives_restart(strata, edge, target_route):
    home = strata.sidebar_button("Home")
    trash = strata.sidebar_button("Trash")
    assert _place_names(strata)[:2] == ["Home", "Trash"]

    source, target = (trash, home) if edge == "before" else (home, trash)
    bounds = target.screen_bounds()
    inset = max(1, bounds.height // 5)
    if target_route == "gap":
        y = bounds.y - 1 if edge == "before" else bounds.y + bounds.height + 1
    else:
        y = bounds.y + inset if edge == "before" else bounds.y + bounds.height - inset
    strata.pointer.drag_points(
        source.screen_bounds().center,
        (bounds.center[0], y),
        steps=12,
    )

    strata.wait(
        lambda: _place_names(strata)[:2] == ["Trash", "Home"],
        "the dropped Trash row to precede Home",
    )
    strata.application.stop()
    strata.application.start()
    assert _place_names(strata)[:2] == ["Trash", "Home"]


@pytest.mark.parametrize(
    "preferences", [{"sidebar_compact": False}, {"sidebar_compact": True}],
    indirect=True, ids=["expanded", "compact"],
)
def test_crossing_both_row_edges_and_their_gap_keeps_one_insertion_destination(strata, preferences):
    bookmarks = strata.environment.config_home / "gtk-3.0" / "bookmarks"
    bookmarks.parent.mkdir(parents=True, exist_ok=True)
    entries = [f"sftp://host/{name.lower()} {name}" for name in ("One", "Two", "Three")]
    bookmarks.write_text("\n".join(entries) + "\n")
    strata.application.stop()
    strata.application.start()
    above = strata.sidebar_button("One").screen_bounds()
    below = strata.sidebar_button("Two").screen_bounds()
    gap = strata.window.find_all(role="grouping", name="PINNED")[1].screen_bounds().center
    strata.pointer.drag_points(
        strata.sidebar_button("Three").screen_bounds().center,
        (above.center[0], above.y + above.height - 2),
        release=False,
    )
    try:
        for point in (gap, (below.center[0], below.y + 2), gap):
            strata.pointer.move_to(*point)
    finally:
        strata.pointer.connection.button(1, False)
    strata.wait(
        lambda: bookmarks.read_text().splitlines() == [entries[0], entries[2], entries[1]],
        "all three hover targets to commit the same pin insertion",
    )


def test_builtin_and_pinned_rows_cannot_reorder_into_each_others_section(strata):
    bookmarks = strata.environment.config_home / "gtk-3.0" / "bookmarks"
    bookmarks.parent.mkdir(parents=True, exist_ok=True)
    expected = "sftp://host/one One\nsftp://host/two Two\n"
    bookmarks.write_text(expected)
    strata.application.stop()
    strata.application.start()

    for source, target in [("Two", "Home"), ("Home", "Two")]:
        bounds = strata.sidebar_button(target).screen_bounds()
        strata.pointer.drag_points(
            strata.sidebar_button(source).screen_bounds().center,
            (bounds.center[0], bounds.y + max(1, bounds.height // 5)),
        )
    strata.application.stop()
    strata.application.start()
    assert bookmarks.read_text() == expected
    assert _place_names(strata)[:2] == ["Home", "Trash"]
