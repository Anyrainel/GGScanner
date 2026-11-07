import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import check_capture_upstreams as checker


class CaptureSourceCheckTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "crypto.rs").write_bytes(b"reviewed\r\n")
        self.manifest = {
            "version": 1,
            "local_files": {"crypto.rs": checker.normalized_hash(b"reviewed\n")},
            "sources": [{"repository": "owner/repo", "reviewed_revision": "a" * 40,
                         "files": {"src/crypto.rs": "b" * 40}}],
        }

    def metadata(self, url):
        if "/contents/" in url:
            self.assertTrue(url.endswith("?ref=" + "c" * 40))
            return {"type": "file", "path": "src/crypto.rs", "sha": "b" * 40}
        if "/commits/" in url:
            self.assertIn("/commits/development", url)
            return {"sha": "c" * 40}
        return {"default_branch": "development"}

    def run_check(self, fetch=None):
        with patch.object(checker, "fetch_json", side_effect=fetch or self.metadata), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            code = checker.check(self.root, self.manifest)
        return code, output.getvalue()

    def test_new_commits_with_unchanged_monitored_sources_pass_and_crlf_is_allowed(self):
        code, output = self.run_check()
        self.assertEqual(code, 0)
        self.assertIn("1 reviewed files unchanged", output)

    def test_unchanged_immutable_revision_needs_no_file_download(self):
        self.manifest["sources"][0]["reviewed_revision"] = "c" * 40

        def unchanged(url):
            self.assertNotIn("/contents/", url)
            return self.metadata(url)

        code, output = self.run_check(unchanged)
        self.assertEqual(code, 0)
        self.assertIn("reviewed revision unchanged", output)

    def test_changed_upstream_requires_semantic_review_without_automatic_writes(self):
        def changed(url):
            metadata = self.metadata(url)
            if "/contents/" in url:
                metadata["sha"] = "d" * 40
            return metadata

        before = (self.root / "crypto.rs").read_bytes()
        code, output = self.run_check(changed)
        self.assertEqual(code, 1)
        self.assertIn("REVIEW UPSTREAM", output)
        self.assertIn("semantic review is required", output)
        self.assertEqual((self.root / "crypto.rs").read_bytes(), before)

    def test_changed_local_decoder_is_not_silently_treated_as_reviewed(self):
        (self.root / "crypto.rs").write_bytes(b"new algorithm\n")
        code, output = self.run_check()
        self.assertEqual(code, 1)
        self.assertIn("REVIEW LOCAL crypto.rs", output)

    def test_invalid_remote_response_cannot_report_consistency(self):
        for malformed in [[], {}, {"type": "dir", "path": "src/crypto.rs", "sha": "b" * 40},
                          {"type": "file", "path": "src/crypto.rs", "sha": "not-a-sha"}]:
            def invalid(url):
                return malformed if "/contents/" in url else self.metadata(url)
            with self.subTest(response=malformed), self.assertRaises(ValueError):
                self.run_check(invalid)

    def test_unavailable_upstream_is_explicitly_unverified(self):
        with patch.object(checker.Path, "read_text", return_value=json.dumps(self.manifest)), \
                patch.object(checker, "check", side_effect=OSError("offline")), \
                contextlib.redirect_stderr(io.StringIO()) as output:
            self.assertEqual(checker.main(), 1)
        self.assertIn("Consistency is unverified", output.getvalue())


if __name__ == "__main__":
    unittest.main()
