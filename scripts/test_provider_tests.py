# SPDX-License-Identifier: MIT

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import provider_tests  # noqa: E402

MANIFEST = 'schema_version = 1\nid = "{id}"\nname = "X"\ndescription = "Y"\ncategory = "other"\nmaintainer = "octocat"\nruntime = "{runtime}"\nentry = "provider.py"\n'


def provider(root: Path, id: str, *, passing: bool, runtime: str = sys.executable) -> Path:
    directory = root / id
    (directory / "tests").mkdir(parents=True)
    (directory / "catalog.toml").write_text(MANIFEST.format(id=id, runtime=runtime))
    (directory / "provider.py").write_text("print('hi')\n")
    outcome = "pass" if passing else "self.fail('broken')"
    (directory / "tests" / "test_provider.py").write_text(
        f"import unittest\n\nclass T(unittest.TestCase):\n    def test_it(self):\n        {outcome}\n"
    )
    return directory


class ProviderTestsTest(unittest.TestCase):
    def test_discovers_providers_and_reports_failures(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            provider(root, "alpha", passing=True)
            (root / "alpha" / "tests" / "apt-packages.txt").write_text("python3-gi\n# comment\ndbus-daemon\n")
            provider(root, "_template", passing=False)
            (root / ".hidden").mkdir()
            self.assertEqual(
                [path.name for path in provider_tests.provider_directories(root)], ["alpha"]
            )
            self.assertEqual(provider_tests.apt_packages(root), ["python3-gi", "dbus-daemon"])
            self.assertEqual(provider_tests.main(["--root", str(root)]), 0)
            provider(root, "beta", passing=False)
            self.assertEqual(provider_tests.main(["--root", str(root)]), 1)

    def test_a_provider_without_tests_only_needs_to_compile(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = provider(root, "gamma", passing=True)
            (directory / "tests" / "test_provider.py").unlink()
            (directory / "tests").rmdir()
            self.assertEqual(provider_tests.main(["--root", str(root)]), 0)
            (directory / "provider.py").write_text("def broken(:\n")
            self.assertEqual(provider_tests.main(["--root", str(root)]), 1)

    def test_an_empty_catalog_passes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            self.assertEqual(provider_tests.main(["--root", str(Path(temporary) / "none")]), 0)


if __name__ == "__main__":
    unittest.main()
