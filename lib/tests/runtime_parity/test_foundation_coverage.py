# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

from pathlib import Path
import unittest

from foundation_coverage import production_limit, read_lcov


class CoverageAccountingTests(unittest.TestCase):
    def test_repeated_instantiations_merge_without_double_counting_lines(self):
        filename = str(Path("sample.rs").resolve())
        result = read_lcov(f"SF:{filename}\nDA:1,0\nDA:2,3\nend_of_record\nSF:{filename}\nDA:1,1\nDA:3,0\nend_of_record")
        self.assertEqual(result[filename], {1: 1, 2: 3, 3: 0})

    def test_malformed_line_data_fails_instead_of_producing_a_green_report(self):
        for data in ["DA:1,1", "SF:sample.rs\nDA:0,1", "SF:sample.rs\nDA:1,-1"]:
            with self.assertRaises(ValueError):
                read_lcov(data)

    def test_only_a_trailing_test_section_is_excluded(self):
        filename = str(Path("sample.rs").resolve())
        source = "fn production() {}\n#[cfg(test)]\nmod tests {}\n"
        function = {"name": "5tests6sample", "filenames": [filename], "regions": [[3, 1, 3, 10, 1, 0, 0, 0]]}
        self.assertEqual(production_limit(source, filename, [function]), 2)
        function["name"] = "production_added_after_tests"
        with self.assertRaises(ValueError):
            production_limit(source, filename, [function])

    def test_a_module_without_tests_keeps_all_production_lines(self):
        self.assertEqual(production_limit("fn f() {}\nfn g() {}\n", "sample.rs", []), 3)


if __name__ == "__main__":
    unittest.main()
