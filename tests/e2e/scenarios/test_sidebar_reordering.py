# SPDX-License-Identifier: MIT

import pytest


@pytest.mark.parametrize("edge", ["before", "after"])
def test_dragging_builtin_places_reorders_the_sidebar_and_survives_restart(strata, edge):
    home = strata.sidebar_button("Home")
    trash = strata.sidebar_button("Trash")
    assert [row.name for row in home.parent.children[:2]] == ["Home", "Trash"]

    source, target = (trash, home) if edge == "before" else (home, trash)
    bounds = target.screen_bounds()
    inset = max(1, bounds.height // 5)
    y = bounds.y + inset if edge == "before" else bounds.y + bounds.height - inset
    strata.pointer.drag_points(
        source.screen_bounds().center,
        (bounds.center[0], y),
        steps=12,
    )

    strata.wait(
        lambda: [row.name for row in strata.sidebar_button("Home").parent.children[:2]]
        == ["Trash", "Home"],
        "the dropped Trash row to precede Home",
    )
    strata.application.stop()
    strata.application.start()
    assert [row.name for row in strata.sidebar_button("Home").parent.children[:2]] == [
        "Trash", "Home"
    ]


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
    assert [row.name for row in strata.sidebar_button("Home").parent.children[:2]] == [
        "Home", "Trash"
    ]
