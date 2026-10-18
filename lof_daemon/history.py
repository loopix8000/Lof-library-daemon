import json
import uuid

from diff_match_patch import diff_match_patch

from .database import content_of, encode
from .errors import DomainError
from .models import DocumentMutation, HistoryPolicy

PROTECTED_REASONS = {"manual", "before_restore", "restore"}


def entry(row, full=False):
    result = {key: row[key] for key in ("id", "document_id", "document_version", "reason", "created_at")}
    result["protected"] = bool(row["protected"])
    result["cursor"] = row["seq"]
    if full:
        result["snapshot"] = json.loads(row["snapshot"])
    return result


def capture(db, document, reason, now):
    """전송 버전과 별도로 보관할 복구 시점만 만든다. 같은 내용은 표지만 추가한다."""
    library_id, document_id = document["library_id"], document["id"]
    previous = db.execute("SELECT * FROM histories WHERE library_id=? AND document_id=? ORDER BY seq DESC LIMIT 1",
                          (library_id, document_id)).fetchone()
    protected = reason in PROTECTED_REASONS
    created = previous is None or previous["content_hash"] != document["sha256"]
    marked = False
    if created:
        history_id = uuid.uuid4().hex
        db.execute("""INSERT INTO histories(id,library_id,document_id,document_version,content_hash,snapshot,reason,protected,created_at)
                      VALUES(?,?,?,?,?,?,?,?,?)""", (history_id, library_id, document_id, document["version"],
                   document["sha256"], encode(document), reason, int(protected), now))
    else:
        history_id = previous["id"]
        if protected and (not previous["protected"] or (reason == "manual" and previous["reason"] != "manual")):
            db.execute("UPDATE histories SET protected=1,reason=? WHERE id=?", (reason, history_id))
            marked = True
    current = db.execute("SELECT * FROM histories WHERE id=?", (history_id,)).fetchone()
    db.execute("""INSERT INTO history_state VALUES(?,?,?,?,?,?,?,?)
                  ON CONFLICT(library_id,document_id) DO UPDATE SET snapshot=excluded.snapshot,
                  content_hash=excluded.content_hash,recorded_at=excluded.recorded_at,
                  checked_version=excluded.checked_version,checked_policy=excluded.checked_policy,changed_chars=0""",
               (library_id, document_id, encode(document), document["sha256"], current["created_at"],
                document["version"], 0, 0))
    return entry(current, True), created, marked


def changed_chars(before, after):
    old = "" if before["deleted"] else before["body"]
    new = "" if after["deleted"] else after["body"]
    if old == new:
        return 0
    differ = diff_match_patch()
    differ.Diff_Timeout = 0.2
    # 키 입력 횟수나 전체 길이 차이가 아니라 비교 결과의 추가·삭제 글자 수를 센다.
    return sum(len(text) for operation, text in differ.diff_main(old, new) if operation != 0)


class HistoryManager:
    def __init__(self, store):
        self.store = store

    def list(self, library_id, document_id, before, limit):
        with self.store.transaction() as db:
            self.store.require_library(db, library_id)
            if self.store.document_row(db, library_id, document_id) is None:
                raise DomainError(404, "document_not_found", "문서를 찾을 수 없습니다.")
            rows = db.execute("""SELECT seq,id,document_id,document_version,reason,protected,created_at
                                 FROM histories WHERE library_id=? AND document_id=? AND seq<?
                                 ORDER BY seq DESC LIMIT ?""", (library_id, document_id, before, limit + 1)).fetchall()
            return {"entries": [entry(row) for row in rows[:limit]], "has_more": len(rows) > limit}

    def read(self, library_id, document_id, history_id):
        with self.store.transaction() as db:
            self.store.require_library(db, library_id)
            row = self._read(db, library_id, document_id, history_id)
            return entry(row, True)

    @staticmethod
    def _read(db, library_id, document_id, history_id):
        row = db.execute("SELECT * FROM histories WHERE library_id=? AND document_id=? AND id=?",
                         (library_id, document_id, history_id)).fetchone()
        if row is None:
            raise DomainError(404, "history_not_found", "이력을 찾을 수 없습니다.")
        return row

    def _capture(self, db, document, reason):
        result, created, marked = capture(db, document, reason, self.store.clock())
        if created or marked:
            self.store.event(db, document["library_id"], "history.created" if created else "history.marked",
                             {"id": result["id"], "document_id": document["id"], "reason": result["reason"]})
        return {"entry": result, "created": created, "marked": marked}

    def manual(self, library_id, document_id, request):
        with self.store.transaction(True) as db:
            self.store.require_library(db, library_id)
            row = self.store.document_row(db, library_id, document_id)
            if row is None:
                raise DomainError(404, "document_not_found", "문서를 찾을 수 없습니다.")
            if row["version"] != request.expected_version:
                raise DomainError(409, "revision_conflict", "수동 저장 전에 최신 원고를 확인해 주세요.", current_version=row["version"])
            return self._capture(db, self.store.document(row), "manual")

    def restore(self, library_id, document_id, history_id, request):
        with self.store.transaction(True) as db:
            self.store.require_library(db, library_id)

            def action():
                history = self._read(db, library_id, document_id, history_id)
                row = self.store.document_row(db, library_id, document_id)
                if row["version"] != request.expected_version:
                    raise DomainError(409, "revision_conflict", "복원 전에 최신 원고를 확인해 주세요.", current_version=row["version"])
                current = self.store.document(row)
                self._capture(db, current, "before_restore")
                old = json.loads(history["snapshot"])
                mutation = DocumentMutation(**content_of(old), **request.model_dump())
                status, result = self.store.apply_document(db, library_id, document_id, mutation)
                if status == 409:
                    raise RuntimeError("복원 중 잠긴 문서의 버전이 바뀌었습니다.")
                self._capture(db, result["document"], "restore")
                return 200, result

            return self.store.once(db, library_id, request, f"restore:{document_id}:{history_id}", action)

    def tick(self):
        count = 0
        now = self.store.clock()
        with self.store.transaction(True) as db:
            # 같은 버전이 글자 수 조건을 만족하지 않았다면 다음 입력까지 다시 비교하지 않는다.
            rows = db.execute("""SELECT d.*,h.snapshot AS baseline,h.checked_version,h.checked_policy,h.changed_chars,
                                        l.policy,l.settings_version AS policy_version
                                 FROM documents d JOIN history_state h ON h.library_id=d.library_id AND h.document_id=d.id
                                 JOIN libraries l ON l.id=d.library_id
                                 WHERE d.content_hash<>h.content_hash
                                   AND h.recorded_at<=? - json_extract(l.policy,'$.min_minutes')*60
                                   AND (h.checked_version<>d.version OR h.checked_policy<>l.settings_version
                                        OR h.changed_chars>=json_extract(l.policy,'$.min_changed_chars'))
                                 ORDER BY h.recorded_at,d.library_id,d.id LIMIT 20""", (now,)).fetchall()
            for row in rows:
                document = self.store.document(row)
                policy = HistoryPolicy.model_validate_json(row["policy"])
                changed = row["changed_chars"]
                if row["checked_version"] != row["version"] or row["checked_policy"] != row["policy_version"]:
                    changed = changed_chars(json.loads(row["baseline"]), document)
                db.execute("""UPDATE history_state SET checked_version=?,checked_policy=?,changed_chars=?
                              WHERE library_id=? AND document_id=?""",
                           (row["version"], row["policy_version"], changed, row["library_id"], row["id"]))
                if changed >= policy.min_changed_chars:
                    self._capture(db, document, "automatic")
                    count += 1
        return count

    def maintain(self):
        removed = 0
        now = self.store.clock()
        with self.store.transaction(True) as db:
            for library in db.execute("SELECT * FROM libraries").fetchall():
                policy = HistoryPolicy.model_validate_json(library["policy"])
                if policy.retention_days is None and not policy.smart.enabled:
                    continue
                rows = db.execute("SELECT seq,id,document_id,protected,created_at FROM histories WHERE library_id=? ORDER BY seq DESC",
                                  (library["id"],)).fetchall()
                latest = set()
                buckets = set()
                delete = []
                for row in rows:
                    document_id = row["document_id"]
                    is_latest = document_id not in latest
                    latest.add(document_id)
                    expired = policy.retention_days is not None and row["created_at"] < now - policy.retention_days * 86400
                    if expired and not is_latest and (not row["protected"] or policy.expire_protected):
                        delete.append(row["id"])
                        continue
                    if row["protected"] or not policy.smart.enabled or row["created_at"] >= now - policy.smart.keep_recent_minutes * 60:
                        continue
                    # 모든 기기에 동일한 UTC 고정 구간을 사용하고 구간의 최신 원본 이력을 남긴다.
                    bucket = (document_id, int(row["created_at"] // (policy.smart.bucket_minutes * 60)))
                    if bucket in buckets and not is_latest:
                        delete.append(row["id"])
                    buckets.add(bucket)
                if delete:
                    db.executemany("DELETE FROM histories WHERE id=?", [(history_id,) for history_id in delete])
                    self.store.event(db, library["id"], "history.pruned", {"removed": len(delete)})
                    removed += len(delete)
        return removed
