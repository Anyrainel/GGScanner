"""Compare Genshin GOOD artifact exports as multisets. Requires scipy (like diff_report.py)."""

import argparse
import json
import math
import sys
from collections import Counter, defaultdict
from pathlib import Path

from good_export_comparison import ExportRow, account_identity, differences, key, load_export, match_groups

REQUIRED = ("setKey", "slotKey", "rarity", "level", "mainStatKey", "location", "lock")
STATES = {"lock", "astralMark", "location"}
DETAIL_FAMILIES = ("totalRolls", "initialValue", "rolls")


def number(value, label):
    if type(value) not in (int, float) or not math.isfinite(value):
        raise ValueError(f"{label}: expected a finite number")
    return value


def artifact(index, raw, optional):
    if not isinstance(raw, dict):
        raise ValueError(f"artifact[{index}]: expected an object")
    fields = {key: raw.get(key) for key in REQUIRED}
    for key, value in fields.items():
        typ = int if key in ("rarity", "level") else bool if key == "lock" else str
        if type(value) is not typ or (typ is str and key != "location" and not value):
            raise ValueError(f"artifact[{index}].{key}: missing or invalid")
    for key in optional:
        value = raw.get(key, raw.get("elixerCrafted", False) if key == "elixirCrafted" else False)
        if type(value) is not bool:
            raise ValueError(f"artifact[{index}].{key}: expected a boolean")
        fields[key] = value
    detail = {}
    if raw.get("totalRolls") is not None:
        detail["totalRolls"] = number(raw["totalRolls"], "totalRolls")
    for group in ("substats", "unactivatedSubstats"):
        stats = raw.get(group, [] if group == "unactivatedSubstats" else None)
        if not isinstance(stats, list):
            raise ValueError(f"artifact[{index}].{group}: expected a list")
        for sub in stats:
            if not isinstance(sub, dict) or not isinstance(sub.get("key"), str) or not sub["key"]:
                raise ValueError(f"artifact[{index}].{group}: missing stat key")
            name = f"{group}.{sub['key']}"
            if name in fields:
                raise ValueError(f"artifact[{index}].{name}: duplicate stat key")
            fields[name] = number(sub.get("value"), name)
            if sub.get("initialValue") is not None:
                detail[name + ".initialValue"] = number(sub["initialValue"], name + ".initialValue")
            if "rolls" in sub and not isinstance(sub["rolls"], list):
                raise ValueError(f"artifact[{index}].{name}.rolls: expected a list")
            if sub.get("rolls"):
                detail[name + ".rolls"] = tuple(number(v, name + ".rolls") for v in sub["rolls"])
    return ExportRow(index, fields, detail)


def family(name):
    if name.endswith(".initialValue"):
        return "initialValue"
    if name.endswith(".rolls"):
        return "rolls"
    return name


def has_differences(report):
    return bool(report["expected_only_observable_copies"] or report["actual_only_observable_copies"]
                or report["detail_field_differences"])


def compare(expected, actual, min_rarity=0, min_level=0, examples=8):
    optional = set()
    extension_coverage = {}
    for name, aliases in (("astralMark", ("astralMark",)), ("elixirCrafted", ("elixirCrafted", "elixerCrafted"))):
        # Some older GOOD producers do not export these extensions at all.
        counts = [sum(isinstance(a, dict) and any(k in a for k in aliases) for a in data["artifacts"])
                  for data in (expected, actual)]
        extension_coverage[name] = dict(zip(("expected_artifacts", "actual_artifacts"), counts))
        if all(counts):
            optional.add(name)

    def prepare(data):
        items = [artifact(i, a, optional) for i, a in enumerate(data["artifacts"])]
        return [a for a in items if a.fields["rarity"] >= min_rarity and a.fields["level"] >= min_level]

    e_items, a_items = prepare(expected), prepare(actual)
    remaining = [{a.index: a for a in items} for items in (e_items, a_items)]
    pairs = match_groups(remaining, lambda a: key(a.fields))
    exact = len(pairs)
    pairs.extend(match_groups(remaining, lambda a: key({k: v for k, v in a.fields.items() if k not in STATES})))
    identity_matches = len(pairs)

    # Only diagnose near matches with strong independent evidence. Ambiguous
    # ties remain separate missing/extra entries; they are not physical IDs.
    candidates = [defaultdict(list), defaultdict(list)]
    for e in remaining[0].values():
        for a in remaining[1].values():
            diffs = differences(e, a)
            nonstate = [k for k in diffs if k not in STATES]
            if len(nonstate) > 2:
                continue
            cost = (len(nonstate), len(diffs))
            candidates[0][e.index].append((cost, a.index))
            candidates[1][a.index].append((cost, e.index))

    def unique_best(options):
        if not options:
            return None
        smallest = min(cost for cost, _ in options)
        best = [index for cost, index in options if cost == smallest]
        return best[0] if len(best) == 1 else None

    for ei, choices in candidates[0].items():
        ai = unique_best(choices)
        if ai is not None and unique_best(candidates[1][ai]) == ei:
            pairs.append((remaining[0].pop(ei), remaining[1].pop(ai)))

    direct = Counter()
    detail_diffs = Counter()
    comparable = Counter()
    samples = []
    for e, a in sorted(pairs, key=lambda pair: pair[0].index):
        fields = differences(e, a)
        shared = e.detail.keys() & a.detail.keys()
        meta = sorted(k for k in shared if e.detail[k] != a.detail[k])
        direct.update(fields)
        detail_diffs.update(meta)
        comparable.update(family(k) for k in shared)
        if len(samples) < examples and (fields or meta):
            values = {k: {"expected": e.fields[k] if k in e.fields else e.detail.get(k),
                          "actual": a.fields[k] if k in a.fields else a.detail.get(k)}
                      for k in fields + meta}
            samples.append({"expected_index": e.index, "actual_index": a.index, "fields": values})

    def detail_counts(names):
        counts = Counter(family(k) for k in names)
        return {key: counts[key] for key in DETAIL_FAMILIES}

    def leftovers(side):
        return [{"index": a.index, **a.fields} for a in list(side.values())[:examples]]

    return {
        "raw_counts": {"expected": len(expected["artifacts"]), "actual": len(actual["artifacts"])},
        "scope": {"min_rarity": min_rarity, "min_level": min_level},
        "compared_counts": {"expected": len(e_items), "actual": len(a_items)},
        "has_items_on_both_sides_in_scope": bool(e_items and a_items),
        "exact_observable_matches": exact,
        "expected_only_observable_copies": len(e_items) - exact,
        "actual_only_observable_copies": len(a_items) - exact,
        "identity_matches_with_state_changes": identity_matches - exact,
        "likely_correspondences": len(pairs) - identity_matches,
        "unpaired_expected": len(remaining[0]), "unpaired_actual": len(remaining[1]),
        "observable_field_differences": dict(sorted(direct.items())),
        "compared_observable_fields": [*REQUIRED, "substats", "unactivatedSubstats", *sorted(optional)],
        "optional_extension_coverage_in_input_files": extension_coverage,
        "detail_coverage": {"expected_values": detail_counts(k for a in e_items for k in a.detail),
                            "actual_values": detail_counts(k for a in a_items for k in a.detail),
                            "paired_comparable_values": {k: comparable[k] for k in DETAIL_FAMILIES}},
        "detail_field_differences": dict(sorted(detail_diffs.items())),
        "difference_examples": samples,
        "unpaired_expected_examples": leftovers(remaining[0]), "unpaired_actual_examples": leftovers(remaining[1]),
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("actual", type=Path, help="GOOD file to check, typically the OCR export")
    parser.add_argument("expected", type=Path, help="reference GOOD file, typically the capture export")
    parser.add_argument("--min-rarity", type=int, default=0)
    parser.add_argument("--min-level", type=int, default=0)
    parser.add_argument("--examples", type=int, default=8)
    args = parser.parse_args(argv)
    if min(args.examples, args.min_rarity, args.min_level) < 0:
        parser.error("minimums and example count must be nonnegative")
    try:
        expected = load_export(args.expected, ("artifacts",))
        actual = load_export(args.actual, ("artifacts",))
        account = account_identity(expected, actual)
        result = compare(expected, actual, args.min_rarity, args.min_level, args.examples)
    except (KeyError, ValueError, OSError) as error:
        print(f"Cannot compare: {error}", file=sys.stderr)
        return 2
    except ImportError as error:
        print(f"Cannot compare duplicate groups: {error}. Install scipy in this Python environment.", file=sys.stderr)
        return 2
    result["inputs"] = {role: {"path": str(path), "source": data.get("source"), "version": data.get("version")}
                        for role, path, data in (("actual", args.actual, actual), ("expected", args.expected, expected))}
    result["account"] = account
    result["interpretation"] = ("Differences describe these two files. Attribution to OCR requires the same "
                                "inventory state and export filters. GOOD has no per-piece IDs; paired differences "
                                "are possible correspondences. totalRolls/initialValue may be inferred by OCR; "
                                "per-roll history is available from capture.")
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return int(has_differences(result))


if __name__ == "__main__":
    sys.exit(main())
