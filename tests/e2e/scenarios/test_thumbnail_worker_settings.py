# SPDX-License-Identifier: MIT
"""Thumbnail presentation and worker preferences are saved and searchable."""

import pytest
from PIL import Image


def _open_thumbnail_setting(strata):
    button = strata.window.find(role="button", name="Settings")
    assert button is not None and button.activate()
    search = strata.wait(
        lambda: strata.window.find(role="text", name="Search settings"),
        "settings search",
    )
    strata.pointer.click(search)
    strata.keyboard.type_text("Show thumbnails")
    return strata.wait(
        lambda: next(
            (
                node for node in strata.window.find_all(name="Show thumbnails")
                if node.role in {"check box", "toggle button", "switch"}
            ),
            None,
        ),
        "thumbnail switch",
    )


@pytest.mark.parametrize("mode", ["Columns", "List", "Icons"])
@pytest.mark.preferences(show_thumbnails=False)
def test_show_thumbnails_switch_persists_without_disabling_preview(strata, mode):
    Image.new("RGB", (80, 32), "green").save(strata.fixture.root / "photo.png")
    strata.switch_view(mode)
    strata.wait(lambda: strata.entry("photo.png"), "image in listing")
    strata.select_entry("photo.png")
    strata.keyboard.press("space")
    strata.wait(lambda: strata.preview() is not None, "image preview with thumbnails disabled")
    strata.pointer.click(strata.preview().find(role="button", name="Close preview (Space)"))
    strata.wait(lambda: strata.preview() is None, "preview closed")

    toggle = _open_thumbnail_setting(strata)
    assert not toggle.has_state("checked")
    for expected in (True, False):
        assert toggle.activate()
        strata.wait(
            lambda: toggle.has_state("checked") == expected
            and strata.environment.read_preferences().get("show_thumbnails") == str(expected).lower(),
            f"thumbnail preference saved as {expected}",
        )
    strata.application.stop()
    strata.application.start()
    assert not _open_thumbnail_setting(strata).has_state("checked")


@pytest.mark.preferences(thumbnail_workers=16)
def test_thumbnail_workers_stepper_persists_changes_and_resets(strata):
    settings = strata.window.find(role="button", name="Settings")
    assert settings is not None and settings.activate()
    search = strata.wait(
        lambda: strata.window.find(role="text", name="Search settings"),
        "settings search",
    )
    strata.pointer.click(search)
    strata.keyboard.type_text("thumbnail workers")

    def control(name):
        return strata.wait(
            lambda: strata.window.find(role="button", name=name),
            name,
        )

    for name, expected in [
        ("Decrease thumbnail workers", "15"),
        ("Increase thumbnail workers", "16"),
    ]:
        strata.pointer.click(control(name))
        strata.wait(
            lambda: strata.environment.read_preferences().get("thumbnail_workers") == expected,
            f"worker count saved as {expected}",
        )
    assert not control("Increase thumbnail workers").has_state("sensitive")
    reset = control("16")
    assert reset.description == "Reset thumbnail workers"
    strata.pointer.click(reset)
    strata.wait(
        lambda: strata.environment.read_preferences().get("thumbnail_workers") in {"1", "2", "3", "4"},
        "CPU-based default restored",
    )
