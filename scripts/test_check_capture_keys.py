import base64
import contextlib
import io
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

import check_capture_keys as checker


def table(identifier="1", byte=0):
    return json.dumps({identifier: base64.b64encode(bytes([byte]) * 4096).decode()}).encode()


def rust_table(entries=None, concat_identifiers=()):
    entries = json.loads(table()) if entries is None else entries
    arms = []
    for identifier, encoded in entries.items():
        if identifier in concat_identifiers:
            value = f'concat!(\n"{encoded[:123]}",\n"{encoded[123:]}",\n)'
        else:
            value = f'"{encoded}"'
        arms.append(f"{identifier} => {value},")
    return (
        "fn keys(version: u32) {\nlet b64 = match version {\n"
        + "\n".join(arms)
        + '\n_ => { println!("{version}"); return None }\n};\n}'
    ).encode()


def source_download(urls, stardb_hsr=None, reliquary=None):
    stardb_sha = "a" * 40
    reliquary_sha = "b" * 40
    responses = {
        f"https://api.github.com/repos/{checker.SOURCE_REPOSITORY}/commits/master":
            json.dumps({"sha": stardb_sha}).encode(),
        f"https://api.github.com/repos/{checker.RELIQUARY_REPOSITORY}/commits/main":
            json.dumps({"sha": reliquary_sha}).encode(),
        f"https://raw.githubusercontent.com/{checker.SOURCE_REPOSITORY}/{stardb_sha}/keys/gi.json":
            table(),
        f"https://raw.githubusercontent.com/{checker.SOURCE_REPOSITORY}/{stardb_sha}/keys/hsr.json":
            table() if stardb_hsr is None else stardb_hsr,
        f"https://raw.githubusercontent.com/{checker.RELIQUARY_REPOSITORY}/{reliquary_sha}/{checker.RELIQUARY_KEY_FILE}":
            rust_table() if reliquary is None else reliquary,
    }

    def download(url):
        urls.append(url)
        return responses[url]

    return download


class KeyCheckTests(unittest.TestCase):
    def test_missing_changed_and_retained_historical_keys(self):
        self.assertEqual(checker.compare_keys({"1": b"a", "2": b"old"},
                                             {"1": b"b", "3": b"new"}), (["3"], ["1"]))
        self.assertEqual(checker.compare_keys({"1": b"a", "2": b"old"}, {"1": b"a"}), ([], []))

    def test_validates_identifiers_encoding_length_and_duplicates(self):
        for data in [b"[]", b"{}", b'{"1": "bad!"}', b'{"1": "AAAA"}',
                     b'{"1": 4}', table("65536"), table("01"), b'{"1": "", "1": ""}']:
            with self.subTest(data=data[:35]), self.assertRaises(ValueError):
                checker.parse_keys(data, 0xFFFF)
        self.assertEqual(len(checker.parse_keys(table("2920436228"), 0xFFFFFFFF)["2920436228"]), 4096)

    def test_checks_outgoing_commit_instead_of_working_tree(self):
        urls = []
        with patch.object(checker, "download", side_effect=source_download(urls)), \
                patch.object(checker.subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 0, stdout=table(), stderr=b"")) as git, \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(checker.check(Path("."), ["outgoing", "outgoing"]), 0)
        self.assertEqual(git.call_count, 2)
        self.assertTrue(all(call.args[0][2].startswith("outgoing:") for call in git.call_args_list))
        self.assertEqual(sum("/commits/" in url for url in urls), 2)
        self.assertTrue(all(
            f"/{'a' * 40}/keys/" in url or f"/{'b' * 40}/{checker.RELIQUARY_KEY_FILE}" in url
            for url in urls if "raw.githubusercontent.com" in url
        ))
        self.assertIn(f"{checker.SOURCE_REPOSITORY}@{'a' * 40}", output.getvalue())
        self.assertIn(f"{checker.RELIQUARY_REPOSITORY}@{'b' * 40}", output.getvalue())

    def test_outdated_keys_block_without_writing_files(self):
        with patch.object(checker, "download", side_effect=source_download(
                [], stardb_hsr=table("2"))), \
                patch.object(checker, "read_local", return_value=table()), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(checker.check(Path("."), []), 1)
        self.assertIn("missing IDs=['2']", output.getvalue())

    def test_reliquary_only_current_47_key_blocks_the_previous_false_pass(self):
        current = json.loads(table()) | json.loads(table("2766937619", byte=47))
        with patch.object(checker, "download", side_effect=source_download(
                [], reliquary=rust_table(current))), \
                patch.object(checker, "read_local", return_value=table()), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(checker.check(Path("."), []), 1)
        self.assertIn("missing IDs=['2766937619']", output.getvalue())

    def test_parses_32_rust_arms_including_concatenated_and_current_keys(self):
        entries = {
            str(identifier): base64.b64encode(bytes([identifier]) * 4096).decode()
            for identifier in range(1, 31)
        } | json.loads(table("2920436228", byte=46)) | json.loads(table("2766937619", byte=47))
        parsed = checker.parse_reliquary_keys(rust_table(entries, concat_identifiers=("1",)))
        self.assertEqual(len(parsed), 32)
        self.assertEqual(parsed["1"], bytes([1]) * 4096)
        self.assertEqual(parsed["2766937619"], bytes([47]) * 4096)

    def test_rust_parser_rejects_duplicates_invalid_values_and_unknown_expressions(self):
        valid = rust_table()
        malformed = [
            b"", valid + valid,
            valid.replace(b'1 => "', b'4294967296 => "'),
            valid.replace(b'1 => "', b'01 => "'),
            valid.replace(b'1 => "', b'1 => "!'),
            rust_table({"1": "AAAA"}),
            valid.replace(b"_ =>", valid[valid.index(b"1 =>"):valid.index(b"_ =>")] + b"_ =>"),
            valid.replace(b"1 =>", b'2 => include_str!("new-key"), 1 =>'),
            valid.replace(b"1 =>", b"2 => concat!(), 1 =>"),
            valid.replace(b"return None", b"return lookup_other_key()"),
            valid.replace(b"};", b'3 => "AAAA", };'),
        ]
        for data in malformed:
            with self.subTest(data=data[:75]), self.assertRaises(ValueError):
                checker.parse_reliquary_keys(data)

    def test_conflicting_sources_fail_before_inspecting_outgoing_keys(self):
        with patch.object(checker, "download", side_effect=source_download(
                [], reliquary=rust_table(json.loads(table(byte=1))))), \
                patch.object(checker, "read_local") as read_local:
            with self.assertRaisesRegex(ValueError, "disagree on HSR key IDs=\\['1'\\]"):
                checker.check(Path("."), [])
        read_local.assert_not_called()

    def test_identical_sources_merge_and_historical_keys_remain_allowed(self):
        self.assertEqual(checker.merge_keys({"1": b"a"}, {"1": b"a", "2": b"b"}),
                         {"1": b"a", "2": b"b"})

    def test_invalid_source_metadata_cannot_select_mutable_downloads(self):
        for metadata in [[], {}, {"sha": "main"}, {"sha": "z" * 40}]:
            with self.subTest(metadata=metadata), \
                    patch.object(checker, "download", return_value=json.dumps(metadata).encode()), \
                    self.assertRaises(ValueError):
                checker.resolve_revision(checker.RELIQUARY_REPOSITORY, "main")

    def test_unavailable_reliquary_blocks_even_when_stardb_is_current(self):
        downloads = source_download([])

        def fail_reliquary(url):
            if checker.RELIQUARY_REPOSITORY in url:
                raise OSError("Reliquary unavailable")
            return downloads(url)

        with patch.object(checker.sys, "argv", ["check_capture_keys.py"]), \
                patch.object(checker, "download", side_effect=fail_reliquary), \
                contextlib.redirect_stderr(io.StringIO()) as output:
            self.assertEqual(checker.main(), 1)
        self.assertIn("Reliquary unavailable", output.getvalue())

    def test_unavailable_upstream_blocks(self):
        with patch.object(checker.sys, "argv", ["check_capture_keys.py"]), \
                patch.object(checker, "download", side_effect=OSError("offline")), \
                contextlib.redirect_stderr(io.StringIO()) as output:
            self.assertEqual(checker.main(), 1)
        self.assertIn("verification failed", output.getvalue())


if __name__ == "__main__":
    unittest.main()
