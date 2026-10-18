# API v1

`/status`, `/healthz`와 아래 인증 API를 제외한 `/v1` 요청은 `Authorization: Bearer <인증 키>` 헤더가 필요합니다.
인증 키는 기기 인증 키(`lofd_…`)나 웹 관리자 로그인 키(`lofs_…`)입니다. 기기 인증 키로는 동기화만 할 수 있고, `/v1/admin` API는 웹 관리자 로그인 키가 필요합니다.
요청은 최대 8 MiB이고, 본문은 최대 100만 Unicode 글자입니다. 시간 값은 UTC Unix 초입니다.
서재·문서·기기·요청 ID에는 영문, 숫자, `_`, `-`를 사용하며 최대 128자입니다.
오류 응답은 `{"error":"코드","message":"안내 문구"}` 형식입니다. 요청 값이 규칙에 맞지 않으면 `422 invalid_request`입니다.

## 인증과 기기 연결

| 요청 | 인증 | 설명 |
| --- | --- | --- |
| `POST /v1/pair` | 없음 | `{"code":"ABCD-EFGH","name":"노트북","platform":"windows"}`로 기기 등록 |
| `GET /v1/me` | 모든 키 | 현재 키의 종류(`device`·`session`)·기기 ID와 서버 정보 |
| `POST /v1/setup` | 없음 | 웹 관리 전용. `{"code":"ABCD-EFGH","username":"admin","password":"…"}`로 관리자 계정 생성. 로그인 키 반환 |
| `POST /v1/auth/login` | 없음 | 웹 관리 전용. `{"username":"admin","password":"…"}`로 로그인. 12시간 유효한 로그인 키 반환 |
| `POST /v1/auth/logout` | 로그인 키 | 현재 로그인 종료 |

웹 관리(`/v1/setup`, `/v1/auth/login`, `/v1/admin`)는 Docker 서버에서만 열립니다. 설치형 앱은 로그인 없이 앱 안에서 관리하므로 이 API가 없고 `404`를 반환합니다.
최초 설정 코드는 관리자 계정이 없을 때 Docker 서버 시작 로그에 표시되며, 계정을 만든 뒤에는 다시 쓸 수 없습니다.
아이디는 영문·숫자·`_`·`-`·`.`로 3~32자, 비밀번호는 8~256자입니다. 아이디나 비밀번호가 틀리면 어느 쪽인지 구분하지 않고 `401 invalid_credentials`를 반환합니다.
코드·비밀번호를 10분 동안 10번 틀리면 `429 too_many_attempts`를 반환합니다.

`POST /v1/pair`의 응답:

```json
{
  "device": {"id":"…","name":"노트북","platform":"windows","created_at":1790000000.0,"last_seen_at":1790000000.0},
  "token": "lofd_…",
  "server": {"id":"…","name":"NAS","version":"0.1.0"}
}
```

`token`은 이 응답에서 한 번만 받으므로 기기에 안전하게 보관합니다. 동기화 요청의 `device_id`에는 응답의 기기 ID를 사용할 수 있습니다.
코드가 없으면 `404 pairing_code_invalid`, 이미 썼으면 `410 pairing_code_used`, 만료됐으면 `410 pairing_code_expired`입니다.

연결 정보는 `lof-library://connect?server=<URL 인코딩한 서버 주소>&code=<연결 코드>` 형식입니다. QR에도 같은 문자열을 넣습니다.

## 웹 관리

Docker 서버에서 웹 관리자로 로그인한 키로만 사용합니다.

| 요청 | 설명 |
| --- | --- |
| `GET /v1/admin/server` | 서버 ID·이름·버전, 관리자 아이디, 가동 시간, 저장 공간, 서재·문서·기기 수, 실시간 연결 수 |
| `PUT /v1/admin/password` | `{"current_password":"…","new_password":"…"}`. 모든 로그인을 끝내고 새 로그인 키 반환 |
| `GET /v1/admin/devices` | 연결 기기 목록과 실시간 연결 여부(`connected`), 마지막 접속 시각 |
| `PATCH /v1/admin/devices/{기기}` | `{"name":"새 이름"}` |
| `DELETE /v1/admin/devices/{기기}` | 연결 해제. 해당 기기의 키와 실시간 수신이 즉시 끊깁니다 |
| `POST /v1/admin/pairing-codes` | `{"name":"노트북","server_url":"http://192.168.0.2:8787"}`로 연결 코드 발급 |
| `GET /v1/admin/pairing-codes` | 사용 전이고 만료되지 않은 코드 목록(코드 값은 제외) |
| `DELETE /v1/admin/pairing-codes/{코드 ID}` | 사용 전 코드 취소 |

연결 코드 발급 응답에는 `code`, `expires_at`, `server_url`, `connection`(연결 정보)이 들어 있습니다. 코드 값은 이때만 받을 수 있습니다.
`server_url`을 생략하면 요청한 주소를 사용합니다.

## 서재 만들기

`POST /v1/libraries`

```json
{"id":"my-library","name":"내 서재"}
```

`GET /v1/libraries`로 연결할 서재 목록을 조회합니다. 문서의 `kind`는 `episode`, `memo`, `plot`, `project`입니다.
작품 정보는 `project` 문서의 `metadata`에 넣고 다른 문서가 해당 작품 ID를 참조할 수 있습니다.

## 연결과 재연결

1. `GET /v1/libraries/{서재}/snapshot`으로 문서·첨부 목록, 공유 설정과 `cursor`를 함께 받습니다. 이 목록에는 본문이 없습니다.
2. 캐시에 없는 문서나 SHA-256이 다른 문서만 `GET /v1/libraries/{서재}/documents/{문서}`로 받습니다.
3. 목록에서 받은 커서로 `GET /v1/libraries/{서재}/events?after={cursor}`를 구독합니다. 본문을 받는 동안 발생한 변경도 이어서 전달됩니다.
4. 변경 이벤트를 받으면 필요한 문서를 다시 읽습니다. 적용을 완료한 이벤트 ID를 기기에 저장합니다.
5. 끊겼다가 다시 연결할 때 `after` 또는 `Last-Event-ID` 헤더로 마지막 적용 ID를 보냅니다. 헤더를 보내면 헤더 값이 우선합니다.

`events`는 SSE이며, 인증 헤더를 붙일 수 있는 스트리밍 클라이언트를 사용합니다. 토큰을 URL에 넣지 않습니다.
15초마다 연결 유지 신호를 보냅니다. 이벤트를 읽을 때 서버가 최신 원고를 직접 전송하는 것은 아닙니다.

```text
id: 42
event: document.updated
data: {"id":42,"type":"document.updated","data":{"id":"doc-1","version":2,"sha256":"..."},"created_at":1790000000.0}
```

짧은 요청으로 확인하려면 `GET /v1/libraries/{서재}/changes?after={cursor}&limit=100`을 사용합니다.
응답의 `has_more`가 참이면 응답 `cursor`부터 다음 페이지를 읽습니다. 커서는 서버 전체의 단조 증가 번호여서 중간 번호가 비어 있을 수 있습니다.
서버 백업 복구 등으로 커서가 현재 서버보다 앞서면 `409 cursor_ahead`를 반환하므로 서재 목록부터 다시 받습니다.
본문·설정 등을 실제 적용하기 전에 커서를 앞당기지 않습니다.

## 원고 보내기

`PUT /v1/libraries/{서재}/documents/doc-1`

```json
{
  "device_id":"desktop-1",
  "mutation_id":"5f2ee2e2-a3ec-4ddb-8259-92b84f97f122",
  "expected_version":0,
  "kind":"episode",
  "title":"첫 번째 원고",
  "body":"원고 본문",
  "metadata":{"project_id":"project-1"},
  "deleted":false
}
```

`mutation_id`는 UUID처럼 요청마다 고유한 값으로 만듭니다.
새 문서는 `expected_version: 0`, 수정할 때는 마지막으로 확인한 서버 문서 버전을 사용합니다.
저장마다 `version`이 증가하고 같은 내용은 증가하지 않습니다.
쓰기 응답은 문서 ID·버전·해시·삭제 여부가 담긴 영수증입니다. 본문은 GET으로 받습니다.

연결 실패 후 재전송할 때는 같은 기기 ID·요청 ID·내용을 그대로 사용합니다. 서버가 처음 처리한 결과를 돌려줍니다.
나중 수정에는 새 요청 ID를 만듭니다. 다른 기기의 ID와 섞이지 않도록 기기 ID도 영구 보관합니다.
오프라인 변경에는 그 편집을 시작한 서버 버전을 계속 사용하여 다른 기기의 변경을 확인할 수 있게 합니다.

삭제는 같은 API에 `deleted: true`로 보냅니다. 기존 서버 본문을 보존한 삭제 표시를 남기며 목록에서도 유지합니다.
GET 응답의 `ETag`를 다음 GET의 `If-None-Match`로 보내면 변경이 없을 때 `304`를 반환합니다.

## 충돌

버전이 다르면 서버 원고를 덮어쓰지 않고 `409 revision_conflict`와 `conflict_id`를 반환합니다.
입력한 원고는 충돌 사본으로 별도 보존됩니다. 같은 요청을 재전송해도 같은 충돌 ID를 반환합니다.

- `GET /v1/libraries/{서재}/conflicts`: 미해결 충돌 목록. `after`에 직전 페이지 마지막 ID를 넣습니다.
- `GET /v1/libraries/{서재}/conflicts/{충돌}`: 서버 버전과 충돌 사본 조회.
- `POST /v1/libraries/{서재}/conflicts/{충돌}/resolve`: 아래 요청으로 해결.

```json
{"device_id":"desktop-1","mutation_id":"resolve-uuid","expected_version":2,"choice":"incoming"}
```

`incoming`은 충돌 사본을 새 서버 버전으로 적용하고, `current`는 현재 서버 원고를 유지합니다.
해결 전에도 현재 서버 버전이 바뀌었는지 확인합니다. 해결된 사본도 서버에서 바로 삭제하지 않습니다.

## 공유 이력 설정

`GET /v1/libraries/{서재}/settings`로 설정과 `settings_version`을 읽습니다.
`PUT /v1/libraries/{서재}/settings`로 이력 정책 전체를 보냅니다. 일부 항목을 생략하면 그 항목은 기본값으로 돌아갑니다.

```json
{
  "expected_version":1,
  "history":{
    "min_minutes":10,
    "min_changed_chars":200,
    "retention_days":null,
    "expire_protected":false,
    "smart":{"enabled":false,"keep_recent_minutes":60,"bucket_minutes":10}
  }
}
```

다른 기기가 먼저 바꿨으면 `409 settings_conflict`입니다. 설정 변경은 `settings.updated` 이벤트로 모든 기기에 알려집니다.
서버만 이력을 생성하고 정리하므로 기기마다 설정을 따로 실행하지 않습니다.

## 이력 조회·수동 기록·복원

- `GET /v1/libraries/{서재}/documents/{문서}/history`: 최신순 목록. `limit`은 최대 500입니다.
- 이전 페이지는 목록의 마지막 `cursor`를 `before`에 넣어 조회합니다.
- `GET /v1/libraries/{서재}/documents/{문서}/history/{이력}`: 해당 시점의 전체 문서.
- `POST /v1/libraries/{서재}/documents/{문서}/history`: `{"expected_version":2,"device_id":"desktop-1"}`로 수동 기록.
- `POST /v1/libraries/{서재}/documents/{문서}/history/{이력}/restore`: `{"expected_version":2,"device_id":"desktop-1","mutation_id":"restore-uuid"}`로 복원.

이력 생성·표시 변경·정리는 각각 `history.created`, `history.marked`, `history.pruned`로 알립니다.
이력 본문은 최신 원고 동기화에 포함하지 않으며, 이력 API를 호출할 때만 전달합니다.

## 첨부 자료

`PUT /v1/libraries/{서재}/assets/{SHA-256}`에 원본 바이트와 `Content-Type`을 보냅니다.
서버에서 해시를 검증하므로 주소와 내용이 다르면 `422 hash_mismatch`입니다.
`GET`으로 같은 주소에서 받습니다. 문서 `metadata`에 필요한 해시와 원래 파일명을 기록합니다.
수정된 자료는 새 해시로 올리며 이전 주소의 내용을 덮어쓰지 않습니다.

## 상태 확인

`GET /status`는 인증 없이 외부 연결을 확인하기 위한 API입니다.

```json
{"service":"Lof Library Daemon","version":"0.1.0","server_id":"…","name":"NAS","status":"ok","database":"ok","time":"2026-10-18T08:00:00Z","uptime_seconds":120}
```

`GET /healthz`는 정상일 때 `{"status":"ok"}`를 반환합니다. 두 API는 DB 장애 시 HTTP 503을 반환합니다.
