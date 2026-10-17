# SPDX-License-Identifier: MIT
"""Icons thumbnail sizes follow their own wheel and key shortcuts, apart from text size."""

import pytest


@pytest.mark.preferences(
    browser_mode="icons",
    icons_thumbnail_size=64,
    remember_folder_views=False,
    text_size=21,
)
def test_shift_wheel_and_ctrl_alt_keys_resize_thumbnails_not_text(strata):
    def thumbnail_size(expected, waiting_for):
        strata.wait(
            lambda: strata.environment.read_preferences().get("icons_thumbnail_size")
            == expected,
            waiting_for,
        )

    listing = strata.entry_container()
    strata.settle(listing)
    strata.pointer.scroll(
        listing.screen_bounds().center, clicks=2, down=False, modifiers=["ctrl", "shift"]
    )
    thumbnail_size("80", "Ctrl+Shift+wheel up to enlarge thumbnails two steps")
    strata.pointer.scroll(
        listing.screen_bounds().center, clicks=1, down=True, modifiers=["ctrl", "shift"]
    )
    thumbnail_size("72", "Ctrl+Shift+wheel down to shrink thumbnails one step")
    assert strata.environment.read_preferences()["text_size"] == "21"

    strata.keyboard.press("ctrl+alt+=")
    thumbnail_size("80", "Ctrl+Alt+= to enlarge thumbnails")
    strata.keyboard.press("ctrl+alt+-")
    thumbnail_size("72", "Ctrl+Alt+- to shrink thumbnails")
    strata.keyboard.press("ctrl+alt+0")
    thumbnail_size("64", "Ctrl+Alt+0 to restore the default thumbnail size")
    assert strata.environment.read_preferences()["text_size"] == "21"
