"""Validate Genshin continuous capture and fingerprint each requested inventory multiset."""

import argparse
import hashlib
import json
import sys
from pathlib import Path


def summarize_category(data, name):
    items = data[name]
    if not isinstance(items, list):
        raise ValueError(f"continuous capture must have an explicit {name} array")
    if name == "achievements":
        if not all(type(item) is int and 0 < item <= 0xFFFFFFFF for item in items):
            raise ValueError("continuous capture contains an invalid achievement ID")
    elif not all(isinstance(item, dict) for item in items):
        raise ValueError(f"continuous capture contains an invalid {name} record")
    # Keep duplicates and every field. JSON key order and inventory order may change.
    records = sorted(json.dumps(item, sort_keys=True, separators=(",", ":"), allow_nan=False)
                     for item in items)
    return {"count": len(items), "sha256": hashlib.sha256("\n".join(records).encode("utf-8")).hexdigest()}


def summarize(snapshot, export_path=None, *, account=False):
    session = snapshot["sessionId"]
    revision = snapshot["revision"]
    updated_at = snapshot["updatedAt"]
    data = snapshot["data"]
    if not isinstance(session, str) or not session.strip():
        raise ValueError("continuous capture has no login session")
    if type(revision) is not int or revision < 1 or type(updated_at) is not int or updated_at < 1:
        raise ValueError("continuous capture has invalid revision or update time")
    if data.get("format") != "GOOD" or data.get("version") != 3:
        raise ValueError("expected a Genshin GOOD v3 capture")
    uid = data["metadata"]["uid"]
    if not isinstance(uid, str) or not uid.isascii() or not uid.isdigit() or int(uid) == 0:
        raise ValueError("continuous capture has no verified game UID")
    names = ["characters", "weapons", "artifacts"] if account else ["artifacts"]
    if account and data.get("achievements") is not None:
        names.append("achievements")
    categories = {name: summarize_category(data, name) for name in names}
    if export_path:
        export_path.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
    return {"sessionId": session, "revision": revision, "updatedAt": updated_at, "uid": uid,
            "artifactCount": categories["artifacts"]["count"],
            "artifactSha256": categories["artifacts"]["sha256"], "categories": categories}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    parser.add_argument("--export", type=Path)
    parser.add_argument("--account", action="store_true")
    args = parser.parse_args()
    try:
        summary = summarize(json.loads(args.snapshot.read_text(encoding="utf-8-sig")),
                            args.export, account=args.account)
    except (AttributeError, KeyError, TypeError, ValueError, OSError) as error:
        print(f"Invalid continuous capture: {error}", file=sys.stderr)
        return 2
    print(json.dumps(summary))
    return 0


if __name__ == "__main__":
    sys.exit(main())
