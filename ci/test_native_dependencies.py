# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

import unittest

from check_native_dependencies import QUERY_PLUGIN, dependency_selections, forbidden_dependencies


class NativeDependencyTests(unittest.TestCase):
    def test_native_context_is_checked_without_workspace_feature_unification(self):
        selections = list(dependency_selections(["native-fixture"]))
        self.assertIn(("drasi-computation-plugin-sdk", []), selections)
        self.assertIn(("drasi-computation-plugin-sdk", ["otel-context"]), selections)
        self.assertIn(("native-fixture", ["dynamic-plugin"]), selections)

    def test_model_and_ast_packages_are_not_the_engine(self):
        self.assertEqual(
            forbidden_dependencies("drasi-core-types v1\ndrasi-query-ast v1\n"),
            set(),
        )

    def test_direct_and_transitive_engine_packages_are_rejected(self):
        self.assertEqual(
            forbidden_dependencies(
                "drasi-computation-sink-sse v1\n"
                "drasi-lib v1 (/checkout/lib)\n"
                "drasi-core v1 (*)\n"
                "drasi-query-cypher v1\n"
            ),
            {"drasi-lib", "drasi-core", "drasi-query-cypher"},
        )

    def test_only_query_plugin_can_depend_on_the_query_engine(self):
        tree = "drasi-core v1\ndrasi-query-cypher v1\ndrasi-lib v1\ndrasi-plugin-sdk v1\n"
        self.assertEqual(
            forbidden_dependencies(tree, QUERY_PLUGIN), {"drasi-lib", "drasi-plugin-sdk"}
        )
        self.assertEqual(
            forbidden_dependencies(tree, "another-plugin"),
            {"drasi-core", "drasi-query-cypher", "drasi-lib", "drasi-plugin-sdk"},
        )


if __name__ == "__main__":
    unittest.main()
