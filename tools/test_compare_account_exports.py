import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from compare_account_exports import compare_account, main


def character(**fields):
    return {"key": "Traveler", "level": 81, "ascension": 6, "constellation": 4,
            "talent": {"auto": 2, "skill": 2, "burst": 2}, **fields}


def weapon(**fields):
    return {"key": "Rust", "level": 90, "ascension": 6, "refinement": 5, "location": "Aloy", "lock": True, **fields}


def account(**arrays):
    return {"characters": [], "weapons": [], "artifacts": [], "achievements": [], **arrays}


def cli(actual, expected):
    with tempfile.TemporaryDirectory() as temp:
        paths = [Path(temp, name + ".json") for name in ("actual", "expected")]
        for path, data in zip(paths, (actual, expected)):
            path.write_text(json.dumps(data), encoding="utf-8")
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            status = main(list(map(str, paths)))
        return status, json.loads(stdout.getvalue()) if stdout.getvalue() else stderr.getvalue()


class AccountComparisonTests(unittest.TestCase):
    def test_duplicate_traveler_retained_with_independent_element_talent_and_default_twin(self):
        one, twin = character(element="Cryo"), character(level=20, ascension=0, constellation=0)
        other = character(key="Manekina", element="Pyro")
        result = compare_account(account(characters=[one, twin, other]), account(characters=[other, twin, one]))
        self.assertEqual(result["status"], "match")
        self.assertEqual(result["categories"]["characters"]["exact_matches"], 3)
        fix = character(element="Hydro", talent={"auto": 2, "skill": 1, "burst": 2})
        result = compare_account(account(characters=[one, twin, other]), account(characters=[fix, other]))
        section = result["categories"]["characters"]
        self.assertEqual(result["status"], "differences")
        self.assertEqual(section["exact_matches"], 1)
        self.assertEqual(section["field_differences_on_same_key_pairs"], {"element": 1, "talent.skill": 1})
        self.assertEqual(section["unpaired_expected_examples"][0]["level"], 20)

    def test_same_key_elements_take_precedence_for_field_diagnostics(self):
        pyro = character(element="Pyro")
        cryo = character(element="Cryo", level=20, constellation=0, ascension=0)
        result = compare_account(account(characters=[pyro, cryo]),
                                 account(characters=[{**pyro, "element": "Cryo"}, {**cryo, "element": "Pyro"}]))
        diffs = result["categories"]["characters"]["field_differences_on_same_key_pairs"]
        self.assertEqual(diffs, {"ascension": 2, "constellation": 2, "level": 2})

    def test_known_character_element_missing_on_other_side_is_visible(self):
        result = compare_account(account(characters=[character(element="Pyro")]), account(characters=[character()]))
        section = result["categories"]["characters"]
        self.assertEqual(section["status"], "differences")
        self.assertEqual(section["optional_field_coverage"]["element"], {"expected": 1, "actual": 0})
        self.assertEqual(section["difference_examples"][0]["fields"]["element"], {"expected": "Pyro", "actual": None})

    def test_weapon_duplicates_state_location_and_legacy_rarity_coverage(self):
        base, loose, legacy = weapon(), weapon(location="", lock=False), weapon(rarity=4)
        result = compare_account(account(weapons=[base, loose, base]), account(weapons=[loose, legacy, loose]))
        section = result["categories"]["weapons"]
        self.assertEqual(section["exact_matches"], 2)
        self.assertNotIn("rarity", section["compared_fields"])
        self.assertEqual(section["field_differences_on_same_key_pairs"], {"location": 1, "lock": 1})
        result = compare_account(account(weapons=[weapon(rarity=3)]), account(weapons=[legacy]))
        self.assertEqual(result["categories"]["weapons"]["field_differences_on_same_key_pairs"], {"rarity": 1})

    def test_authoritative_empty_achievements_missing_sources_and_duplicates(self):
        expected, actual = account(achievements=[111, 222]), account(achievements=[222, 111, 111])
        status, report = cli(actual, expected)
        self.assertEqual((status, report["status"]), (0, "match"))
        self.assertEqual(report["categories"]["achievements"]["raw_counts"]["actual"], 3)
        actual["achievements"] = []
        status, report = cli(actual, expected)
        self.assertEqual(status, 1)
        self.assertEqual(report["categories"]["achievements"]["expected_only_examples"], [111, 222])
        del actual["achievements"]
        status, report = cli(actual, expected)
        self.assertEqual((status, report["status"]), (0, "match_on_available_categories"))
        self.assertEqual(report["categories"]["achievements"]["provided_counts"], {"expected": 2, "actual": None})
        self.assertFalse(report["all_category_arrays_present"])
        self.assertEqual(cli({"characters": []}, {"weapons": []})[0], 2)

    def test_invalid_shapes_uid_mismatch_and_artifact_delegation(self):
        for data in [account(weapons=""), account(characters=[character(talent={})]), account(achievements=[True])]:
            self.assertEqual(cli(data, account())[0], 2)
        self.assertEqual(cli(account(metadata={"uid": "1"}), account(metadata={"uid": "2"}))[0], 2)
        art = {"setKey": "Test", "slotKey": "flower", "rarity": 5, "level": 20, "mainStatKey": "hp",
               "substats": [], "location": "", "lock": False}
        status, report = cli(account(artifacts=[art]), account(artifacts=[art, art]))
        self.assertEqual(status, 1)
        self.assertEqual(report["categories"]["artifacts"]["expected_only_observable_copies"], 1)


if __name__ == "__main__":
    unittest.main()
