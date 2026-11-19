"""Exercise the live parity command against a loopback-only fake GGScanner."""

import copy
import json
import re
import shutil
import subprocess
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from urllib.parse import parse_qs, urlsplit


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "check-genshin-artifact-parity.ps1"
JOB_ID = "00000000-0000-4000-8000-000000000001"
MOCK_UID = "100000001"
OTHER_UID = "100000002"
CATEGORIES = ("characters", "weapons", "artifacts", "achievements")


def capture():
    flower = {
        "setKey": "GladiatorsFinale", "slotKey": "flower", "mainStatKey": "hp",
        "rarity": 5, "level": 0, "location": "", "lock": False,
        "substats": [
            {"key": "critRate_", "value": 3.9},
            {"key": "eleMas", "value": 21},
            {"key": "atk", "value": 19},
        ],
        "unactivatedSubstats": [{"key": "critDMG_", "value": 7.8}],
    }
    plume = dict(copy.deepcopy(flower), slotKey="plume", mainStatKey="atk")
    amber = {"key": "Amber", "level": 80, "ascension": 6, "constellation": 0,
             "talent": {"auto": 6, "skill": 7, "burst": 8}}
    bow = {"key": "FavoniusWarbow", "level": 80, "ascension": 6,
           "refinement": 1, "location": "", "lock": False}
    sword = dict(bow, key="CoolSteel", location="Kaeya")
    return {
        "sessionId": "00000000-0000-4000-8000-000000000010",
        "revision": 12, "updatedAt": 1793900000000,
        "data": {
            "format": "GOOD", "version": 3, "source": "yas-GOODScanner",
            "metadata": {"uid": MOCK_UID},
            "characters": [amber, dict(copy.deepcopy(amber), key="Kaeya")],
            "weapons": [bow, copy.deepcopy(bow), sword],
            "achievements": [80001, 80002],
            "artifacts": [flower, copy.deepcopy(flower), plume],
        },
    }


class ManagerHandler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, value):
        payload = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        path = urlsplit(self.path)
        manager = self.server
        manager.requests.append(("GET", path.path))
        if path.path == "/continuous":
            snapshot = manager.before if not manager.posts else manager.after
            return self.reply(200, snapshot)
        if path.path == "/health":
            return self.reply(200, {"status": "ok", "enabled": True,
                                    "busy": False, "gameAlive": True})
        if path.path == "/status":
            status = {"state": "idle"} if not manager.posts else {
                "state": "completed", "jobId": manager.status_job, "summary": manager.result["summary"],
            }
            return self.reply(200, status)
        if manager.posts and parse_qs(path.query) == {"jobId": [JOB_ID]}:
            if path.path == "/result":
                return self.reply(200, manager.result)
            category = path.path.lstrip("/")
            if category in manager.ocr:
                return self.reply(200, manager.ocr[category])
        manager.unexpected.append(path.path)
        self.reply(404, {"error": "Unexpected endpoint or jobId"})

    def do_POST(self):
        manager = self.server
        manager.requests.append(("POST", self.path))
        if self.path != "/scan":
            manager.unexpected.append(self.path)
            return self.reply(404, {"error": "Unexpected endpoint"})
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        manager.posts.append(request)
        self.reply(202, {
            "jobId": JOB_ID, "targets": {k: request.get(k, False) for k in CATEGORIES},
            "artifactMode": "all", "artifactLimit": None,
        })


@unittest.skipUnless(shutil.which("pwsh"), "PowerShell 7 is required")
class LiveParityTests(unittest.TestCase):
    def exercise(self, before, after, ocr, *, account=False, failed_phase=None, wrong_job=False,
                 status="match", code=0):
        if account:
            requested = [k for k in CATEGORIES if k != "achievements" or k in before["data"]]
        else:
            requested = ["artifacts"]
            ocr = {"artifacts": ocr}
        with TemporaryDirectory() as tmp, ThreadingHTTPServer(("127.0.0.1", 0), ManagerHandler) as manager:
            manager.before, manager.after, manager.ocr = before, after, ocr
            manager.posts, manager.requests, manager.unexpected = [], [], []
            manager.status_job = "00000000-0000-4000-8000-000000000099" if wrong_job else JOB_ID
            errors = int(failed_phase is not None)
            summary = {"total": len(requested), "success": len(requested) - errors,
                       "already_correct": 0, "not_found": 0, "errors": errors,
                       "aborted": 0}
            manager.result = {"results": [{"id": k, "status": "ui_error" if k == failed_phase else "success"}
                                          for k in requested], "summary": summary}
            thread = threading.Thread(target=manager.serve_forever, kwargs={"poll_interval": 0.05})
            thread.start()
            try:
                result = subprocess.run(
                    ["pwsh", "-NoProfile", "-File", str(SCRIPT), "-Port", str(manager.server_port),
                     "-OutputRoot", tmp, "-TimeoutMinutes", "2", *(["-Account"] if account else [])],
                    cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace",
                    timeout=40, check=False,
                )
            finally:
                manager.shutdown()
                thread.join()

            console = result.stdout + result.stderr
            self.assertFalse(any(uid in console for uid in (MOCK_UID, OTHER_UID)),
                             "Console included a mock account identifier")
            self.assertIsNone(re.search(r'"(characters|weapons|artifacts|achievements)"\s*:\s*\[', console),
                              "Console dumped account records")
            self.assertEqual(result.returncode, code, f"Unexpected exit code for {status}")
            self.assertEqual(manager.unexpected, [])
            self.assertEqual(len(manager.posts), 1)
            request = manager.posts[0]
            self.assertEqual({k for k in CATEGORIES if request.get(k)}, set(requested))
            self.assertEqual(request.get("artifactMode", "all"), "all")
            self.assertIsNone(request.get("artifactLimit"))
            runs = [p for p in Path(tmp).iterdir() if p.is_dir()]
            self.assertEqual(len(runs), 1)
            run = runs[0]

            def saved(name):
                return json.loads((run / name).read_text(encoding="utf-8-sig"))

            metadata = saved("metadata.json")
            self.assertEqual(metadata["status"], status)
            if account and "achievements" not in requested:
                self.assertEqual(metadata["coverage"]["achievements"], "missing_capture_baseline")
            self.assertTrue(saved("capture-before.json") == before, "Saved capture baseline differs")
            if wrong_job:
                self.assertNotIn(("GET", "/result"), manager.requests)
                self.assertFalse((run / "ocr.json").exists())
            else:
                output = saved("ocr.json")
                self.assertEqual(output["jobId"], JOB_ID)
                self.assertEqual(output["job"], manager.result)
                for k in requested:
                    if k != failed_phase:
                        self.assertTrue(output[k] == ocr[k], f"Saved {k} array differs")
            for k in CATEGORIES:
                expected_reads = int(k in requested and not wrong_job and k != failed_phase)
                self.assertEqual(manager.requests.count(("GET", "/" + k)), expected_reads, k)
            if not wrong_job and not failed_phase:
                self.assertTrue(saved("capture-after.json") == after, "Saved final capture differs")
                snapshots = [i for i, request in enumerate(manager.requests)
                             if request == ("GET", "/continuous")]
                self.assertEqual(len(snapshots), 2)
                self.assertLess(snapshots[0], manager.requests.index(("POST", "/scan")))
                for k in requested:
                    self.assertGreater(snapshots[1], manager.requests.index(("GET", "/" + k)))
            report = (run / "report.txt").read_text(encoding="utf-8-sig").strip()
            self.assertTrue(report)
            if status == "failed":
                self.assertTrue(report == metadata["error"], "Missing failure diagnostic")
                self.assertFalse(any(uid in report for uid in (MOCK_UID, OTHER_UID)),
                                 "Failure diagnostic included a mock account identifier")

    def test_matching_exports_and_duplicate_copies(self):
        before = capture()
        self.exercise(before, copy.deepcopy(before), copy.deepcopy(before["data"]["artifacts"]))

    def test_small_ocr_arrays_keep_their_shape(self):
        for size in (0, 1):
            with self.subTest(size=size):
                before = capture()
                before["data"]["artifacts"] = before["data"]["artifacts"][:size]
                self.exercise(before, copy.deepcopy(before), before["data"]["artifacts"])

    def test_revision_and_order_can_change_with_identical_artifacts(self):
        before = capture()
        after = copy.deepcopy(before)
        after["revision"] += 1
        after["updatedAt"] += 900
        after["data"]["achievements"].append(42)
        after["data"]["artifacts"].reverse()
        self.exercise(before, after, after["data"]["artifacts"])

    def test_ocr_difference_is_a_valid_report(self):
        before = capture()
        ocr = copy.deepcopy(before["data"]["artifacts"])
        ocr[0]["lock"] = True
        self.exercise(before, copy.deepcopy(before), ocr, status="differences", code=1)

    def test_session_uid_and_inventory_changes_invalidate_comparison(self):
        for change in ("session", "uid", "multiplicity", "lock"):
            with self.subTest(change=change):
                before = capture()
                after = copy.deepcopy(before)
                after["revision"] += 1
                after["updatedAt"] += 900
                if change == "session":
                    after["sessionId"] = "00000000-0000-4000-8000-000000000020"
                elif change == "uid":
                    after["data"]["metadata"]["uid"] = OTHER_UID
                elif change == "multiplicity":
                    # A,A,B -> A,B,B preserves both list length and the set of types.
                    after["data"]["artifacts"][1] = copy.deepcopy(after["data"]["artifacts"][2])
                else:
                    after["data"]["artifacts"][0]["lock"] = True
                self.exercise(before, after, before["data"]["artifacts"], status="failed", code=2)

    def test_failed_ocr_phase_cannot_compare_cached_artifacts(self):
        before = capture()
        self.exercise(before, copy.deepcopy(before), before["data"]["artifacts"],
                      failed_phase="artifacts", status="failed", code=2)

    def test_account_matches_all_categories_regardless_of_order(self):
        before = capture()
        after = copy.deepcopy(before)
        after["revision"] += 1
        for category in CATEGORIES:
            after["data"][category].reverse()
        self.exercise(before, after, {k: before["data"][k] for k in CATEGORIES}, account=True)

    def test_account_empty_achievements_are_covered(self):
        before = capture()
        before["data"]["achievements"] = []
        self.exercise(before, copy.deepcopy(before), before["data"], account=True)

    def test_account_missing_achievements_omits_phase_and_reports_incomplete(self):
        before = capture()
        del before["data"]["achievements"]
        self.exercise(before, copy.deepcopy(before), before["data"], account=True, status="incomplete", code=2)

    def test_ocr_differences_in_each_account_category(self):
        for category in CATEGORIES:
            with self.subTest(category=category):
                before = capture()
                ocr = copy.deepcopy(before["data"])
                ocr[category].pop(0)
                self.exercise(before, copy.deepcopy(before), ocr, account=True, status="differences", code=1)

    def test_packet_changes_in_each_account_category_invalidate_comparison(self):
        for category in CATEGORIES:
            with self.subTest(category=category):
                before = capture()
                after = copy.deepcopy(before)
                after["revision"] += 1
                items = after["data"][category]
                if category in ("weapons", "artifacts"):
                    # Same length and same distinct records, but A,A,B becomes A,B,B.
                    items[1] = copy.deepcopy(items[2])
                else:
                    items.pop(0)
                self.exercise(before, after, before["data"], account=True, status="failed", code=2)

    def test_account_wrong_job_and_failed_character_phase(self):
        for options in ({"wrong_job": True}, {"failed_phase": "characters"}):
            with self.subTest(options=options):
                before = capture()
                self.exercise(before, copy.deepcopy(before), before["data"], account=True,
                              status="failed", code=2, **options)


if __name__ == "__main__":
    unittest.main()
