"""Read-only drift check for the reviewed capture decoder sources and RSA keys.

This is a review alarm, not an automatic merge or proof of live compatibility.
Run check_capture_keys.py separately for the changing dispatch-key tables.
"""

import hashlib
import json
from pathlib import Path
import re
import sys
import urllib.error
from urllib.parse import quote

from check_capture_keys import download, unique_object


MANIFEST = "docs/capture-upstreams.json"


def sha(value, length):
    if not isinstance(value, str) or not re.fullmatch(f"[0-9a-f]{{{length}}}", value):
        raise ValueError("invalid source fingerprint")
    return value


def normalized_hash(data):
    # Git checkouts may use CRLF on Windows; the reviewed source uses LF.
    return hashlib.sha256(data.replace(b"\r\n", b"\n")).hexdigest()


def fetch_json(url):
    return json.loads(download(url), object_pairs_hook=unique_object)


def require_object(value):
    if not isinstance(value, dict):
        raise ValueError("expected a GitHub metadata object")
    return value


def check(root, manifest):
    if (manifest.get("version") != 1 or not isinstance(manifest.get("sources"), list)
            or not manifest["sources"] or not manifest.get("local_files")):
        raise ValueError("invalid or empty capture review manifest")

    changed = False
    for path, expected in require_object(manifest["local_files"]).items():
        if normalized_hash((root / path).read_bytes()) != sha(expected, 64):
            print(f"REVIEW LOCAL {path}: changed since the recorded semantic review")
            changed = True

    for source in manifest["sources"]:
        source = require_object(source)
        repository = source["repository"]
        if not re.fullmatch(r"[\w.-]+/[\w.-]+", repository):
            raise ValueError("invalid source repository")
        api = f"https://api.github.com/repos/{repository}"
        metadata = require_object(fetch_json(api))
        branch = metadata.get("default_branch")
        if not isinstance(branch, str) or not branch:
            raise ValueError("missing upstream default branch")
        commit = require_object(fetch_json(f"{api}/commits/{quote(branch, safe='')}"))
        revision = sha(commit.get("sha"), 40)
        reviewed = sha(source["reviewed_revision"], 40)
        if not source.get("files"):
            raise ValueError("empty monitored source file set")
        files = require_object(source["files"])
        for expected in files.values():
            sha(expected, 40)
        if revision == reviewed:
            # Git commits are immutable; no file in the reviewed tree can change.
            print(f"OK {repository}@{revision} (reviewed revision unchanged)")
            continue
        # Every file is read at the same immutable revision, even if HEAD moves.
        paths = []
        for path, expected in files.items():
            file = require_object(fetch_json(f"{api}/contents/{quote(path, safe='/')}?ref={revision}"))
            if file.get("type") != "file" or file.get("path") != path:
                raise ValueError(f"unexpected upstream file metadata for {repository}/{path}")
            if sha(file.get("sha"), 40) != sha(expected, 40):
                paths.append(path)
        if paths:
            changed = True
            print(f"REVIEW UPSTREAM {repository}@{revision}")
            for path in paths:
                print(f"  https://github.com/{repository}/blob/{revision}/{path}")
            print(f"  Compare with reviewed revision {reviewed}; semantic review is required.")
        else:
            print(f"OK {repository}@{revision} ({len(source['files'])} reviewed files unchanged)")
    if not changed:
        print("Reviewed decoder sources and RSA files match their recorded fingerprints.")
    return 1 if changed else 0


def main():
    try:
        root = Path(__file__).resolve().parent.parent
        manifest = json.loads((root / MANIFEST).read_text(encoding="utf-8"), object_pairs_hook=unique_object)
        return check(root, require_object(manifest))
    except (OSError, ValueError, KeyError, TypeError, urllib.error.URLError) as error:
        print(f"Capture source verification failed: {error}. Consistency is unverified.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
