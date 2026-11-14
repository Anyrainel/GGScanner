"""Read-only check of bundled keys against immutable StarDB and Reliquary revisions."""

import argparse
import base64
import binascii
import json
from pathlib import Path
import re
import subprocess
import sys
import urllib.error
import urllib.request


SOURCE_REPOSITORY = "juliuskreutz/stardb-exporter"
RELIQUARY_REPOSITORY = "IceDynamix/reliquary"
RELIQUARY_KEY_FILE = "src/network/crypto.rs"
GAME_FILES = {
    "gi": ("genshin/keys/gi.json", 0xFFFF),
    "hsr": ("experimental/hsr/keys/hsr.json", 0xFFFFFFFF),
}
MAX_DOWNLOAD_BYTES = 2 * 1024 * 1024


def download(url):
    request = urllib.request.Request(url, headers={"User-Agent": "GOODScanner-key-check"})
    with urllib.request.urlopen(request, timeout=15) as response:
        data = response.read(MAX_DOWNLOAD_BYTES + 1)
    if len(data) > MAX_DOWNLOAD_BYTES:
        raise ValueError("upstream response exceeds the size limit")
    return data


def unique_object(pairs):
    result = {}
    for name, value in pairs:
        if name in result:
            raise ValueError("duplicate JSON entry")
        result[name] = value
    return result


def parse_keys(data, max_identifier):
    document = json.loads(data, object_pairs_hook=unique_object)
    if not isinstance(document, dict) or not document:
        raise ValueError("key table must be a nonempty JSON object")
    decoded = {}
    for identifier, encoded in document.items():
        if not re.fullmatch(r"0|[1-9][0-9]*", identifier) or int(identifier) > max_identifier:
            raise ValueError("key table contains an invalid identifier")
        if not isinstance(encoded, str):
            raise ValueError(f"key {identifier} must be a base64 string")
        try:
            key = base64.b64decode(encoded, validate=True)
        except (ValueError, binascii.Error) as error:
            raise ValueError(f"key {identifier} contains invalid base64") from error
        if len(key) != 4096:
            raise ValueError(f"key {identifier} must decode to 4096 bytes")
        decoded[identifier] = key
    return decoded


def parse_reliquary_keys(data: bytes) -> dict[str, bytes]:
    source = data.decode("utf-8")
    header = r"\blet\s+b64\s*=\s*match\s+version\s*\{"
    if len(re.findall(header, source)) != 1:
        raise ValueError("Reliquary must contain exactly one dispatch-key match table")
    table = re.search(
        header + r"(?P<arms>.*?)\s*_\s*=>\s*\{\s*"
        r'(?:println!\(\s*"[^"\\]*"\s*\);\s*)?'
        r"return\s+None\s*;?\s*\}\s*,?\s*\};",
        source, re.DOTALL,
    )
    if table is None:
        raise ValueError("Reliquary dispatch-key table has an unsupported fallback")

    # Accept only the reviewed base64 string/concat! arms. Unrecognized Rust
    # expressions must block verification, never silently hide a new key.
    literal = r'"[A-Za-z0-9+/=\s]+"'
    concatenated = r"concat\s*!\s*\(\s*" + literal + r"(?:\s*,\s*" + literal + r")*\s*,?\s*\)"
    arm = re.compile(
        r"\s*(?P<id>0|[1-9][0-9]*)\s*=>\s*(?P<value>"
        + literal + "|" + concatenated + r")\s*,",
    )
    arms = table.group("arms")
    encoded = {}
    position = 0
    while arms[position:].strip():
        match = arm.match(arms, position)
        if match is None:
            raise ValueError("Reliquary contains an unsupported dispatch-key arm")
        identifier = match.group("id")
        if identifier in encoded:
            raise ValueError(f"Reliquary contains duplicate dispatch key {identifier}")
        pieces = re.findall(r'"([^"]*)"', match.group("value"))
        encoded[identifier] = re.sub(r"\s+", "", "".join(pieces))
        position = match.end()
    return parse_keys(json.dumps(encoded).encode(), 0xFFFFFFFF)


def merge_keys(first: dict[str, bytes], second: dict[str, bytes]) -> dict[str, bytes]:
    conflicting = sorted(
        (identifier for identifier in first.keys() & second.keys()
         if first[identifier] != second[identifier]), key=int,
    )
    if conflicting:
        raise ValueError(f"StarDB and Reliquary disagree on HSR key IDs={conflicting}")
    return first | second


def resolve_revision(repository: str, branch: str) -> str:
    metadata = json.loads(download(
        f"https://api.github.com/repos/{repository}/commits/{branch}"))
    if not isinstance(metadata, dict):
        raise ValueError(f"{repository} commit metadata must be an object")
    revision = metadata.get("sha")
    if not isinstance(revision, str) or not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError(f"{repository} did not return a valid commit identifier")
    return revision


def compare_keys(local, upstream):
    # Keep historical local keys even if upstream stops distributing them.
    missing = sorted(upstream.keys() - local.keys(), key=int)
    changed = sorted((key for key in upstream.keys() & local.keys()
                      if upstream[key] != local[key]), key=int)
    return missing, changed


def read_local(root, path, revision):
    if revision is None:
        return (root / path).read_bytes()
    result = subprocess.run(
        ["git", "show", f"{revision}:{path}"], cwd=root,
        capture_output=True, check=False,
    )
    if result.returncode:
        raise ValueError(f"cannot read {path} from Git revision {revision}")
    return result.stdout


def check(root, revisions):
    source_revision = resolve_revision(SOURCE_REPOSITORY, "master")
    upstream = {
        game: parse_keys(download(
            f"https://raw.githubusercontent.com/{SOURCE_REPOSITORY}/{source_revision}/keys/{game}.json"
        ), max_identifier)
        for game, (_, max_identifier) in GAME_FILES.items()
    }
    reliquary_revision = resolve_revision(RELIQUARY_REPOSITORY, "main")
    reliquary = parse_reliquary_keys(download(
        f"https://raw.githubusercontent.com/{RELIQUARY_REPOSITORY}/{reliquary_revision}/{RELIQUARY_KEY_FILE}"
    ))
    upstream["hsr"] = merge_keys(upstream["hsr"], reliquary)
    print(f"Checking capture keys against {SOURCE_REPOSITORY}@{source_revision}")
    print(f"HSR also checks {RELIQUARY_REPOSITORY}@{reliquary_revision}/{RELIQUARY_KEY_FILE}")
    outdated = False
    for revision in dict.fromkeys(revisions or [None]):
        label = revision or "working tree"
        for game, (path, max_identifier) in GAME_FILES.items():
            local = parse_keys(read_local(root, path, revision), max_identifier)
            missing, changed = compare_keys(local, upstream[game])
            if missing or changed:
                outdated = True
                print(f"OUTDATED {label} {path}: missing IDs={missing}; changed IDs={changed}")
            else:
                print(f"OK {label} {path} ({len(local)} bundled keys)")
    if outdated:
        print("Update the bundled key tables, validate and commit them before pushing. "
              "Key freshness does not verify inventory protocol compatibility.")
    return 1 if outdated else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", action="append", help="Git tree to check; repeatable")
    args = parser.parse_args()
    try:
        return check(Path(__file__).resolve().parent.parent, args.revision)
    except (OSError, ValueError, urllib.error.URLError) as error:
        print(f"Capture key verification failed: {error}. Push/build must wait until verification succeeds.",
              file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
