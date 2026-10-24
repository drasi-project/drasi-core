# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

import unittest

from check_native_dependencies import forbidden_dependencies


class NativeDependencyTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
