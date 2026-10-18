from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import hashlib
import json
from pathlib import Path
import socket
import sqlite3
import tempfile
import threading
import time
import unittest

from fastapi.testclient import TestClient
import httpx
import uvicorn

from lof_daemon.app import create_app
from lof_daemon.backup import backup
from lof_daemon.config import Config, InstanceLock
from lof_daemon.history import changed_chars
from lof_daemon.models import DocumentMutation

TOKEN = "test-only-" + "x" * 40
HEADERS = {"Authorization": f"Bearer {TOKEN}"}
BASE = "/v1/libraries/library"


def cleanup(temporary):
    # 테스트가 직접 만든 폴더만 확인한 뒤 정리한다.
    directory = Path(temporary.name).resolve()
    if not directory.name.startswith("lof-daemon-test-"):
        raise RuntimeError("테스트 임시 폴더 경로가 올바르지 않습니다.")
    for path in directory.rglob("*"):
        if not path.resolve().is_relative_to(directory):
            raise RuntimeError("임시 폴더 밖을 가리키는 파일이 있습니다.")
    temporary.cleanup()


class DaemonTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="lof-daemon-test-")
        self.config = Config(Path(self.temporary.name), TOKEN, history_tick_seconds=3600)
        self.app = create_app(self.config)
        self.client = TestClient(self.app)
        self.client.__enter__()
        self.client.headers.update(HEADERS)
        self.now = 1700000000.0
        self.app.state.store.clock = lambda: self.now
        self.assertEqual(self.client.post("/v1/libraries", json={"id": "library", "name": "시험 서재"}).status_code, 201)
        self.serial = 0

    def tearDown(self):
        self.client.__exit__(None, None, None)
        cleanup(self.temporary)

    def put(self, body, version=0, **extra):
        self.serial += 1
        payload = {"title": "원고", "body": body, "device_id": "pc-a", "mutation_id": f"mutation-{self.serial}",
                   "expected_version": version, **extra}
        return self.client.put(f"{BASE}/documents/doc", json=payload)

    def policy(self, **values):
        current = self.client.get(f"{BASE}/settings").json()
        policy = {**current["history"], **values}
        response = self.client.put(f"{BASE}/settings", json={"expected_version": current["settings_version"], "history": policy})
        self.assertEqual(response.status_code, 200, response.text)
        return response.json()

    def histories(self):
        response = self.client.get(f"{BASE}/documents/doc/history")
        self.assertEqual(response.status_code, 200, response.text)
        return response.json()["entries"]

    def test_public_status_and_authentication(self):
        for route in ("/status", "/healthz"):
            response = self.client.get(route, headers={"Authorization": ""})
            self.assertEqual(response.status_code, 200)
            self.assertEqual(response.json()["status"], "ok")
            self.assertNotIn(TOKEN, response.text)
        self.assertEqual(self.client.get("/v1/libraries", headers={"Authorization": ""}).status_code, 401)
        self.assertEqual(self.client.get("/v1/libraries", headers={"Authorization": "Bearer wrong-token"}).status_code, 401)
        self.assertEqual(self.client.get("/openapi.json").status_code, 200)

    def test_versions_retries_and_noop(self):
        first = self.put("초안", mutation_id="stable")
        self.assertEqual(first.status_code, 201, first.text)
        cursor = self.client.get(f"{BASE}/snapshot").json()["cursor"]
        retry = self.put("초안", mutation_id="stable")
        self.assertEqual(retry.json(), first.json())
        same = self.put("초안", version=0)
        self.assertFalse(same.json()["changed"])
        self.assertEqual(same.json()["document"]["version"], 1)
        self.assertEqual(self.client.get(f"{BASE}/snapshot").json()["cursor"], cursor)
        reused = self.put("다른 내용", mutation_id="stable")
        self.assertEqual(reused.status_code, 409)
        self.assertEqual(reused.json()["error"], "mutation_reused")
        self.assertEqual(self.client.get(f"{BASE}/documents/doc", headers={"If-None-Match": '"1"'}).status_code, 304)

    def test_conflicting_edit_is_preserved_and_can_be_resolved(self):
        self.put("초안")
        self.put("서버 수정", 1)
        conflict = self.put("오프라인 수정", 1, device_id="pc-b", mutation_id="offline")
        self.assertEqual(conflict.status_code, 409)
        conflict_id = conflict.json()["conflict_id"]
        self.assertEqual(self.put("오프라인 수정", 1, device_id="pc-b", mutation_id="offline").json(), conflict.json())
        saved = self.client.get(f"{BASE}/conflicts/{conflict_id}").json()
        self.assertEqual(saved["incoming"]["body"], "오프라인 수정")
        self.assertEqual(self.client.get(f"{BASE}/documents/doc").json()["body"], "서버 수정")
        request = {"expected_version": 2, "device_id": "pc-b", "mutation_id": "resolve", "choice": "incoming"}
        response = self.client.post(f"{BASE}/conflicts/{conflict_id}/resolve", json=request)
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(self.client.get(f"{BASE}/documents/doc").json()["body"], "오프라인 수정")
        self.assertEqual(self.client.post(f"{BASE}/conflicts/{conflict_id}/resolve", json=request).json(), response.json())
        self.assertEqual(self.client.get(f"{BASE}/conflicts").json()["entries"], [])

    def test_two_devices_cannot_overwrite_same_version(self):
        self.put("초안")
        store = self.app.state.store
        requests = [DocumentMutation(title="원고", body=body, expected_version=1,
                                     device_id=f"pc-{i}", mutation_id="concurrent")
                    for i, body in enumerate(("첫 번째 변경", "두 번째 변경"))]
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda payload: store.mutate_document("library", "doc", payload), requests))
        self.assertEqual(sorted(result[0] for result in results), [200, 409])
        self.assertEqual(len(store.conflicts("library", "", 100)["entries"]), 1)

    def test_history_requires_elapsed_time_and_character_changes(self):
        self.policy(min_minutes=1, min_changed_chars=5)
        self.put("초안")
        self.now += 10
        self.put("초안" + "가" * 10, 1)
        self.assertEqual(self.app.state.history.tick(), 0)
        self.now += 51
        self.assertEqual(self.app.state.history.tick(), 1)
        self.assertEqual(len(self.histories()), 2)
        self.now += 1000
        self.assertEqual(self.app.state.history.tick(), 0)
        self.put("초안" + "가" * 10 + "나", 2)
        self.assertEqual(self.app.state.history.tick(), 0)
        self.put("초안" + "가" * 10 + "나다라마바", 3)
        self.assertEqual(self.app.state.history.tick(), 1)
        self.assertEqual(len(self.histories()), 3)

    def test_same_length_edit_and_revert(self):
        self.policy(min_minutes=1, min_changed_chars=2)
        self.put("가나다")
        self.now += 61
        self.put("가마바", 1)
        self.assertEqual(self.app.state.history.tick(), 1)
        self.put("전혀 다른 원고", 2)
        self.put("가마바", 3)
        self.now += 61
        self.assertEqual(self.app.state.history.tick(), 0)
        self.assertEqual(len(self.histories()), 2)

    def test_manual_history_deduplicates_and_restore_preserves_current(self):
        self.put("처음 원고")
        original = self.histories()[0]["id"]
        capture = self.client.post(f"{BASE}/documents/doc/history", json={"expected_version": 1, "device_id": "pc-a"})
        self.assertFalse(capture.json()["created"])
        self.assertEqual(len(self.histories()), 1)
        self.assertTrue(self.histories()[0]["protected"])
        self.now += 1
        self.put("복원 전의 원고", 1)
        request = {"expected_version": 2, "device_id": "pc-a", "mutation_id": "restore"}
        result = self.client.post(f"{BASE}/documents/doc/history/{original}/restore", json=request)
        self.assertEqual(result.status_code, 200, result.text)
        self.assertEqual(result.json()["document"]["version"], 3)
        self.assertEqual(self.client.get(f"{BASE}/documents/doc").json()["body"], "처음 원고")
        histories = self.histories()
        self.assertEqual(len(histories), 3)
        previous = next(row for row in histories if row["reason"] == "before_restore")
        saved = self.client.get(f"{BASE}/documents/doc/history/{previous['id']}").json()
        self.assertEqual(saved["snapshot"]["body"], "복원 전의 원고")
        self.assertEqual(self.client.post(f"{BASE}/documents/doc/history/{original}/restore", json=request).json(), result.json())

    def test_shared_settings_reject_stale_writes_and_apply_new_threshold(self):
        policy = self.policy(min_minutes=1, min_changed_chars=100)
        self.put("가")
        self.now += 61
        self.put("가나다", 1)
        self.assertEqual(self.app.state.history.tick(), 0)
        self.policy(min_changed_chars=2)
        self.assertEqual(self.app.state.history.tick(), 1)
        stale = self.client.put(f"{BASE}/settings", json={"expected_version": policy["settings_version"], "history": policy["history"]})
        self.assertEqual(stale.status_code, 409)
        self.assertEqual(self.client.get(f"{BASE}/settings").json()["history"]["min_changed_chars"], 2)

    def test_smart_pruning_keeps_fixed_buckets_and_manual_history(self):
        self.now = 120000.0
        self.policy(min_minutes=1, min_changed_chars=1, smart={"enabled": True, "keep_recent_minutes": 60, "bucket_minutes": 10})
        self.put("a")
        for version in range(1, 6):
            self.now = 120000.0 + version * 60
            self.put("a" * (version + 1), version)
            self.assertEqual(self.app.state.history.tick(), 1)
            if version == 2:
                self.client.post(f"{BASE}/documents/doc/history", json={"expected_version": version + 1, "device_id": "pc-a"})
        self.assertEqual(len(self.histories()), 6)
        self.now += 7200
        self.assertEqual(self.app.state.history.maintain(), 4)
        histories = self.histories()
        self.assertEqual(len(histories), 2)
        self.assertEqual({row["reason"] for row in histories}, {"automatic", "manual"})
        self.assertEqual(self.app.state.history.maintain(), 0)

    def test_retention_keeps_latest_and_protected_unless_opted_in(self):
        self.policy(retention_days=1)
        self.put("처음")
        self.client.post(f"{BASE}/documents/doc/history", json={"expected_version": 1, "device_id": "pc-a"})
        self.now += 100
        self.put("두 번째", 1)
        self.client.post(f"{BASE}/documents/doc/history", json={"expected_version": 2, "device_id": "pc-a"})
        self.now += 86400 * 3
        self.assertEqual(self.app.state.history.maintain(), 0)
        self.policy(expire_protected=True)
        self.assertEqual(self.app.state.history.maintain(), 1)
        self.assertEqual(len(self.histories()), 1)

    def test_change_feed_pagination_and_future_cursor(self):
        cursor = self.client.get(f"{BASE}/snapshot").json()["cursor"]
        self.put("첫 원고")
        first = self.client.get(f"{BASE}/changes", params={"after": cursor, "limit": 1}).json()
        self.assertTrue(first["has_more"])
        second = self.client.get(f"{BASE}/changes", params={"after": first["cursor"], "limit": 1}).json()
        self.assertGreater(second["events"][0]["id"], first["events"][0]["id"])
        self.assertFalse(second["has_more"])
        self.assertEqual(self.client.get(f"{BASE}/changes", params={"after": 999999}).status_code, 409)
        self.assertEqual(self.client.get(f"{BASE}/events", params={"after": 999999}).status_code, 409)
        self.assertEqual(self.client.get(f"{BASE}/events", headers={"Last-Event-ID": "invalid"}).status_code, 422)

    def test_assets_are_hash_addressed_and_noop_uploads_do_not_emit(self):
        data = "첨부 자료".encode()
        sha256 = hashlib.sha256(data).hexdigest()
        route = f"{BASE}/assets/{sha256}"
        self.assertEqual(self.client.put(route, content=b"wrong").status_code, 422)
        self.assertTrue(self.client.put(route, content=data).json()["created"])
        cursor = self.client.get(f"{BASE}/snapshot").json()["cursor"]
        self.assertFalse(self.client.put(route, content=data).json()["created"])
        self.assertEqual(self.client.get(f"{BASE}/snapshot").json()["cursor"], cursor)
        self.assertEqual(self.client.get(route).content, data)
        self.assertEqual(self.client.get(route, headers={"If-None-Match": f'"{sha256}"'}).status_code, 304)

    def test_persistence_on_restart(self):
        first = self.put("재시작 보존", mutation_id="persisted")
        self.client.__exit__(None, None, None)
        self.app = create_app(self.config)
        self.client = TestClient(self.app)
        self.client.__enter__()
        self.client.headers.update(HEADERS)
        self.assertEqual(self.client.get(f"{BASE}/documents/doc").json()["body"], "재시작 보존")
        self.assertEqual(self.put("재시작 보존", mutation_id="persisted").json(), first.json())
        self.assertEqual(len(self.histories()), 1)

    def test_deleted_content_is_preserved_and_can_be_restored(self):
        self.put("삭제해도 남길 원고")
        original = self.histories()[0]["id"]
        self.put("", 1, deleted=True)
        deleted = self.client.get(f"{BASE}/documents/doc").json()
        self.assertTrue(deleted["deleted"])
        self.assertEqual(deleted["body"], "삭제해도 남길 원고")
        result = self.client.post(f"{BASE}/documents/doc/history/{original}/restore",
                                  json={"expected_version": 2, "device_id": "pc-a", "mutation_id": "undelete"})
        self.assertFalse(result.json()["document"]["deleted"])

    def test_single_process_lock(self):
        with self.assertRaises(RuntimeError):
            InstanceLock(self.config.data_dir)

    def test_request_size_limit_and_validation(self):
        self.assertEqual(self.client.put(f"{BASE}/assets/" + "0" * 64,
                                        content=b"x", headers={"Content-Length": str(9 * 1024 * 1024)}).status_code, 413)
        self.assertEqual(self.client.post("/v1/libraries", json={"id": "../other", "name": "잘못된 경로"}).status_code, 422)
        self.assertEqual(self.client.put(f"{BASE}/settings", json={"expected_version": 1,
                                        "history": {"min_minutes": 0}}).status_code, 422)

    def test_write_receipts_do_not_store_hidden_history_copies(self):
        self.put("보관 조건을 충족하지 않은 본문")
        self.put("보관 조건을 충족하지 않은 수정본", 1)
        with self.app.state.store.transaction() as db:
            for row in db.execute("SELECT response FROM mutations"):
                receipt = json.loads(row["response"])
                self.assertNotIn("body", receipt["document"])
        self.assertEqual(len(self.histories()), 1)

    def test_status_reports_unhealthy_storage(self):
        self.app.state.store.healthy = lambda: False
        self.assertEqual(self.client.get("/healthz").status_code, 503)
        status = self.client.get("/status")
        self.assertEqual(status.status_code, 503)
        self.assertEqual(status.json()["database"], "error")

    def test_character_count_uses_unicode_characters_and_replacements(self):
        self.assertEqual(changed_chars({"body": "가😀나", "deleted": False},
                                       {"body": "가🦊나", "deleted": False}), 2)
        self.assertEqual(changed_chars({"body": "가나다", "deleted": False},
                                       {"body": "가나다", "deleted": False}), 0)

    def test_running_database_backup_is_complete_and_never_overwrites(self):
        self.put("백업할 원고")
        destination = self.config.data_dir / "backup.sqlite"
        backup(self.config.data_dir, destination)
        with closing(sqlite3.connect(destination)) as db:
            self.assertEqual(db.execute("SELECT body FROM documents").fetchone()[0], "백업할 원고")
            self.assertEqual(db.execute("PRAGMA integrity_check").fetchone()[0], "ok")
        with self.assertRaises(FileExistsError):
            backup(self.config.data_dir, destination)

    def test_chunked_upload_cannot_bypass_request_limit(self):
        def chunks():
            yield b"x" * (4 * 1024 * 1024)
            yield b"y" * (4 * 1024 * 1024 + 1)

        response = self.client.put(f"{BASE}/assets/" + "0" * 64, content=chunks())
        self.assertEqual(response.status_code, 413, response.text)


class LiveEventsTests(unittest.TestCase):
    def test_live_sse_and_resume(self):
        temporary = tempfile.TemporaryDirectory(prefix="lof-daemon-test-")
        app = create_app(Config(Path(temporary.name), TOKEN, heartbeat_seconds=0.1))
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        port = listener.getsockname()[1]
        server = uvicorn.Server(uvicorn.Config(app, log_level="error"))
        thread = threading.Thread(target=lambda: server.run(sockets=[listener]), daemon=True)
        thread.start()
        try:
            deadline = time.monotonic() + 5
            while not server.started and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(server.started)
            with httpx.Client(base_url=f"http://127.0.0.1:{port}", headers=HEADERS, timeout=5) as client:
                self.assertEqual(client.get("/status").json()["status"], "ok")
                client.post("/v1/libraries", json={"id": "library", "name": "실시간 서재"})
                cursor = client.get(f"{BASE}/snapshot").json()["cursor"]
                with client.stream("GET", f"{BASE}/events", params={"after": cursor}) as response:
                    self.assertEqual(response.status_code, 200)
                    client.put(f"{BASE}/documents/doc", json={"title": "원고", "body": "실시간 변경",
                               "device_id": "pc", "mutation_id": "live", "expected_version": 0})
                    lines = response.iter_lines()
                    event = None
                    for line in lines:
                        if line.startswith("data: "):
                            event = json.loads(line[6:])
                            break
                    self.assertEqual(event["type"], "document.updated")
                    self.assertEqual(event["data"]["id"], "doc")
                with client.stream("GET", f"{BASE}/events", headers={"Last-Event-ID": str(event["id"])}) as response:
                    for line in response.iter_lines():
                        if line.startswith("data: "):
                            replayed = json.loads(line[6:])
                            break
                    self.assertEqual(replayed["type"], "history.created")
                    self.assertGreater(replayed["id"], event["id"])
        finally:
            server.should_exit = True
            thread.join(timeout=5)
            listener.close()
            if thread.is_alive():
                raise RuntimeError("검증용 서버가 종료되지 않았습니다.")
            cleanup(temporary)


if __name__ == "__main__":
    unittest.main()
