# SPDX-License-Identifier: MIT

from __future__ import annotations

import base64
import time
import zipfile

import pytest

from harness.fixtures import FixtureTree
from harness.interaction import SEPARATE_CLICK_GAP

# Hover and late release selection follow GTK's next event; give them time to land before asserting
# that they did not happen.
SETTLE_HOLD = 0.3


# ZIP and header-encrypted 7z containing protected.txt; password: fixture-password.
ENCRYPTED_ARCHIVES = {
    "zip": "UEsDBAoACQAAABdsM12zQ1U4JwAAABsAAAANABwAcHJvdGVjdGVkLnR4dFVUCQADXuOual7jrmp1eAsAAQToAwAABOgDAACQyEkvHVKZmURUaFrAXIjmRah+W9tCT9En6fCshzKr4S/pzBgPWBlQSwcIs0NVOCcAAAAbAAAAUEsBAh4DCgAJAAAAF2wzXbNDVTgnAAAAGwAAAA0AGAAAAAAAAQAAAKSBAAAAAHByb3RlY3RlZC50eHRVVAUAA17jrmp1eAsAAQToAwAABOgDAABQSwUGAAAAAAEAAQBTAAAAfgAAAAAA",
    "7z": "N3q8ryccAARK90rXoAAAAAAAAAAvAAAAAAAAAM2lRMsbe4LgOwc6BQCZYKv5bMKO/JVT8lQovoXQhOLoYQdQk+uWUC0i3pVYfHvs3WsWQxoB/iug9vZINEehfZwB2T+4IWJod2G0L8SsGPd8fL2S7eqrf0iiMWv3YxcjHoairfwd3pFxFkmBBuIT/XLKPJC7MVu79tKCB+pPvxRxvfdd0S2wr+A0/f/dCbpioH0eSSubzTPZlYFEs1FLcbo7KwIJFwYgAQmAgAAHCwEAASQG8QcBElMPiTvkREbaQxsUI9zKFIworgxyCgEqHRUKAAA=",
}


@pytest.fixture
def fixture_tree(request):
    extension = request.param
    if extension == "mixed":
        tree = FixtureTree.create({"a.txt": "before", "bundle.zip": "", "c.txt": "middle", "encrypted.zip": ""})
        with zipfile.ZipFile(tree.path("bundle.zip"), "w") as archive:
            archive.writestr("top.txt", "top")
        tree.path("encrypted.zip").write_bytes(base64.b64decode(ENCRYPTED_ARCHIVES["zip"]))
    elif extension == "plain":
        tree = FixtureTree.create({"a.txt": "before", "bundle.zip": "", "z.txt": "after"})
        with zipfile.ZipFile(tree.path("bundle.zip"), "w") as archive:
            archive.writestr("docs/inside.txt", "inside")
            archive.writestr("docs/inner/deep.txt", "deep")
            archive.writestr("docs/more/a.txt", "a")
            archive.writestr("docs/more/b.txt", "b")
            archive.writestr("top.txt", "top")
            archive.writestr("zz.txt", "last")
    else:
        tree = FixtureTree.create({f"encrypted.{extension}": ""})
        tree.path(f"encrypted.{extension}").write_bytes(base64.b64decode(ENCRYPTED_ARCHIVES[extension]))
    try:
        yield tree
    finally:
        tree.cleanup()


def archive_row(strata, text):
    preview = strata.preview()
    if preview is None:
        return None
    for row in preview.find_all(role="list item"):
        if any(label.name == text for label in row.find_all(role="label")):
            return row
    return None


@pytest.mark.parametrize("fixture_tree", ["plain"], indirect=True)
@pytest.mark.parametrize("enter_key", ["Right", "Return", "space"])
@pytest.mark.parametrize("automatic", [
    pytest.param(True, marks=pytest.mark.preferences(single_click_previews=True), id="automatic"),
    pytest.param(False, marks=pytest.mark.preferences(single_click_previews=False), id="manual"),
])
@pytest.mark.preferences(browser_mode="columns", type_to_search=False)
def test_columns_archive_preview_waits_for_explicit_entry(strata, fixture_tree, enter_key, automatic):
    root = strata.current_directory()
    strata.select_entry_with_keyboard("a.txt")
    strata.keyboard.press("Down")
    strata.wait_for_focused_entry("bundle.zip")
    if automatic:
        strata.wait(lambda: strata.preview_shows("docs/"), "the passive archive contents")
        assert strata.preview().find(states={"selected"}) is None
        member = strata.settle(archive_row(strata, "top.txt"))
        strata.pointer.move_to(*member.screen_bounds().center)
        strata.settle(member)
        time.sleep(SETTLE_HOLD)
        assert strata.preview().find(states={"selected"}) is None, "hovering selected a member"
        assert strata.focused_name() == "bundle.zip"
    else:
        assert strata.preview() is None

    strata.keyboard.press("Down")
    strata.wait_for_focused_entry("z.txt")
    strata.keyboard.press("Up")
    strata.wait_for_focused_entry("bundle.zip")
    if automatic:
        strata.wait(lambda: strata.preview_shows("docs/"), "the archive contents again")
    strata.keyboard.press(enter_key)
    strata.wait(lambda: strata.focused_name() is None, "explicit entry to focus the archive tree")
    strata.wait(lambda: strata.preview_shows("docs/"), "the explicitly opened archive contents")
    strata.keyboard.press("Right")
    strata.wait(lambda: strata.preview_shows("inside.txt"), "Right to open the archive folder")
    strata.keyboard.press("Escape")
    strata.wait_for_focused_entry("bundle.zip")
    strata.keyboard.press("Down")
    strata.wait_for_focused_entry("z.txt")
    assert strata.current_directory() == root
    assert sorted(path.name for path in fixture_tree.root.iterdir()) == ["a.txt", "bundle.zip", "z.txt"]


@pytest.mark.parametrize("fixture_tree", ["mixed"], indirect=True)
@pytest.mark.parametrize("browser_mode", [
    pytest.param("list", marks=pytest.mark.preferences(browser_mode="list"), id="list"),
    pytest.param("icons", marks=pytest.mark.preferences(browser_mode="icons"), id="icons"),
])
@pytest.mark.preferences(type_to_search=False)
def test_open_preview_does_not_take_focus_when_navigating_onto_archives(strata, fixture_tree, browser_mode):
    strata.select_entry_with_keyboard("a.txt")
    strata.keyboard.press("space")
    strata.wait(lambda: strata.preview_shows("before"), "the text preview")
    next_key = "Down" if browser_mode == "list" else "Right"

    strata.keyboard.press(next_key)
    strata.wait_for_focused_entry("bundle.zip")
    strata.wait(lambda: strata.preview_shows("top.txt"), "the passive archive contents")
    time.sleep(SETTLE_HOLD)
    assert strata.focused_name() == "bundle.zip", "the archive took the keys"
    assert strata.preview().find(states={"selected"}) is None

    strata.keyboard.press(next_key)
    strata.wait_for_focused_entry("c.txt")
    strata.keyboard.press(next_key)
    strata.wait_for_focused_entry("encrypted.zip")
    strata.wait(lambda: strata.preview_shows("Password-protected archive"), "the password prompt")
    time.sleep(SETTLE_HOLD)
    assert strata.focused_name() == "encrypted.zip", "the password prompt took the keys"
    assert strata.preview().find(states={"editable", "focused"}) is None

    strata.keyboard.press("Home")
    strata.wait_for_focused_entry("a.txt")


@pytest.mark.parametrize("fixture_tree", ["plain"], indirect=True)
@pytest.mark.preferences(browser_mode="list", type_to_search=False)
def test_archive_rows_select_on_click_not_hover(strata, fixture_tree):
    strata.select_entry("bundle.zip")
    strata.keyboard.press("space")
    strata.wait(lambda: strata.preview_shows("docs/"), "the archive contents")
    strata.wait(
        lambda: archive_row(strata, "docs/").has_state("selected"),
        "the first member to be highlighted",
    )

    zz = strata.settle(archive_row(strata, "zz.txt"))
    strata.pointer.move_to(*zz.screen_bounds().center)
    strata.settle(zz)
    time.sleep(SETTLE_HOLD)
    assert not zz.has_state("selected"), "hovering selected a member"
    assert archive_row(strata, "docs/").has_state("selected")
    strata.keyboard.press("Down")
    strata.wait(
        lambda: archive_row(strata, "top.txt").has_state("selected"),
        "Down to continue from the highlighted member, not the hovered one",
    )

    strata.pointer.click(archive_row(strata, "top.txt"))
    strata.wait(lambda: archive_row(strata, "top.txt").has_state("selected"), "the clicked file")
    assert strata.preview_shows("top.txt") and not strata.preview_shows("inside.txt")
    strata.keyboard.press("Down")
    strata.wait(lambda: archive_row(strata, "zz.txt").has_state("selected"), "Down from the clicked file")

    strata.pointer.click(archive_row(strata, "docs/"), modifiers=["ctrl"])
    strata.wait(lambda: strata.preview_shows("inside.txt"), "a Ctrl+click to open the folder")
    strata.keyboard.press("Left")
    strata.wait(lambda: strata.preview_shows("top.txt"), "Left to return to the archive root")

    docs = archive_row(strata, "docs/").screen_bounds()
    strata.pointer.click(archive_row(strata, "docs/"), at=(docs.center[0], docs.y + 1))
    strata.wait(lambda: strata.preview_shows("inside.txt"), "a click on the row's edge to open the folder")
    strata.keyboard.press("Left")
    strata.wait(lambda: strata.preview_shows("top.txt"), "Left to return to the archive root")

    time.sleep(SEPARATE_CLICK_GAP)
    strata.pointer.double_click(archive_row(strata, "docs/"))
    strata.wait(lambda: strata.preview_shows("inner/"), "a double-click to open the folder")
    assert not strata.preview_shows("deep.txt"), "a double-click opened two levels"
    assert archive_row(strata, "inner/").has_state("selected")

    time.sleep(SEPARATE_CLICK_GAP)
    strata.pointer.double_click(archive_row(strata, "more/"))
    strata.wait(lambda: strata.preview_shows("b.txt"), "a double-click to open the second folder")
    time.sleep(SETTLE_HOLD)
    assert archive_row(strata, "a.txt").has_state("selected"), "the double-click's second release moved the highlight"

    strata.keyboard.press("Escape")
    strata.wait(lambda: strata.preview() is None, "Escape to close the archive preview")
    assert sorted(path.name for path in fixture_tree.root.iterdir()) == ["a.txt", "bundle.zip", "z.txt"]


@pytest.mark.parametrize("fixture_tree", ["zip", "7z"], indirect=True)
def test_archive_password_retry_crosses_the_real_sandbox(strata, fixture_tree):
    filename = next(fixture_tree.root.iterdir()).name
    strata.select_entry(filename)
    strata.keyboard.press("space")
    strata.wait(lambda: strata.preview_shows("Password-protected archive"), "the password prompt")
    assert not strata.preview_shows("protected.txt")

    strata.wait(
        lambda: strata.preview().find(states={"editable", "focused"}),
        "the password entry to take focus",
    )
    strata.keyboard.type_text("dismiss-this-password")
    strata.keyboard.press("Escape")
    strata.wait(lambda: strata.preview() is None, "Escape to close the password prompt")
    strata.wait_for_focused_entry(filename)
    strata.keyboard.press("space")
    strata.wait(
        lambda: strata.preview() and strata.preview().find(states={"editable", "focused"}),
        "the reopened password entry to take focus",
    )
    strata.keyboard.type_text("incorrect-password")
    strata.keyboard.press("Return")
    strata.wait(lambda: strata.preview_shows("The password is incorrect."), "a rejected password")
    assert not strata.preview_shows("protected.txt")

    strata.wait(
        lambda: strata.preview().find(states={"editable", "focused"}),
        "the cleared password entry to take focus",
    )
    strata.keyboard.type_text("fixture-password")
    strata.pointer.click(strata.preview().find(role="button", name="Unlock"))
    strata.wait(lambda: strata.preview_shows("protected.txt"), "the unlocked archive listing")
    assert not strata.preview_shows("synthetic archive contents")
    assert sorted(path.name for path in fixture_tree.root.iterdir()) == [filename]
    strata.pointer.click(strata.preview().find(role="button", name="Close preview (Space)"))
    strata.wait(lambda: strata.preview() is None, "the preview to close")


@pytest.mark.parametrize("fixture_tree", ["zip"], indirect=True)
@pytest.mark.preferences(browser_mode="list", tenxer_mode=True, type_to_search=False)
def test_tenxer_password_and_archive_previews_keep_their_keys(strata, fixture_tree):
    filename = next(fixture_tree.root.iterdir()).name
    root = strata.current_directory()
    strata.select_entry_with_keyboard(filename)

    strata.keyboard.press("l")
    strata.wait(
        lambda: strata.preview() and strata.preview().find(states={"editable", "focused"}),
        "l to hand the keys to the password entry",
    )
    strata.keyboard.type_text("hjkl q~")
    assert strata.environment.read_preferences().get("tenxer_mode") == "true", "q was typed"
    assert strata.current_directory() == root
    assert strata.preview().find(states={"editable", "focused"}) is not None
    strata.keyboard.press("shift+Tab")
    strata.wait_for_focused_entry(filename)
    assert strata.preview_shows("Password-protected archive"), "Shift+Tab keeps the prompt"

    strata.keyboard.press("l")
    strata.wait(
        lambda: strata.preview().find(states={"editable", "focused"}),
        "l to return to the password entry",
    )
    strata.keyboard.press("ctrl+a")
    strata.keyboard.type_text("fixture-password")
    strata.keyboard.press("Return")
    strata.wait(lambda: strata.preview_shows("protected.txt"), "the unlocked archive listing")
    for key in ("l", "Return", "j", "k", "space"):
        strata.keyboard.press(key)
    assert strata.preview_shows("protected.txt")
    assert strata.focused_name() is None, "the archive tree keeps the keys"
    assert sorted(path.name for path in fixture_tree.root.iterdir()) == [filename]

    strata.keyboard.press("h")
    strata.wait_for_focused_entry(filename)
    assert strata.preview_shows("protected.txt"), "h at the archive root keeps the preview"
    assert strata.current_directory() == root
    strata.keyboard.press("l")
    strata.wait(lambda: strata.focused_name() is None, "l to re-enter the archive tree")
    strata.keyboard.press("Escape")
    strata.wait(lambda: strata.preview() is None, "Escape to close the archive preview")
    strata.wait_for_focused_entry(filename)
    assert sorted(path.name for path in fixture_tree.root.iterdir()) == [filename]
