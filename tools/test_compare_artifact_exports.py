import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from compare_artifact_exports import compare, main


def piece(**fields):
    return {
        "setKey": "TestSet", "slotKey": "flower", "rarity": 5, "level": 20,
        "mainStatKey": "hp", "location": "", "lock": False,
        "substats": [{"key": "critRate_", "value": 3.9}, {"key": "def", "value": 19}],
        **fields,
    }


def export(*pieces):
    return {"format": "GOOD", "artifacts": list(pieces)}


def run_cli(actual, expected, *args):
    with tempfile.TemporaryDirectory() as temp:
        paths = [Path(temp, name + ".json") for name in ("actual", "expected")]
        for path, data in zip(paths, (actual, expected)):
            path.write_text(json.dumps(data), encoding="utf-8")
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            status = main([*map(str, paths), *args])
        return status, json.loads(stdout.getvalue()) if stdout.getvalue() else stderr.getvalue()


class ArtifactComparisonTests(unittest.TestCase):
    def test_inventory_and_substat_order_do_not_change_duplicate_counts(self):
        expected = export(piece(lock=True), piece(), piece())
        actual = export(piece(), piece(), piece(lock=True))
        for item in actual["artifacts"]:
            item["substats"].reverse()
        status, report = run_cli(actual, expected)
        self.assertEqual(status, 0)
        self.assertEqual(report["exact_observable_matches"], 3)
        self.assertEqual(report["observable_field_differences"], {})

    def test_identical_length_cannot_mask_duplicate_substitution(self):
        expected = export(piece(), piece(), piece(lock=True))
        actual = export(piece(), piece(lock=True), piece(lock=True))
        status, report = run_cli(actual, expected)
        self.assertEqual(status, 1)
        self.assertEqual(report["expected_only_observable_copies"], 1)
        self.assertEqual(report["actual_only_observable_copies"], 1)
        self.assertEqual(report["identity_matches_with_state_changes"], 1)
        self.assertEqual(report["observable_field_differences"], {"lock": 1})
        self.assertEqual(report["difference_examples"][0]["fields"]["lock"], {"expected": False, "actual": True})

    def test_unequal_duplicate_multiplicity_and_empty_inputs(self):
        for expected, actual, exact, missing, extra in [
            (export(piece(), piece()), export(piece()), 1, 1, 0),
            (export(), export(piece()), 0, 0, 1),
            (export(piece()), export(), 0, 1, 0),
            (export(), export(), 0, 0, 0),
        ]:
            with self.subTest(missing=missing, extra=extra, exact=exact):
                status, report = run_cli(actual, expected)
                self.assertEqual(status, int(bool(missing or extra)))
                self.assertEqual(report["exact_observable_matches"], exact)
                self.assertEqual((report["unpaired_expected"], report["unpaired_actual"]), (missing, extra))

    def test_raw_one_tenth_stat_difference_is_reported_once(self):
        actual = piece()
        actual["substats"][0]["value"] = 3.8
        status, report = run_cli(export(actual), export(piece()))
        self.assertEqual(status, 1)
        self.assertEqual(report["likely_correspondences"], 1)
        self.assertEqual(report["observable_field_differences"], {"substats.critRate_": 1})

    def test_active_and_unactivated_stats_are_distinct(self):
        actual = piece()
        actual["unactivatedSubstats"] = [actual["substats"].pop()]
        report = compare(export(piece()), export(actual))
        self.assertEqual(report["observable_field_differences"], {"substats.def": 1, "unactivatedSubstats.def": 1})

    def test_conditional_details_are_counted_only_where_both_sources_provide_them(self):
        expected, actual = piece(totalRolls=8), piece(totalRolls=7)
        expected["substats"][0].update(rolls=[3.89], initialValue=3.9)
        actual["substats"][0]["initialValue"] = 3.8
        status, report = run_cli(export(actual), export(expected))
        self.assertEqual(status, 1)
        self.assertEqual(report["exact_observable_matches"], 1)
        self.assertEqual(report["detail_field_differences"], {"substats.critRate_.initialValue": 1, "totalRolls": 1})
        self.assertEqual(report["detail_coverage"]["paired_comparable_values"], {"totalRolls": 1, "initialValue": 1, "rolls": 0})
        status, report = run_cli(export(piece()), export(expected))
        self.assertEqual(status, 0)
        self.assertEqual(report["detail_coverage"]["expected_values"]["rolls"], 1)
        self.assertEqual(report["detail_coverage"]["actual_values"]["rolls"], 0)

    def test_indistinguishable_items_find_compatible_detail_assignment(self):
        expected_a, expected_b = piece(totalRolls=7), piece(totalRolls=7)
        expected_a["substats"][0]["initialValue"] = 3.1
        actual_a, actual_b = piece(totalRolls=7), piece(totalRolls=7)
        actual_a["substats"][0]["initialValue"] = 3.2
        # The shared totalRolls-only copy on each side may correspond to either item.
        status, report = run_cli(export(actual_b, actual_a), export(expected_a, expected_b))
        self.assertEqual(status, 0)
        self.assertEqual(report["detail_coverage"]["paired_comparable_values"]["initialValue"], 0)
        actual_b["substats"][0]["initialValue"] = 3.2
        status, report = run_cli(export(actual_b, actual_a), export(expected_a, expected_b))
        self.assertEqual(status, 1)
        self.assertEqual(report["detail_field_differences"], {"substats.critRate_.initialValue": 1})

    def test_shared_extensions_and_legacy_elixer_spelling(self):
        report = compare(export(piece(astralMark=True, elixerCrafted=True)), export(piece(elixirCrafted=True)))
        self.assertEqual(report["exact_observable_matches"], 1)
        self.assertNotIn("astralMark", report["compared_observable_fields"])
        self.assertIn("elixirCrafted", report["compared_observable_fields"])
        self.assertEqual(report["optional_extension_coverage_in_input_files"]["astralMark"]["actual_artifacts"], 0)
        report = compare(export(piece(elixerCrafted=False)), export(piece(elixirCrafted=True)))
        self.assertEqual(report["observable_field_differences"], {"elixirCrafted": 1})

    def test_ambiguous_correspondences_leave_copies_unpaired(self):
        a, b = piece(setKey="First"), piece(setKey="Second")
        report = compare(export(a, b), export(piece(setKey="Third")))
        self.assertEqual(report["likely_correspondences"], 0)
        self.assertEqual((report["unpaired_expected"], report["unpaired_actual"]), (2, 1))

    def test_scope_is_applied_to_both_inputs_and_reports_filtered_counts(self):
        status, report = run_cli(export(piece(), piece(level=0)), export(piece(), piece(rarity=4)),
                                 "--min-rarity", "5", "--min-level", "20")
        self.assertEqual(status, 0)
        self.assertEqual(report["raw_counts"], {"expected": 2, "actual": 2})
        self.assertEqual(report["compared_counts"], {"expected": 1, "actual": 1})

    def test_missing_category_malformed_field_and_conflicting_uids_are_incomparable(self):
        for invalid in [{"format": "GOOD"}, export(piece(lock=None)), {**export(), "metadata": "bad"}]:
            self.assertEqual(run_cli(invalid, export())[0], 2)
        actual, expected = export(), export()
        actual["metadata"], expected["metadata"] = {"uid": "1"}, {"uid": "2"}
        self.assertEqual(run_cli(actual, expected)[0], 2)


if __name__ == "__main__":
    unittest.main()
