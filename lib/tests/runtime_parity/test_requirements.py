# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

from datetime import date
from pathlib import Path
from tempfile import TemporaryDirectory
import sys
import unittest

sys.dont_write_bytecode = True

from check_requirements import check_discovery, check_exemptions, load_ledger, report, FIELDS


class RequirementGuards(unittest.TestCase):
    def setUp(self):
        self.rows = load_ledger(Path(__file__).with_name("requirements.tsv"))

    def test_removing_a_whole_requirement_cannot_shrink_the_gate(self):
        with TemporaryDirectory() as temporary:
            ledger = Path(temporary) / "requirements.tsv"
            ledger.write_text("\n".join("\t".join(row[field] for field in FIELDS) for row in self.rows[1:]))
            with self.assertRaisesRegex(ValueError, "every approved scenario"):
                load_ledger(ledger)

    def test_missing_named_case_fails_even_if_other_tests_are_present(self):
        with self.assertRaisesRegex(ValueError, "missing requirement evidence"):
            check_discovery(self.rows, "integration", "lib-integration-tests", {("unrelated", "passes"): "discovered"})

    def test_partial_contract_is_not_qualified_by_a_passing_test(self):
        with TemporaryDirectory() as temporary:
            directory = Path(temporary)
            row = next(row for row in self.rows if row["state"] == "partial")
            (directory / f"{row['profile']}.executed.tsv").write_text(f"{row['binary']}\t{row['case']}\tok\n")
            outcome = report(self.rows, directory)
            observed = next(result for result in outcome["requirements"] if result["id"] == row["id"])
            self.assertEqual(observed["outcome"], "ok")
            self.assertFalse(observed["satisfied"])
            self.assertFalse(outcome["replacement_qualified"])

    def test_expired_quarantine_cannot_hide_an_ignored_test(self):
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "ignored.tsv"
            path.write_text("package\tbinary\tcase\t-\ttracked gap\towner\t2026-09-01\n")
            with self.assertRaisesRegex(ValueError, "expired test quarantine"):
                check_exemptions(path, date(2026, 9, 26))

    def test_named_cases_cannot_hide_a_failed_or_synthetic_matrix(self):
        with TemporaryDirectory() as temporary:
            directory = Path(temporary)
            row = next(row for row in self.rows if row["state"] == "covered")
            (directory / f"{row['profile']}.executed.tsv").write_text(f"{row['binary']}\t{row['case']}\tok\n")
            (directory / "execution-mode").write_text("actual\n")
            self.assertFalse(report([row], directory)["replacement_qualified"])
            (directory / "matrix.exit-code").write_text("1\n")
            self.assertFalse(report([row], directory)["replacement_qualified"])
            (directory / "matrix.exit-code").write_text("0\n")
            (directory / "execution-mode").write_text("synthetic\n")
            self.assertFalse(report([row], directory)["replacement_qualified"])


if __name__ == "__main__":
    unittest.main()
