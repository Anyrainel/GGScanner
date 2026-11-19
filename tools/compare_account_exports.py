"""Compare characters, weapons, artifacts and completed achievements in two Genshin GOOD exports."""

import argparse
import json
import sys
from collections import Counter
from pathlib import Path

from compare_artifact_exports import compare as compare_artifacts
from compare_artifact_exports import has_differences as artifact_differences
from good_export_comparison import ExportRow, account_identity, differences, key, load_export, match_groups


CATEGORIES = ("characters", "weapons", "artifacts", "achievements")
CHARACTER = {"key": str, "level": int, "ascension": int, "constellation": int,
             "talent.auto": int, "talent.skill": int, "talent.burst": int}
WEAPON = {"key": str, "level": int, "ascension": int, "refinement": int, "location": str, "lock": bool}


def read_row(category, index, item, required, extensions):
    fields = {}
    for name, typ in {**required, **extensions}.items():
        value = item
        for part in name.split("."):
            value = value.get(part) if isinstance(value, dict) else None
        if name in extensions and value is None:
            continue
        if type(value) is not typ or (typ is str and name != "location" and not value):
            raise ValueError(f"{category}[{index}].{name}: missing or invalid")
        fields[name] = value
    return ExportRow(index, fields)


def compare_equipment_or_characters(category, expected, actual, examples):
    is_character = category == "characters"
    required = CHARACTER if is_character else WEAPON
    extension = "element" if is_character else "rarity"
    counts = [sum(isinstance(item, dict) and item.get(extension) is not None for item in items)
              for items in (expected, actual)]
    # Element is part of the GOOD character model. A known element missing from
    # its counterpart is evidence to report, including when the other export
    # dropped it from all multi-element characters. Weapon rarity is a legacy
    # extension that the current GOOD weapon serializer never writes.
    extensions = {extension: str if is_character else int} if is_character or all(counts) else {}
    sides = [{i: read_row(category, i, item, required, extensions) for i, item in enumerate(items)}
             for items in (expected, actual)]
    exact = len(match_groups(sides, lambda r: key(r.fields)))
    pairs = []
    if is_character:
        # Two Traveler records with the same key can have different active
        # elements. Preserve both, then diagnose a changed or missing element.
        pairs.extend(match_groups(sides, lambda r: (r.fields["key"], r.fields.get("element"))))
    pairs.extend(match_groups(sides, lambda r: r.fields["key"]))
    diff_counts = Counter()
    samples = []
    for e, a in sorted(pairs, key=lambda pair: pair[0].index):
        changed = differences(e, a)
        diff_counts.update(changed)
        if len(samples) < examples:
            samples.append({"key": e.fields["key"], "expected_index": e.index, "actual_index": a.index,
                            "fields": {k: {"expected": e.fields.get(k), "actual": a.fields.get(k)} for k in changed}})

    def unpaired(side):
        return [{"index": row.index, **row.fields} for row in list(side.values())[:examples]]

    return {
        "status": "match" if exact == len(expected) == len(actual) else "differences",
        "compared_fields": [*required, *extensions],
        "optional_field_coverage": {extension: {"expected": counts[0], "actual": counts[1]}},
        "compared_counts": {"expected": len(expected), "actual": len(actual)},
        "exact_matches": exact,
        "expected_only_copies": len(expected) - exact, "actual_only_copies": len(actual) - exact,
        "field_differences_on_same_key_pairs": dict(sorted(diff_counts.items())), "difference_examples": samples,
        "unpaired_expected": len(sides[0]), "unpaired_actual": len(sides[1]),
        "unpaired_expected_examples": unpaired(sides[0]), "unpaired_actual_examples": unpaired(sides[1]),
    }


def compare_achievements(expected, actual, examples):
    for items in (expected, actual):
        if any(type(i) is not int or not 0 < i <= 0xFFFFFFFF for i in items):
            raise ValueError("achievements: expected positive integer IDs")
    e_ids, a_ids = set(expected), set(actual)
    return {
        "status": "match" if e_ids == a_ids else "differences",
        "raw_counts": {"expected": len(expected), "actual": len(actual)},
        "unique_counts": {"expected": len(e_ids), "actual": len(a_ids)}, "matching_ids": len(e_ids & a_ids),
        "expected_only_count": len(e_ids - a_ids), "actual_only_count": len(a_ids - e_ids),
        "expected_only_examples": sorted(e_ids - a_ids)[:examples], "actual_only_examples": sorted(a_ids - e_ids)[:examples],
    }


def compare_account(expected, actual, examples=8):
    account = account_identity(expected, actual)
    sections = {}
    for name in CATEGORIES:
        e, a = expected.get(name), actual.get(name)
        if any(items is not None and not isinstance(items, list) for items in (e, a)):
            raise ValueError(f"{name}: expected an array")
        if e is None or a is None:
            sections[name] = {
                "status": "unavailable",
                "provided_counts": {"expected": len(e) if e is not None else None,
                                    "actual": len(a) if a is not None else None},
            }
        elif name == "artifacts":
            section = compare_artifacts(expected, actual, examples=examples)
            sections[name] = {"status": "differences" if artifact_differences(section) else "match", **section}
        elif name == "achievements":
            sections[name] = compare_achievements(e, a, examples)
        else:
            sections[name] = compare_equipment_or_characters(name, e, a, examples)

    unavailable = [name for name, section in sections.items() if section["status"] == "unavailable"]
    differs = any(section["status"] == "differences" for section in sections.values())
    if differs:
        status = "differences"
    elif len(unavailable) == len(CATEGORIES):
        status = "no_comparable_categories"
    elif unavailable:
        status = "match_on_available_categories"
    else:
        status = "match"
    return {
        "status": status, "account": account, "unavailable_categories": unavailable,
        "all_category_arrays_present": not unavailable, "categories": sections,
        "interpretation": ("Only simultaneous inventories with the same export filters can establish scan parity. "
                           "Character and weapon copies are counted separately, including repeated Traveler keys. "
                           "An unmatched capture Traveler may need a check against the available character roster. "
                           "Achievement IDs are compared as sets. Artifact totalRolls/initialValue can be inferred "
                           "by OCR; per-roll history comes from capture."),
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("actual", type=Path, help="GOOD file to check, typically the OCR export")
    parser.add_argument("expected", type=Path, help="reference GOOD file, typically the capture export")
    parser.add_argument("--examples", type=int, default=8, help="maximum examples per discrepancy section")
    args = parser.parse_args(argv)
    if args.examples < 0:
        parser.error("example count must be nonnegative")
    try:
        expected, actual = load_export(args.expected), load_export(args.actual)
        report = compare_account(expected, actual, args.examples)
    except (ValueError, OSError, ImportError) as error:
        print(f"Cannot compare: {error}", file=sys.stderr)
        return 2
    report["inputs"] = {role: {"path": str(path), "source": data.get("source"), "version": data.get("version")}
                        for role, path, data in (("actual", args.actual, actual), ("expected", args.expected, expected))}
    print(json.dumps(report, indent=2, ensure_ascii=False))
    return {"differences": 1, "no_comparable_categories": 2}.get(report["status"], 0)


if __name__ == "__main__":
    sys.exit(main())
