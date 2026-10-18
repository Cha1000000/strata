# SPDX-License-Identifier: MIT
"""Settings → Integrations installs, updates and removes catalog providers."""

import json

import pytest

MANIFEST = """schema_version = 1
id = "example"
name = "Example Cloud"
description = "Share links for Example Cloud."
category = "cloud"
maintainer = "octocat"
runtime = "python3"
entry = "provider.py"

[[requirements]]
command = "example-daemon"
packages = { apt = "example-daemon", pacman = "example-daemon" }
"""

PROVIDER = "import sys\nfor line in sys.stdin:\n    pass\n"


@pytest.fixture(autouse=True)
def catalog(test_environment):
    """A one-entry catalog directory that replaces the bundled one."""

    folder = test_environment.root / "catalog" / "example"
    folder.mkdir(parents=True)
    (folder / "catalog.toml").write_text(MANIFEST)
    (folder / "provider.py").write_text(PROVIDER)
    test_environment.extra_variables["STRATA_PROVIDER_CATALOG"] = str(folder.parent)
    return folder


def open_integrations(strata, ready):
    assert strata.window.find(role="button", name="Settings").activate()
    page = strata.wait(
        lambda: strata.window.find(role="button", name="Integrations"),
        "Integrations settings",
    )
    assert page.activate()
    strata.wait(lambda: strata.window.find(role="button", name=ready), ready)
    # Requirement results arrive once per page and rebuild the rows.
    strata.wait(lambda: strata.window.find(name="python3: Available"), "requirement checks")


def reopen_integrations(strata, ready):
    assert strata.window.find(role="button", name="Close settings").activate()
    strata.wait(
        lambda: strata.window.find(role="button", name="Close settings") is None,
        "settings closed",
    )
    open_integrations(strata, ready)


def labels(strata):
    return {" ".join(node.name.split()) for node in strata.window.find_all(role="label")}


def test_catalog_entries_install_update_and_remove(strata, catalog):
    installed = strata.environment.config_home / "strata/providers/example"
    open_integrations(strata, "Install Example Cloud")
    strata.wait(lambda: "Not installed" in labels(strata), "catalog status")
    assert strata.window.find(name="example-daemon: Missing") is not None
    assert strata.window.find(role="button", name="Restart now") is None

    assert strata.window.find(role="button", name="Install Example Cloud").activate()
    strata.wait(lambda: "Installed" in labels(strata), "installed status")
    manifest = json.loads((installed / "provider.json").read_text())
    assert manifest["command"][0].endswith("/python3")
    assert manifest["command"][1] == str(installed / "provider.py")
    assert (installed / "provider.py").read_text() == PROVIDER
    assert (installed / "catalog-install.json").exists()
    strata.wait(
        lambda: strata.window.find(role="button", name="Restart now") is not None,
        "restart notice",
    )

    (catalog / "provider.py").write_text(PROVIDER + "# release 2\n")
    reopen_integrations(strata, "Update Example Cloud")
    strata.wait(lambda: "Update available" in labels(strata), "update offered")
    assert strata.window.find(role="button", name="Update Example Cloud").activate()
    strata.wait(
        lambda: (installed / "provider.py").read_text() == PROVIDER + "# release 2\n",
        "bundled files replaced the installed copy",
    )
    strata.wait(lambda: "Installed" in labels(strata), "updated status")

    (installed / "provider.py").write_text("# local change\n")
    (catalog / "provider.py").write_text(PROVIDER + "# release 3\n")
    reopen_integrations(strata, "Update Example Cloud")
    strata.wait(
        lambda: "Update available · Modified locally" in labels(strata),
        "local edit detected",
    )
    assert strata.window.find(role="button", name="Update Example Cloud").activate()
    confirm = strata.wait(
        lambda: strata.window.find(role="button", name="Confirm replacing Example Cloud"),
        "replacement confirmation",
    )
    assert (installed / "provider.py").read_text() == "# local change\n"
    assert confirm.activate()
    strata.wait(
        lambda: (installed / "provider.py").read_text() == PROVIDER + "# release 3\n",
        "confirmed update replaced the local edit",
    )

    assert strata.window.find(role="button", name="Remove Example Cloud").activate()
    strata.wait(lambda: not installed.exists(), "provider directory removed")
    strata.wait(lambda: "Not installed" in labels(strata), "removed status")

    handmade = installed.parent / "handmade"
    handmade.mkdir()
    (handmade / "provider.json").write_text(
        json.dumps({"version": 1, "id": "handmade", "name": "Handmade", "command": ["/bin/sh"]})
    )
    reopen_integrations(strata, "Remove Handmade")
    assert "Handmade" in labels(strata)
    assert strata.window.find(role="button", name="Remove Handmade").activate()
    confirm = strata.wait(
        lambda: strata.window.find(role="button", name="Confirm removing Handmade"),
        "removal confirmation",
    )
    assert handmade.exists()
    assert confirm.activate()
    strata.wait(lambda: not handmade.exists(), "manual provider removed")
