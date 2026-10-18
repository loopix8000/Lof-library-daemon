from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import sqlite3
import threading
import time
import uuid

from .errors import DomainError


def encode(value):
    try:
        return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)
    except (ValueError, TypeError) as error:
        raise DomainError(422, "invalid_json", "유효한 JSON 값만 저장할 수 있습니다.") from error


def digest(value):
    return hashlib.sha256(encode(value).encode("utf-8")).hexdigest()


def content_of(document):
    return {key: document[key] for key in ("kind", "title", "body", "metadata", "deleted")}


class Store:
    def __init__(self, directory: Path, clock=time.time):
        directory.mkdir(parents=True, exist_ok=True)
        self.path = directory / "library.sqlite"
        self.clock = clock
        self.lock = threading.RLock()
        self.db = sqlite3.connect(self.path, isolation_level=None, check_same_thread=False, timeout=5)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=FULL")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.execute("PRAGMA busy_timeout=5000")
        version = self.db.execute("PRAGMA user_version").fetchone()[0]
        if version not in (0, 1, 2):
            self.db.close()
            raise RuntimeError("이 데몬에서 지원하지 않는 데이터베이스 버전입니다.")
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS libraries (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, policy TEXT NOT NULL,
                settings_version INTEGER NOT NULL, created_at REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS documents (
                library_id TEXT NOT NULL REFERENCES libraries(id), id TEXT NOT NULL,
                kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
                metadata TEXT NOT NULL, deleted INTEGER NOT NULL,
                version INTEGER NOT NULL, content_hash TEXT NOT NULL,
                device_id TEXT NOT NULL, updated_at REAL NOT NULL,
                PRIMARY KEY (library_id, id)
            );
            CREATE TABLE IF NOT EXISTS changes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                library_id TEXT NOT NULL REFERENCES libraries(id),
                type TEXT NOT NULL, payload TEXT NOT NULL, created_at REAL NOT NULL
            );
            CREATE INDEX IF NOT EXISTS changes_library ON changes(library_id, id);
            CREATE TABLE IF NOT EXISTS mutations (
                library_id TEXT NOT NULL REFERENCES libraries(id),
                device_id TEXT NOT NULL, mutation_id TEXT NOT NULL,
                fingerprint TEXT NOT NULL, status INTEGER NOT NULL, response TEXT NOT NULL,
                PRIMARY KEY (library_id, device_id, mutation_id)
            );
            CREATE TABLE IF NOT EXISTS conflicts (
                id TEXT PRIMARY KEY, library_id TEXT NOT NULL, document_id TEXT NOT NULL,
                base_version INTEGER NOT NULL, server_version INTEGER NOT NULL,
                device_id TEXT NOT NULL, incoming TEXT NOT NULL, created_at REAL NOT NULL,
                resolved_at REAL,
                FOREIGN KEY (library_id, document_id) REFERENCES documents(library_id, id)
            );
            CREATE INDEX IF NOT EXISTS conflicts_library ON conflicts(library_id, resolved_at);
            CREATE TABLE IF NOT EXISTS assets (
                library_id TEXT NOT NULL REFERENCES libraries(id), sha256 TEXT NOT NULL,
                media_type TEXT NOT NULL, data BLOB NOT NULL, created_at REAL NOT NULL,
                PRIMARY KEY (library_id, sha256)
            );
            CREATE TABLE IF NOT EXISTS histories (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                library_id TEXT NOT NULL, document_id TEXT NOT NULL,
                document_version INTEGER NOT NULL, content_hash TEXT NOT NULL,
                snapshot TEXT NOT NULL, reason TEXT NOT NULL, protected INTEGER NOT NULL,
                created_at REAL NOT NULL,
                FOREIGN KEY (library_id, document_id) REFERENCES documents(library_id, id)
            );
            CREATE INDEX IF NOT EXISTS history_document ON histories(library_id, document_id, seq);
            CREATE TABLE IF NOT EXISTS history_state (
                library_id TEXT NOT NULL, document_id TEXT NOT NULL,
                snapshot TEXT NOT NULL, content_hash TEXT NOT NULL, recorded_at REAL NOT NULL,
                checked_version INTEGER NOT NULL, checked_policy INTEGER NOT NULL,
                changed_chars INTEGER NOT NULL,
                PRIMARY KEY (library_id, document_id),
                FOREIGN KEY (library_id, document_id) REFERENCES documents(library_id, id)
            );
            CREATE TABLE IF NOT EXISTS server (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS devices (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, role TEXT NOT NULL, platform TEXT NOT NULL,
                token_hash TEXT UNIQUE NOT NULL, created_at REAL NOT NULL, last_seen_at REAL
            );
            CREATE TABLE IF NOT EXISTS pairing_codes (
                id TEXT PRIMARY KEY, code_hash TEXT UNIQUE NOT NULL, name TEXT NOT NULL, role TEXT NOT NULL,
                created_by TEXT NOT NULL, created_at REAL NOT NULL, expires_at REAL NOT NULL,
                used_at REAL, device_id TEXT
            );
            CREATE TABLE IF NOT EXISTS sessions (
                token_hash TEXT PRIMARY KEY, created_at REAL NOT NULL, expires_at REAL NOT NULL
            );
            PRAGMA user_version=2;
        """)

    @contextmanager
    def transaction(self, write=False):
        with self.lock:
            self.db.execute("BEGIN IMMEDIATE" if write else "BEGIN")
            try:
                yield self.db
                self.db.commit()
            except BaseException:
                self.db.rollback()
                raise

    def close(self):
        with self.lock:
            self.db.close()

    def healthy(self):
        with self.transaction() as db:
            return db.execute("SELECT 1").fetchone()[0] == 1

    @staticmethod
    def require_library(db, library_id):
        row = db.execute("SELECT * FROM libraries WHERE id=?", (library_id,)).fetchone()
        if row is None:
            raise DomainError(404, "library_not_found", "서재를 찾을 수 없습니다.")
        return row

    @staticmethod
    def document_row(db, library_id, document_id):
        return db.execute("SELECT * FROM documents WHERE library_id=? AND id=?", (library_id, document_id)).fetchone()

    @staticmethod
    def document(row):
        return {
            "id": row["id"], "library_id": row["library_id"], "kind": row["kind"],
            "title": row["title"], "body": row["body"], "metadata": json.loads(row["metadata"]),
            "deleted": bool(row["deleted"]), "version": row["version"],
            "sha256": row["content_hash"], "device_id": row["device_id"], "updated_at": row["updated_at"],
        }

    @staticmethod
    def library(row):
        return {"id": row["id"], "name": row["name"], "history": json.loads(row["policy"]),
                "settings_version": row["settings_version"], "created_at": row["created_at"]}

    def event(self, db, library_id, kind, payload):
        db.execute("INSERT INTO changes(library_id,type,payload,created_at) VALUES(?,?,?,?)",
                   (library_id, kind, encode(payload), self.clock()))

    def libraries(self):
        with self.transaction() as db:
            return [self.library(row) for row in db.execute("SELECT * FROM libraries ORDER BY id")]

    def create_library(self, request):
        with self.transaction(True) as db:
            if db.execute("SELECT 1 FROM libraries WHERE id=?", (request.id,)).fetchone():
                raise DomainError(409, "library_exists", "같은 ID의 서재가 이미 있습니다.")
            db.execute("INSERT INTO libraries VALUES(?,?,?,?,?)",
                       (request.id, request.name, encode(request.history.model_dump()), 1, self.clock()))
            self.event(db, request.id, "library.created", {"id": request.id})
            return self.library(self.require_library(db, request.id))

    def settings(self, library_id):
        with self.transaction() as db:
            return self.library(self.require_library(db, library_id))

    def update_settings(self, library_id, request):
        with self.transaction(True) as db:
            current = self.require_library(db, library_id)
            if current["settings_version"] != request.expected_version:
                raise DomainError(409, "settings_conflict", "다른 기기에서 설정을 바꿨습니다.",
                                  current_version=current["settings_version"])
            policy = encode(request.history.model_dump())
            if current["policy"] != policy:
                db.execute("UPDATE libraries SET policy=?, settings_version=settings_version+1 WHERE id=?", (policy, library_id))
                self.event(db, library_id, "settings.updated", {"settings_version": current["settings_version"] + 1})
            return self.library(self.require_library(db, library_id))

    def snapshot(self, library_id):
        # 목록과 커서를 한 읽기 트랜잭션에서 얻어 재연결 중 변경을 놓치지 않는다.
        with self.transaction() as db:
            library = self.library(self.require_library(db, library_id))
            documents = []
            for row in db.execute("SELECT * FROM documents WHERE library_id=? ORDER BY id", (library_id,)):
                document = self.document(row)
                del document["body"]
                documents.append(document)
            assets = [dict(row) for row in db.execute(
                "SELECT sha256,media_type,length(data) AS size,created_at FROM assets WHERE library_id=? ORDER BY sha256", (library_id,))]
            cursor = db.execute("SELECT coalesce(max(id),0) FROM changes").fetchone()[0]
            return {"library": library, "documents": documents, "assets": assets, "cursor": cursor}

    def read_document(self, library_id, document_id):
        with self.transaction() as db:
            self.require_library(db, library_id)
            row = self.document_row(db, library_id, document_id)
            if row is None:
                raise DomainError(404, "document_not_found", "문서를 찾을 수 없습니다.")
            return self.document(row)

    def once(self, db, library_id, request, operation, action):
        fingerprint = digest({"operation": operation, "request": request.model_dump()})
        previous = db.execute("SELECT * FROM mutations WHERE library_id=? AND device_id=? AND mutation_id=?",
                              (library_id, request.device_id, request.mutation_id)).fetchone()
        if previous:
            if previous["fingerprint"] != fingerprint:
                raise DomainError(409, "mutation_reused", "이미 사용한 mutation_id를 다른 요청에 사용할 수 없습니다.")
            return previous["status"], json.loads(previous["response"])
        status, response = action()
        if "document" in response:
            # 재전송 영수증에 원고 사본을 쌓지 않아 이력 생성·보관 규칙을 우회하지 않는다.
            document = response["document"]
            response = {**response, "document": {key: document[key] for key in
                        ("id", "library_id", "version", "sha256", "deleted")}}
        db.execute("INSERT INTO mutations VALUES(?,?,?,?,?,?)", (library_id, request.device_id,
                   request.mutation_id, fingerprint, status, encode(response)))
        return status, response

    def apply_document(self, db, library_id, document_id, request):
        from .history import capture

        current = self.document_row(db, library_id, document_id)
        content = content_of(request.model_dump())
        if content["deleted"]:
            if current is None:
                raise DomainError(404, "document_not_found", "삭제할 문서를 찾을 수 없습니다.")
            content = {**content_of(self.document(current)), "deleted": True}
        content_hash = digest(content)
        # 이미 같은 상태이면 오래된 응답의 재전송이어도 새 버전을 만들지 않는다.
        if current is not None and current["content_hash"] == content_hash:
            return 200, {"document": self.document(current), "changed": False}
        version = current["version"] if current is not None else 0
        if version != request.expected_version:
            if current is None:
                raise DomainError(409, "document_not_found", "서버에 문서가 없습니다. 새 문서로 등록해 주세요.")
            conflict_id = uuid.uuid4().hex
            db.execute("INSERT INTO conflicts VALUES(?,?,?,?,?,?,?,?,NULL)",
                       (conflict_id, library_id, document_id, request.expected_version, version,
                        request.device_id, encode(content_of(request.model_dump())), self.clock()))
            self.event(db, library_id, "conflict.created", {"id": conflict_id, "document_id": document_id})
            return 409, {"error": "revision_conflict", "message": "서버의 다른 변경이 있습니다. 입력 내용은 충돌 사본으로 보존했습니다.",
                         "current_version": version, "conflict_id": conflict_id}
        db.execute("""INSERT INTO documents VALUES(?,?,?,?,?,?,?,?,?,?,?)
                      ON CONFLICT(library_id,id) DO UPDATE SET
                      kind=excluded.kind,title=excluded.title,body=excluded.body,metadata=excluded.metadata,
                      deleted=excluded.deleted,version=excluded.version,content_hash=excluded.content_hash,
                      device_id=excluded.device_id,updated_at=excluded.updated_at""",
                   (library_id, document_id, content["kind"], content["title"], content["body"], encode(content["metadata"]),
                    int(content["deleted"]), version + 1, content_hash, request.device_id, self.clock()))
        document = self.document(self.document_row(db, library_id, document_id))
        self.event(db, library_id, "document.deleted" if document["deleted"] else "document.updated",
                   {"id": document_id, "version": document["version"], "sha256": content_hash})
        if current is None:
            entry, _, _ = capture(db, document, "initial", self.clock())
            self.event(db, library_id, "history.created", {"id": entry["id"], "document_id": document_id})
        return (201 if current is None else 200), {"document": document, "changed": True}

    def mutate_document(self, library_id, document_id, request):
        with self.transaction(True) as db:
            self.require_library(db, library_id)
            return self.once(db, library_id, request, f"document:{document_id}",
                             lambda: self.apply_document(db, library_id, document_id, request))

    def changes(self, library_id, after, limit):
        with self.transaction() as db:
            self.require_library(db, library_id)
            maximum = db.execute("SELECT coalesce(max(id),0) FROM changes").fetchone()[0]
            if after > maximum:
                raise DomainError(409, "cursor_ahead", "서버의 현재 위치보다 앞선 커서입니다. 서재 목록을 다시 받아 주세요.")
            rows = db.execute("SELECT * FROM changes WHERE library_id=? AND id>? ORDER BY id LIMIT ?",
                              (library_id, after, limit + 1)).fetchall()
            more = len(rows) > limit
            events = [{"id": row["id"], "type": row["type"], "data": json.loads(row["payload"]),
                       "created_at": row["created_at"]} for row in rows[:limit]]
            cursor = events[-1]["id"] if more else maximum
            return {"events": events, "cursor": cursor, "has_more": more}

    def conflicts(self, library_id, after, limit):
        with self.transaction() as db:
            self.require_library(db, library_id)
            rows = db.execute("""SELECT id,document_id,base_version,server_version,device_id,created_at
                                 FROM conflicts WHERE library_id=? AND resolved_at IS NULL AND id>?
                                 ORDER BY id LIMIT ?""", (library_id, after, limit + 1)).fetchall()
            return {"entries": [dict(row) for row in rows[:limit]], "has_more": len(rows) > limit}

    def read_conflict(self, library_id, conflict_id):
        with self.transaction() as db:
            self.require_library(db, library_id)
            row = db.execute("SELECT * FROM conflicts WHERE library_id=? AND id=?", (library_id, conflict_id)).fetchone()
            if row is None:
                raise DomainError(404, "conflict_not_found", "충돌 사본을 찾을 수 없습니다.")
            result = dict(row)
            result["incoming"] = json.loads(result["incoming"])
            return result

    def resolve_conflict(self, library_id, conflict_id, request):
        from .models import DocumentMutation

        with self.transaction(True) as db:
            self.require_library(db, library_id)

            def action():
                row = db.execute("SELECT * FROM conflicts WHERE library_id=? AND id=?", (library_id, conflict_id)).fetchone()
                if row is None or row["resolved_at"] is not None:
                    raise DomainError(404, "conflict_not_found", "해결할 충돌 사본이 없습니다.")
                current = self.document_row(db, library_id, row["document_id"])
                if current["version"] != request.expected_version:
                    raise DomainError(409, "revision_conflict", "서버 원고가 다시 바뀌었습니다.", current_version=current["version"])
                result = {"document": self.document(current), "changed": False}
                if request.choice == "incoming":
                    mutation = DocumentMutation(**json.loads(row["incoming"]), device_id=request.device_id,
                                                mutation_id=request.mutation_id, expected_version=request.expected_version)
                    _, result = self.apply_document(db, library_id, row["document_id"], mutation)
                db.execute("UPDATE conflicts SET resolved_at=? WHERE id=?", (self.clock(), conflict_id))
                self.event(db, library_id, "conflict.resolved", {"id": conflict_id})
                return 200, result

            return self.once(db, library_id, request, f"resolve:{conflict_id}", action)

    def put_asset(self, library_id, sha256, media_type, data):
        if hashlib.sha256(data).hexdigest() != sha256:
            raise DomainError(422, "hash_mismatch", "첨부 자료의 SHA-256이 주소와 다릅니다.")
        with self.transaction(True) as db:
            self.require_library(db, library_id)
            previous = db.execute("SELECT 1 FROM assets WHERE library_id=? AND sha256=?", (library_id, sha256)).fetchone()
            if previous is None:
                db.execute("INSERT INTO assets VALUES(?,?,?,?,?)", (library_id, sha256, media_type, data, self.clock()))
                self.event(db, library_id, "asset.created", {"sha256": sha256, "size": len(data)})
            return {"sha256": sha256, "size": len(data), "created": previous is None}

    def read_asset(self, library_id, sha256):
        with self.transaction() as db:
            self.require_library(db, library_id)
            row = db.execute("SELECT * FROM assets WHERE library_id=? AND sha256=?", (library_id, sha256)).fetchone()
            if row is None:
                raise DomainError(404, "asset_not_found", "첨부 자료를 찾을 수 없습니다.")
            return row["data"], row["media_type"]
