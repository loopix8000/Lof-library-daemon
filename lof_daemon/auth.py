from collections import deque
from dataclasses import dataclass
import hashlib
import hmac
import secrets
import threading
import time
import uuid

from . import __version__
from .errors import DomainError

CODE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"
PAIRING_SECONDS = 10 * 60
SESSION_SECONDS = 12 * 60 * 60
SEEN_INTERVAL = 60
KEEP_USED_SECONDS = 24 * 60 * 60
SCRYPT = {"n": 2 ** 15, "r": 8, "p": 1, "maxmem": 64 * 1024 * 1024}


@dataclass(frozen=True)
class Principal:
    kind: str
    role: str
    id: str
    name: str

    @property
    def admin(self):
        return self.role == "admin"


def new_code():
    value = "".join(secrets.choice(CODE_ALPHABET) for _ in range(8))
    return f"{value[:4]}-{value[4:]}"


def normalize_code(code):
    return "".join(char for char in code.upper() if char.isascii() and char.isalnum())


def hash_password(password):
    salt = secrets.token_bytes(16)
    derived = hashlib.scrypt(password.encode(), salt=salt, **SCRYPT)
    return f"scrypt${SCRYPT['n']}${SCRYPT['r']}${SCRYPT['p']}${salt.hex()}${derived.hex()}"


def verify_password(password, stored):
    try:
        _, n, r, p, salt, expected = stored.split("$")
        derived = hashlib.scrypt(password.encode(), salt=bytes.fromhex(salt), n=int(n), r=int(r), p=int(p),
                                 maxmem=SCRYPT["maxmem"])
    except (ValueError, TypeError):
        return False
    return hmac.compare_digest(derived.hex(), expected)


class Throttle:
    """인증 실패를 출처별·전체로 세어 연결 코드와 비밀번호 대입을 늦춘다."""

    def __init__(self, limit, total, window=600, clock=time.monotonic):
        self.limit, self.total, self.window, self.clock = limit, total, window, clock
        self.failures = {}
        self.lock = threading.Lock()

    def count(self, key):
        queue = self.failures.get(key)
        if not queue:
            return 0
        oldest = self.clock() - self.window
        while queue and queue[0] <= oldest:
            queue.popleft()
        if not queue:
            del self.failures[key]
        return len(queue)

    def check(self, source):
        with self.lock:
            if self.count(source) >= self.limit or self.count("*") >= self.total:
                raise DomainError(429, "too_many_attempts", "입력 시도가 너무 많습니다. 잠시 후 다시 시도해 주세요.")

    def fail(self, source):
        with self.lock:
            now = self.clock()
            for key in (source, "*"):
                self.failures.setdefault(key, deque()).append(now)


class Auth:
    """서버 키, 관리자 비밀번호와 세션, 기기 인증 키, 일회용 연결 코드를 서로 분리해 관리한다."""

    def __init__(self, store, local_token="", legacy_token="", server_name="Lof Library"):
        self.store = store
        self.local_token, self.legacy_token = local_token, legacy_token
        self.server_name = server_name
        # 서버 키는 최초 기동 때 한 번 만들고 DB에 보관해 재기동·업데이트·백업 복구 후에도 기기 연결을 유지한다.
        with store.transaction(True) as db:
            values = {row["key"]: row["value"] for row in db.execute("SELECT key,value FROM server")}
            for key, factory in (("secret", lambda: secrets.token_hex(32)), ("server_id", lambda: uuid.uuid4().hex)):
                if key not in values:
                    values[key] = factory()
                    db.execute("INSERT INTO server VALUES(?,?)", (key, values[key]))
        self.secret = bytes.fromhex(values["secret"])
        self.server_id = values["server_id"]

    def hash(self, purpose, value):
        return hmac.new(self.secret, f"{purpose}:{value}".encode(), hashlib.sha256).hexdigest()

    @staticmethod
    def get(db, key):
        row = db.execute("SELECT value FROM server WHERE key=?", (key,)).fetchone()
        return row["value"] if row else None

    @staticmethod
    def put(db, key, value):
        db.execute("INSERT INTO server VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", (key, value))

    def identity(self):
        return {"id": self.server_id, "name": self.server_name, "version": __version__}

    # 관리자

    def setup_required(self):
        with self.store.transaction() as db:
            return self.get(db, "admin_password") is None

    def prepare_setup(self):
        """관리자가 없으면 새 최초 설정 코드를 만든다. 이전 코드는 더 이상 쓸 수 없다."""
        with self.store.transaction(True) as db:
            if self.get(db, "admin_password") is not None:
                return None
            code = new_code()
            self.put(db, "setup_code", self.hash("setup", normalize_code(code)))
            return code

    def reset_admin(self):
        with self.store.transaction(True) as db:
            db.execute("DELETE FROM server WHERE key IN ('admin_password','setup_code')")
            db.execute("DELETE FROM sessions")
        return self.prepare_setup()

    def setup(self, principal, code, password):
        hashed = hash_password(password)
        with self.store.transaction(True) as db:
            if self.get(db, "admin_password") is not None:
                raise DomainError(409, "already_configured", "관리자가 이미 설정되어 있습니다.")
            # 데이터 폴더를 실행하는 이 컴퓨터의 관리 앱은 설정 코드 없이 바로 설정할 수 있다.
            if principal is None or principal.kind != "local":
                expected = self.get(db, "setup_code")
                if expected is None or not hmac.compare_digest(expected, self.hash("setup", normalize_code(code or ""))):
                    raise DomainError(403, "setup_code_invalid", "최초 설정 코드가 올바르지 않습니다. 서버 로그의 코드를 확인해 주세요.")
            self.put(db, "admin_password", hashed)
            db.execute("DELETE FROM server WHERE key='setup_code'")
            return self.session(db)

    def session(self, db):
        token = "lofs_" + secrets.token_urlsafe(32)
        now = self.store.clock()
        db.execute("INSERT INTO sessions VALUES(?,?,?)", (self.hash("session", token), now, now + SESSION_SECONDS))
        return {"token": token, "expires_at": now + SESSION_SECONDS}

    def login(self, password):
        with self.store.transaction() as db:
            stored = self.get(db, "admin_password")
        if stored is None:
            raise DomainError(409, "setup_required", "먼저 관리자 비밀번호를 설정해 주세요.")
        if not verify_password(password, stored):
            raise DomainError(401, "invalid_password", "비밀번호가 올바르지 않습니다.")
        with self.store.transaction(True) as db:
            return self.session(db)

    def logout(self, principal):
        if principal.kind == "session":
            with self.store.transaction(True) as db:
                db.execute("DELETE FROM sessions WHERE token_hash=?", (principal.id,))

    def change_password(self, principal, current, new):
        with self.store.transaction() as db:
            stored = self.get(db, "admin_password")
        if stored is None:
            raise DomainError(409, "setup_required", "먼저 관리자 비밀번호를 설정해 주세요.")
        if principal.kind != "local" and not verify_password(current or "", stored):
            raise DomainError(403, "invalid_password", "현재 비밀번호가 올바르지 않습니다.")
        hashed = hash_password(new)
        # 비밀번호를 바꾸면 다른 브라우저의 로그인을 모두 끝내고 요청한 쪽에만 새 세션을 준다.
        with self.store.transaction(True) as db:
            self.put(db, "admin_password", hashed)
            db.execute("DELETE FROM sessions")
            return self.session(db)

    # 인증

    def authenticate(self, token):
        if not token:
            return None
        encoded = token.encode()
        if self.local_token and hmac.compare_digest(encoded, self.local_token.encode()):
            return Principal("local", "admin", "local", "이 컴퓨터")
        if self.legacy_token and hmac.compare_digest(encoded, self.legacy_token.encode()):
            return Principal("legacy", "device", "legacy", "기존 공통 토큰")
        now = self.store.clock()
        if token.startswith("lofs_"):
            hashed = self.hash("session", token)
            with self.store.transaction() as db:
                row = db.execute("SELECT expires_at FROM sessions WHERE token_hash=?", (hashed,)).fetchone()
            return Principal("session", "admin", hashed, "관리자") if row and row["expires_at"] > now else None
        if token.startswith("lofd_"):
            with self.store.transaction() as db:
                row = db.execute("SELECT * FROM devices WHERE token_hash=?", (self.hash("device", token),)).fetchone()
            if row is None:
                return None
            # 마지막 접속 시각은 일정 간격으로만 기록해 요청마다 쓰기가 생기지 않게 한다.
            if row["last_seen_at"] is None or now - row["last_seen_at"] >= SEEN_INTERVAL:
                with self.store.transaction(True) as db:
                    db.execute("UPDATE devices SET last_seen_at=? WHERE id=?", (now, row["id"]))
            return Principal("device", row["role"], row["id"], row["name"])
        return None

    def active(self, principal):
        with self.store.transaction() as db:
            if principal.kind == "device":
                return db.execute("SELECT 1 FROM devices WHERE id=?", (principal.id,)).fetchone() is not None
            if principal.kind == "session":
                row = db.execute("SELECT expires_at FROM sessions WHERE token_hash=?", (principal.id,)).fetchone()
                return row is not None and row["expires_at"] > self.store.clock()
        return True

    # 기기

    def devices(self, online):
        with self.store.transaction() as db:
            rows = db.execute("SELECT id,name,role,platform,created_at,last_seen_at FROM devices ORDER BY created_at,id").fetchall()
        return {"devices": [{**dict(row), "connected": online.get(row["id"], 0) > 0} for row in rows]}

    def rename_device(self, device_id, name):
        if not name.strip():
            raise DomainError(422, "invalid_name", "기기 이름을 입력해 주세요.")
        with self.store.transaction(True) as db:
            if db.execute("UPDATE devices SET name=? WHERE id=?", (name.strip(), device_id)).rowcount == 0:
                raise DomainError(404, "device_not_found", "연결 기기를 찾을 수 없습니다.")
            return dict(db.execute("SELECT id,name,role,platform,created_at,last_seen_at FROM devices WHERE id=?",
                                   (device_id,)).fetchone())

    def revoke_device(self, device_id):
        with self.store.transaction(True) as db:
            if db.execute("DELETE FROM devices WHERE id=?", (device_id,)).rowcount == 0:
                raise DomainError(404, "device_not_found", "연결 기기를 찾을 수 없습니다.")

    # 연결 코드

    def create_pairing(self, principal, name, role):
        code = new_code()
        now = self.store.clock()
        entry = {"id": uuid.uuid4().hex, "name": name.strip(), "role": role, "created_by": principal.name,
                 "created_at": now, "expires_at": now + PAIRING_SECONDS}
        with self.store.transaction(True) as db:
            db.execute("INSERT INTO pairing_codes VALUES(?,?,?,?,?,?,?,NULL,NULL)",
                       (entry["id"], self.hash("pair", normalize_code(code)), entry["name"], role,
                        entry["created_by"], now, entry["expires_at"]))
        return {**entry, "code": code}

    def pairings(self):
        with self.store.transaction() as db:
            rows = db.execute("""SELECT id,name,role,created_by,created_at,expires_at FROM pairing_codes
                                 WHERE used_at IS NULL AND expires_at>? ORDER BY created_at""", (self.store.clock(),))
            return {"entries": [dict(row) for row in rows]}

    def cancel_pairing(self, entry_id):
        with self.store.transaction(True) as db:
            if db.execute("DELETE FROM pairing_codes WHERE id=? AND used_at IS NULL", (entry_id,)).rowcount == 0:
                raise DomainError(404, "pairing_code_not_found", "취소할 연결 코드가 없습니다.")

    def pair(self, code, name, platform):
        token = "lofd_" + secrets.token_urlsafe(32)
        now = self.store.clock()
        with self.store.transaction(True) as db:
            row = db.execute("SELECT * FROM pairing_codes WHERE code_hash=?",
                             (self.hash("pair", normalize_code(code)),)).fetchone()
            if row is None:
                raise DomainError(404, "pairing_code_invalid", "연결 코드를 확인해 주세요.")
            if row["used_at"] is not None:
                raise DomainError(410, "pairing_code_used", "이미 사용한 연결 코드입니다. 관리 화면에서 새 코드를 발급해 주세요.")
            if row["expires_at"] <= now:
                raise DomainError(410, "pairing_code_expired", "연결 코드의 유효 시간이 지났습니다. 관리 화면에서 새 코드를 발급해 주세요.")
            device = {"id": uuid.uuid4().hex, "name": name.strip() or row["name"] or "새 기기", "role": row["role"],
                      "platform": platform.strip(), "created_at": now, "last_seen_at": now}
            db.execute("INSERT INTO devices VALUES(?,?,?,?,?,?,?)", (device["id"], device["name"], device["role"],
                       device["platform"], self.hash("device", token), now, now))
            db.execute("UPDATE pairing_codes SET used_at=?, device_id=? WHERE id=?", (now, device["id"], row["id"]))
        return {"device": device, "token": token, "server": self.identity()}

    def maintain(self):
        now = self.store.clock()
        with self.store.transaction(True) as db:
            removed = db.execute("DELETE FROM pairing_codes WHERE coalesce(used_at, expires_at)<?",
                                 (now - KEEP_USED_SECONDS,)).rowcount
            return removed + db.execute("DELETE FROM sessions WHERE expires_at<=?", (now,)).rowcount
