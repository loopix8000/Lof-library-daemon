import asyncio
from contextlib import asynccontextmanager, suppress
from datetime import datetime, timezone
import json
import logging
import sqlite3
import time
from typing import Annotated
from urllib.parse import urlencode

from fastapi import APIRouter, Depends, FastAPI, Header, HTTPException, Path, Query, Request
from fastapi.responses import JSONResponse, Response, StreamingResponse
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer

from . import __version__
from .auth import Auth, Principal, Throttle
from .config import Config, InstanceLock
from .database import Store
from .errors import DomainError
from .history import HistoryManager
from .models import (Capture, DeviceUpdate, DocumentMutation, LibraryCreate, Login, Pair, PairingCreate,
                     PasswordChange, PolicyUpdate, Resolve, Restore, Setup)

logger = logging.getLogger("lof_daemon")
IdPath = Annotated[str, Path(pattern=r"^[A-Za-z0-9_-]{1,128}$")]
DigestPath = Annotated[str, Path(pattern=r"^[a-f0-9]{64}$")]
MAX_CURSOR = (1 << 63) - 1


class Notifications:
    def __init__(self):
        self.condition = asyncio.Condition()
        self.generation = 0

    async def notify(self):
        async with self.condition:
            self.generation += 1
            self.condition.notify_all()

    async def wait(self, generation, timeout):
        async with self.condition:
            with suppress(asyncio.TimeoutError):
                await asyncio.wait_for(self.condition.wait_for(lambda: self.generation != generation), timeout)


class BodyLimit:
    def __init__(self, app, maximum):
        self.app, self.maximum = app, maximum

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        headers = dict(scope.get("headers", []))
        try:
            length = int(headers.get(b"content-length", b"0"))
        except ValueError:
            return await JSONResponse({"message": "Content-Length를 확인해 주세요."}, status_code=400)(scope, receive, send)
        if length > self.maximum:
            return await JSONResponse({"message": "요청 크기 제한을 초과했습니다."}, status_code=413)(scope, receive, send)
        total = 0

        async def limited_receive():
            nonlocal total
            message = await receive()
            total += len(message.get("body", b""))
            if total > self.maximum:
                raise HTTPException(413, "요청 크기 제한을 초과했습니다.")
            return message

        await self.app(scope, limited_receive, send)


def connection_link(server_url, code):
    """다른 기기에 복사하거나 QR로 보여 줄 연결 정보. 서버 주소와 일회용 코드만 담는다."""
    return "lof-library://connect?" + urlencode({"server": server_url.rstrip("/"), "code": code})


def create_app(config: Config | None = None):
    config = config or Config.from_env()
    notifications = Notifications()
    online = {}
    pair_throttle = Throttle(limit=10, total=100)
    login_throttle = Throttle(limit=10, total=50)

    @asynccontextmanager
    async def lifespan(app):
        instance = InstanceLock(config.data_dir)
        store = None
        task = None
        try:
            store = Store(config.data_dir)
            history = HistoryManager(store)
            auth = Auth(store, config.local_admin_token, config.api_token, config.server_name)
            app.state.store, app.state.history, app.state.auth = store, history, auth
            app.state.notifications = notifications
            app.state.online = online
            started = time.monotonic()
            app.state.started = started
            setup_code = auth.prepare_setup()
            if setup_code:
                logger.warning("관리자가 아직 설정되지 않았습니다. 관리 화면에서 최초 설정 코드 %s 를 입력해 주세요.", setup_code)
            if config.api_token:
                logger.warning("LOF_API_TOKEN은 이전 방식의 공통 토큰입니다. 모든 기기를 연결 코드로 다시 연결한 뒤 설정을 제거해 주세요.")

            async def scheduler():
                next_maintenance = time.monotonic() + config.maintenance_seconds
                while True:
                    await asyncio.sleep(config.history_tick_seconds)
                    try:
                        changed = await asyncio.to_thread(history.tick)
                        if time.monotonic() >= next_maintenance:
                            changed += await asyncio.to_thread(history.maintain)
                            await asyncio.to_thread(auth.maintain)
                            next_maintenance = time.monotonic() + config.maintenance_seconds
                        if changed:
                            await notifications.notify()
                    except Exception:
                        logger.exception("이력 작업을 완료하지 못했습니다. 다음 주기에 재시도합니다.")

            task = asyncio.create_task(scheduler())
            logger.info("서재 데몬을 시작했습니다.")
            yield
        finally:
            if task:
                task.cancel()
                with suppress(asyncio.CancelledError):
                    await task
            if store:
                await asyncio.to_thread(store.close)
            instance.close()

    app = FastAPI(title="Lof Library Daemon", version=__version__, lifespan=lifespan)
    app.add_middleware(BodyLimit, maximum=config.max_request_bytes)
    bearer = HTTPBearer(auto_error=False)

    async def optional_principal(credentials: Annotated[HTTPAuthorizationCredentials | None, Depends(bearer)]):
        token = credentials.credentials if credentials else ""
        return await asyncio.to_thread(app.state.auth.authenticate, token) if token else None

    async def authenticate(principal: Annotated[Principal | None, Depends(optional_principal)]):
        if principal is None:
            raise HTTPException(401, "유효한 인증 키가 필요합니다.", headers={"WWW-Authenticate": "Bearer"})
        return principal

    async def require_admin(principal: Annotated[Principal, Depends(authenticate)]):
        if not principal.admin:
            raise HTTPException(403, "관리자 권한이 필요합니다.")
        return principal

    CurrentPrincipal = Annotated[Principal, Depends(authenticate)]
    AdminPrincipal = Annotated[Principal, Depends(require_admin)]

    router = APIRouter(prefix="/v1", dependencies=[Depends(authenticate)])
    admin = APIRouter(prefix="/v1/admin", dependencies=[Depends(require_admin)], tags=["관리"])
    public = APIRouter(prefix="/v1", tags=["인증"])

    @app.exception_handler(DomainError)
    async def domain_error(_request, error):
        return JSONResponse(error.payload, status_code=error.status)

    @app.exception_handler(sqlite3.Error)
    async def database_error(_request, error):
        logger.error("데이터베이스 작업을 완료하지 못했습니다: %s", type(error).__name__)
        return JSONResponse({"error": "database_unavailable", "message": "저장소를 사용할 수 없습니다. 잠시 후 다시 시도해 주세요."}, status_code=503)

    async def health():
        healthy = await asyncio.to_thread(app.state.store.healthy)
        return JSONResponse({"status": "ok" if healthy else "error"}, status_code=200 if healthy else 503,
                            headers={"Cache-Control": "no-store"})

    app.add_api_route("/healthz", health, methods=["GET"], tags=["상태 확인"])

    @app.get("/status", tags=["상태 확인"])
    async def status():
        healthy = await asyncio.to_thread(app.state.store.healthy)
        setup_required = None
        if healthy:
            with suppress(sqlite3.Error):
                setup_required = await asyncio.to_thread(app.state.auth.setup_required)
        return JSONResponse({
            "service": "Lof Library Daemon", "version": __version__,
            "server_id": app.state.auth.server_id, "name": app.state.auth.server_name,
            "status": "ok" if healthy else "error", "database": "ok" if healthy else "error",
            "setup_required": setup_required,
            "time": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            "uptime_seconds": int(time.monotonic() - app.state.started),
        }, status_code=200 if healthy else 503, headers={"Cache-Control": "no-store"})

    async def write(function, *args):
        result = await asyncio.to_thread(function, *args)
        await notifications.notify()
        return result

    async def guarded(throttle, request, function, *args):
        source = request.client.host if request.client else "unknown"
        throttle.check(source)
        try:
            return await asyncio.to_thread(function, *args)
        except DomainError as error:
            if error.status in (401, 403, 404, 410):
                throttle.fail(source)
            raise

    # 인증과 기기 연결

    @public.post("/setup", status_code=201)
    async def setup(request: Request, payload: Setup,
                    principal: Annotated[Principal | None, Depends(optional_principal)]):
        return await guarded(login_throttle, request, app.state.auth.setup, principal, payload.code, payload.password)

    @public.post("/auth/login")
    async def login(request: Request, payload: Login):
        return await guarded(login_throttle, request, app.state.auth.login, payload.password)

    @public.post("/auth/logout", status_code=204)
    async def logout(principal: CurrentPrincipal):
        await asyncio.to_thread(app.state.auth.logout, principal)
        return Response(status_code=204)

    @public.post("/pair", status_code=201)
    async def pair(request: Request, payload: Pair):
        return await guarded(pair_throttle, request, app.state.auth.pair, payload.code, payload.name, payload.platform)

    @public.get("/me")
    async def me(principal: CurrentPrincipal):
        return {"kind": principal.kind, "role": principal.role, "id": principal.id if principal.kind == "device" else None,
                "name": principal.name, "server": app.state.auth.identity()}

    # 관리

    @admin.get("/server")
    async def server_info():
        def collect():
            size = sum(path.stat().st_size for path in config.data_dir.glob("library.sqlite*") if path.is_file())
            with app.state.store.transaction() as db:
                counts = {table: db.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
                          for table in ("libraries", "documents", "devices")}
            return size, counts

        size, counts = await asyncio.to_thread(collect)
        return {**app.state.auth.identity(), "uptime_seconds": int(time.monotonic() - app.state.started),
                "setup_required": await asyncio.to_thread(app.state.auth.setup_required),
                "data_dir": str(config.data_dir), "storage_bytes": size, "counts": counts,
                "connected_devices": len(online)}

    @admin.put("/password")
    async def change_password(request: Request, payload: PasswordChange, principal: AdminPrincipal):
        return await guarded(login_throttle, request, app.state.auth.change_password, principal,
                             payload.current_password, payload.new_password)

    @admin.get("/devices")
    async def devices():
        return await asyncio.to_thread(app.state.auth.devices, dict(online))

    @admin.patch("/devices/{device_id}")
    async def rename_device(device_id: IdPath, payload: DeviceUpdate):
        return await asyncio.to_thread(app.state.auth.rename_device, device_id, payload.name)

    @admin.delete("/devices/{device_id}", status_code=204)
    async def revoke_device(device_id: IdPath):
        await asyncio.to_thread(app.state.auth.revoke_device, device_id)
        # 연결 중인 실시간 수신이 다음 확인 때 바로 끊기도록 깨운다.
        await notifications.notify()
        return Response(status_code=204)

    @admin.get("/pairing-codes")
    async def pairings():
        return await asyncio.to_thread(app.state.auth.pairings)

    @admin.post("/pairing-codes", status_code=201)
    async def create_pairing(request: Request, payload: PairingCreate, principal: AdminPrincipal):
        entry = await asyncio.to_thread(app.state.auth.create_pairing, principal, payload.name, payload.role)
        server_url = payload.server_url or str(request.base_url)
        return {**entry, "server_url": server_url.rstrip("/"), "connection": connection_link(server_url, entry["code"])}

    @admin.delete("/pairing-codes/{entry_id}", status_code=204)
    async def cancel_pairing(entry_id: IdPath):
        await asyncio.to_thread(app.state.auth.cancel_pairing, entry_id)
        return Response(status_code=204)

    @admin.post("/shutdown", status_code=202)
    async def shutdown(principal: AdminPrincipal):
        if principal.kind != "local":
            raise HTTPException(403, "서버를 실행한 컴퓨터의 관리 앱에서만 종료할 수 있습니다.")
        stop = getattr(app.state, "request_shutdown", None)
        if stop is None:
            raise HTTPException(409, "이 실행 방식에서는 종료를 요청할 수 없습니다.")
        stop()
        return {"status": "stopping"}

    # 동기화

    @router.get("/libraries", tags=["서재"])
    async def libraries():
        return {"libraries": await asyncio.to_thread(app.state.store.libraries)}

    @router.post("/libraries", status_code=201, tags=["서재"])
    async def create_library(payload: LibraryCreate):
        return await write(app.state.store.create_library, payload)

    @router.get("/libraries/{library_id}/settings", tags=["공유 설정"])
    async def settings(library_id: IdPath):
        return await asyncio.to_thread(app.state.store.settings, library_id)

    @router.put("/libraries/{library_id}/settings", tags=["공유 설정"])
    async def update_settings(library_id: IdPath, payload: PolicyUpdate):
        return await write(app.state.store.update_settings, library_id, payload)

    @router.get("/libraries/{library_id}/snapshot", tags=["동기화"])
    async def snapshot(library_id: IdPath):
        return await asyncio.to_thread(app.state.store.snapshot, library_id)

    @router.get("/libraries/{library_id}/changes", tags=["동기화"])
    async def changes(library_id: IdPath, after: int = Query(0, ge=0, le=MAX_CURSOR), limit: int = Query(100, ge=1, le=500)):
        return await asyncio.to_thread(app.state.store.changes, library_id, after, limit)

    @router.get("/libraries/{library_id}/events", tags=["동기화"])
    async def events(request: Request, principal: CurrentPrincipal, library_id: IdPath,
                     after: int = Query(0, ge=0, le=MAX_CURSOR), last_event_id: str | None = Header(None)):
        if last_event_id is not None:
            try:
                after = int(last_event_id)
            except ValueError as error:
                raise HTTPException(422, "Last-Event-ID가 올바른 정수가 아닙니다.") from error
            if not 0 <= after <= MAX_CURSOR:
                raise HTTPException(422, "Last-Event-ID 범위를 확인해 주세요.")
        # 응답 헤더 전에도 서재와 커서를 검증해 재연결 오류를 정상 HTTP 응답으로 알린다.
        await asyncio.to_thread(app.state.store.changes, library_id, after, 1)

        async def stream():
            cursor = after
            online[principal.id] = online.get(principal.id, 0) + 1
            try:
                yield "retry: 3000\n\n"
                while not await request.is_disconnected():
                    # 연결 해제된 기기나 끝난 로그인의 수신은 다음 확인 때 종료한다.
                    if not await asyncio.to_thread(app.state.auth.active, principal):
                        break
                    generation = notifications.generation
                    page = await asyncio.to_thread(app.state.store.changes, library_id, cursor, 100)
                    for event in page["events"]:
                        data = json.dumps(event, ensure_ascii=False, separators=(",", ":"))
                        yield f"id: {event['id']}\nevent: {event['type']}\ndata: {data}\n\n"
                    cursor = page["cursor"]
                    if page["has_more"]:
                        continue
                    if not page["events"]:
                        yield ": keep-alive\n\n"
                    await notifications.wait(generation, config.heartbeat_seconds)
            finally:
                online[principal.id] -= 1
                if online[principal.id] <= 0:
                    del online[principal.id]

        return StreamingResponse(stream(), media_type="text/event-stream", headers={
            "Cache-Control": "no-cache", "X-Accel-Buffering": "no",
        })

    @router.get("/libraries/{library_id}/documents/{document_id}", tags=["동기화"])
    async def document(library_id: IdPath, document_id: IdPath, if_none_match: str | None = Header(None)):
        result = await asyncio.to_thread(app.state.store.read_document, library_id, document_id)
        etag = f'"{result["version"]}"'
        if if_none_match == etag:
            return Response(status_code=304, headers={"ETag": etag})
        return JSONResponse(result, headers={"ETag": etag, "Cache-Control": "no-cache"})

    @router.put("/libraries/{library_id}/documents/{document_id}", tags=["동기화"])
    async def put_document(library_id: IdPath, document_id: IdPath, payload: DocumentMutation):
        code, result = await write(app.state.store.mutate_document, library_id, document_id, payload)
        return JSONResponse(result, status_code=code)

    @router.get("/libraries/{library_id}/conflicts", tags=["충돌"])
    async def conflicts(library_id: IdPath, after: str = Query("", max_length=128), limit: int = Query(100, ge=1, le=500)):
        return await asyncio.to_thread(app.state.store.conflicts, library_id, after, limit)

    @router.get("/libraries/{library_id}/conflicts/{conflict_id}", tags=["충돌"])
    async def conflict(library_id: IdPath, conflict_id: IdPath):
        return await asyncio.to_thread(app.state.store.read_conflict, library_id, conflict_id)

    @router.post("/libraries/{library_id}/conflicts/{conflict_id}/resolve", tags=["충돌"])
    async def resolve(library_id: IdPath, conflict_id: IdPath, payload: Resolve):
        code, result = await write(app.state.store.resolve_conflict, library_id, conflict_id, payload)
        return JSONResponse(result, status_code=code)

    @router.put("/libraries/{library_id}/assets/{sha256}", tags=["첨부 자료"])
    async def put_asset(request: Request, library_id: IdPath, sha256: DigestPath):
        data = await request.body()
        media_type = request.headers.get("content-type", "application/octet-stream")[:200]
        return await write(app.state.store.put_asset, library_id, sha256, media_type, data)

    @router.get("/libraries/{library_id}/assets/{sha256}", tags=["첨부 자료"])
    async def asset(library_id: IdPath, sha256: DigestPath, if_none_match: str | None = Header(None)):
        data, media_type = await asyncio.to_thread(app.state.store.read_asset, library_id, sha256)
        headers = {"ETag": f'"{sha256}"', "Cache-Control": "private, max-age=31536000, immutable",
                   "X-Content-Type-Options": "nosniff", "Content-Disposition": "attachment"}
        if if_none_match == f'"{sha256}"':
            return Response(status_code=304, headers=headers)
        return Response(data, media_type=media_type, headers=headers)

    @router.get("/libraries/{library_id}/documents/{document_id}/history", tags=["이력"])
    async def histories(library_id: IdPath, document_id: IdPath,
                        before: int = Query(MAX_CURSOR, ge=1, le=MAX_CURSOR), limit: int = Query(100, ge=1, le=500)):
        return await asyncio.to_thread(app.state.history.list, library_id, document_id, before, limit)

    @router.post("/libraries/{library_id}/documents/{document_id}/history", tags=["이력"])
    async def manual_history(library_id: IdPath, document_id: IdPath, payload: Capture):
        return await write(app.state.history.manual, library_id, document_id, payload)

    @router.get("/libraries/{library_id}/documents/{document_id}/history/{history_id}", tags=["이력"])
    async def history(library_id: IdPath, document_id: IdPath, history_id: IdPath):
        return await asyncio.to_thread(app.state.history.read, library_id, document_id, history_id)

    @router.post("/libraries/{library_id}/documents/{document_id}/history/{history_id}/restore", tags=["이력"])
    async def restore(library_id: IdPath, document_id: IdPath, history_id: IdPath, payload: Restore):
        code, result = await write(app.state.history.restore, library_id, document_id, history_id, payload)
        return JSONResponse(result, status_code=code)

    app.include_router(public)
    app.include_router(admin)
    app.include_router(router)
    return app
