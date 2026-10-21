#!/usr/bin/env python3
import importlib.util
import sys
import unittest
from pathlib import Path


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "release_check", Path(__file__).with_name("check-sdk-release.py")
)
release_check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_check)


class ReleaseClosureTests(unittest.TestCase):
    def test_abi_change_cannot_stay_on_the_same_package_line(self):
        self.assertTrue(release_check.abi_requires_package_line_change(
            "0.12.0", "0.14.0", "0.12.1", "0.15.0"
        ))
        self.assertFalse(release_check.abi_requires_package_line_change(
            "0.12.0", "0.14.0", "0.13.0", "0.15.0"
        ))
        self.assertFalse(release_check.abi_requires_package_line_change(
            "0.11.3", "0.14.0", "0.12.0", "0.14.0"
        ))

    def test_sdk_line_is_not_the_abi_or_patch_version(self):
        self.assertEqual(release_check.line("0.12.0"), release_check.line("0.12.9"))
        self.assertNotEqual(release_check.line("0.12.0"), release_check.line("0.11.3"))
        self.assertNotEqual(release_check.line("0.12.0"), release_check.line("0.14.0"))
        with self.assertRaises(ValueError):
            release_check.line("not-a-version")

    def test_transitive_wrappers_and_unpublished_dynamic_plugins_are_included(self):
        packages = {
            name: {"manifest": name, "dynamic": dynamic, "publish": publish}
            for name, dynamic, publish in [
                ("drasi-source-probe", True, False),
                ("wrapper", False, True),
                ("outer-wrapper", False, True),
                ("test-only", False, True),
                ("unrelated", False, True),
            ]
        }
        workspace = {"dependencies": {"sdk": {"package": "drasi-plugin-sdk", "version": "0.12.0"}}}
        manifests = {
            "drasi-source-probe": {"dependencies": {"sdk": {"workspace": True}}},
            "wrapper": {"dependencies": {"drasi-source-probe": "0.1.0"}},
            "outer-wrapper": {"build-dependencies": {"wrapper": "0.1.0"}},
            "test-only": {"dev-dependencies": {"wrapper": "0.1.0"}},
            "unrelated": {},
        }
        self.assertEqual(
            release_check.release_closure(manifests, workspace, packages),
            release_check.SDK_PACKAGES | {"drasi-source-probe", "wrapper", "outer-wrapper"},
        )

    def test_target_specific_dependencies_and_aliases(self):
        manifest = {
            "target": {"cfg(unix)": {"dependencies": {"alias": {"package": "drasi-plugin-sdk", "version": "0.12.0"}}}}
        }
        name, value, kind = next(release_check.dependencies(manifest))
        self.assertEqual(kind, "dependencies")
        self.assertEqual(
            release_check.resolved_dependency(name, value, {}),
            ("drasi-plugin-sdk", "0.12.0"),
        )

    def test_additional_required_library_release_expands_closure(self):
        packages = {
            name: {"manifest": name, "dynamic": False, "publish": True}
            for name in ("drasi-lib", "state-store", "wrapper")
        }
        manifests = {
            "drasi-lib": {},
            "state-store": {"dependencies": {"drasi-lib": "0.9.3"}},
            "wrapper": {"dependencies": {"state-store": "0.2.8"}},
        }
        self.assertEqual(
            release_check.release_closure(manifests, {}, packages, {"drasi-lib"}),
            {"drasi-lib", "state-store", "wrapper"},
        )


if __name__ == "__main__":
    unittest.main()
