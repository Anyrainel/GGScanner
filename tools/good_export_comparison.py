"""Shared helpers for offline GOOD inventory comparisons."""

import json
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class ExportRow:
    index: int
    fields: dict
    detail: dict = field(default_factory=dict)


def load_export(path, required_arrays=()):
    with Path(path).open(encoding="utf-8-sig") as stream:
        data = json.load(stream)
    if not isinstance(data, dict) or data.get("format", "GOOD") != "GOOD":
        raise ValueError(f"{path}: expected a GOOD export object")
    if data.get("metadata") is not None and not isinstance(data["metadata"], dict):
        raise ValueError(f"{path}: expected metadata to be an object")
    for name in required_arrays:
        if not isinstance(data.get(name), list):
            raise ValueError(f"{path}: expected an explicit {name} array")
    return data


def account_identity(expected, actual):
    eu, au = [(data.get("metadata") or {}).get("uid") for data in (expected, actual)]
    if eu and au and eu != au:
        raise ValueError("different verified account UIDs; no comparable inventory")
    return "matching supplied UIDs" if au and eu else "UID unavailable on one or both exports"


def key(fields):
    return tuple(sorted(fields.items()))


def differences(e, a):
    missing = object()
    return sorted(k for k in e.fields.keys() | a.fields.keys()
                  if e.fields.get(k, missing) != a.fields.get(k, missing))


def assignment(left, right):
    """Minimize observable differences, then shared detail conflicts, then coverage gaps."""
    if len(left) == len(right) == 1:
        return [(left[0], right[0])]

    def signature(a):
        return key(a.fields), key(a.detail)

    left, right = sorted(left, key=signature), sorted(right, key=signature)
    if signature(left[0]) == signature(left[-1]) and signature(right[0]) == signature(right[-1]):
        return zip(left, right)

    from scipy.optimize import linear_sum_assignment

    # One higher-priority difference outweighs all lower-priority differences
    # in the entire assignment, even for unequal numbers of duplicate copies.
    weight = 1 + 2 * min(len(left), len(right)) * max(1, max(len(a.detail) for a in left + right))

    def cost(e, a):
        shared = e.detail.keys() & a.detail.keys()
        conflicts = sum(e.detail[k] != a.detail[k] for k in shared)
        return (len(differences(e, a)) * weight + conflicts) * weight + len(e.detail.keys() ^ a.detail.keys())

    matrix = [[cost(e, a) for a in right] for e in left]
    rows, cols = linear_sum_assignment(matrix)
    return [(left[r], right[c]) for r, c in zip(rows, cols)]


def match_groups(remaining, group_key):
    """Pair each copy at most once across groups with the same caller-supplied key."""
    groups = []
    for side in remaining:
        buckets = defaultdict(list)
        for row in side.values():
            buckets[group_key(row)].append(row)
        groups.append(buckets)
    pairs = []
    for group in groups[0].keys() & groups[1].keys():
        for e, a in assignment(groups[0][group], groups[1][group]):
            pairs.append((e, a))
            del remaining[0][e.index], remaining[1][a.index]
    return pairs
