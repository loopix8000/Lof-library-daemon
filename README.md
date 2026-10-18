# Lof Library Daemon

Lof Writer용 개인 중앙 동기화 서버입니다. FastAPI와 SQLite를 사용하며 Docker 컨테이너 하나로 실행합니다.
현재 서버 API를 구현한 초기 버전입니다. Lof Writer 앱에서 서버를 연결하는 기능은 후속 작업입니다.

설치형·Docker형 배포와 기기 연결 개선 계획은 [구현 TODO](TODO.md)에 정리했습니다.

## 구성

- **동기화**: 최신 원고와 문서 속성을 저장하고 SSE로 변경을 즉시 알립니다. 변경된 문서만 받아 오며, 끊겼던 연결은 변경 커서로 이어받습니다.
- **충돌 보존**: 같은 버전을 여러 기기가 수정하면 서버 원고와 충돌 사본을 각각 보존합니다. 동일한 요청을 재전송해도 중복 저장하지 않습니다.
- **이력 관리**: 전송과 분리된 작업자가 서재별 공유 규칙으로 복구 시점을 만듭니다. 수동 기록·이전 원고 조회·복원·스마트 정리를 제공합니다.
- **첨부 자료**: SHA-256으로 구분한 불변 파일을 저장합니다. 같은 자료를 다시 올려도 중복 이벤트를 만들지 않습니다.

외부 DB나 Git 설치는 필요하지 않습니다. 서버 이력은 SQLite의 별도 스냅샷으로 저장하며, 앱의 로컬 Git 이관과는 별개입니다.

## Docker로 실행

1. `.env.example`을 `.env`로 복사합니다.
2. 아래 명령으로 토큰을 만들고 `.env`의 `LOF_API_TOKEN`에 넣습니다.

```powershell
$env:PYTHONIOENCODING='utf-8'
python -c "import secrets; print(secrets.token_urlsafe(32))"
```

3. 서버를 실행합니다.

```sh
docker compose up -d --build
```

기본 포트는 `8787`입니다. 원고·자료·설정·이력은 `library-data` 영구 볼륨에 함께 보관합니다.
설정한 토큰은 연결하는 모든 앱에서 사용합니다. 인터넷에 연결할 때는 HTTPS 역방향 프록시를 앞에 둡니다.
프록시의 SSE 응답 버퍼링을 끄고 읽기 제한 시간을 15초보다 길게 설정합니다.

## 외부 연결 확인

브라우저에서 `http://서버주소:8787/status`를 열거나 다음과 같이 확인합니다. 두 상태 API는 인증이 필요 없습니다.

```sh
curl http://서버주소:8787/status
curl http://서버주소:8787/healthz
```

`/status`는 서버 이름, 버전, UTC 시각, 가동 시간과 DB 상태를 반환합니다.
정상이면 HTTP `200`과 `"status":"ok"`, DB를 사용할 수 없으면 HTTP `503`을 반환합니다.
`/healthz`는 Docker 상태 확인에 사용하는 간단한 API입니다.
외부 확인에는 실행 PC의 `localhost` 대신 NAS 주소나 공개 도메인을 사용하고 포트·프록시를 연결해야 합니다.

## 로컬 실행

Python 3.11 이상이 필요합니다. PowerShell 기준입니다.

```powershell
$env:PYTHONIOENCODING='utf-8'
python -m venv .venv
.venv\Scripts\python -m pip install -r requirements.lock
.venv\Scripts\python -m pip install --no-deps -e .
$env:LOF_API_TOKEN = .venv\Scripts\python -c "import secrets; print(secrets.token_urlsafe(32))"
.venv\Scripts\python -m lof_daemon
```

API 문서는 `http://localhost:8787/docs`에 있습니다. 문서 화면의 **Authorize**에 서버 토큰을 입력합니다.
동기화·이력 API는 모두 `Authorization: Bearer <토큰>` 헤더가 필요합니다.
로컬 데이터는 기본 `data/`에 저장하며 `LOF_DATA_DIR`로 변경할 수 있습니다.

## 이력 규칙

| 설정 | 기본값 | 의미 |
| --- | --- | --- |
| `min_minutes` | 10 | 마지막 이력 생성 이후 최소 경과 시간 |
| `min_changed_chars` | 200 | 마지막 이력과 비교한 본문의 추가·삭제 글자 수 합계 |
| `retention_days` | `null` | 보관 일수. `null`이면 기간 제한 없음 |
| `expire_protected` | `false` | 수동 저장·복원 기록에도 보관 기간을 적용할지 여부 |
| `smart.enabled` | `false` | 오래된 자동 이력의 구간별 정리 사용 |
| `smart.keep_recent_minutes` | 60 | 스마트 정리에서 모든 이력을 유지하는 최근 시간 |
| `smart.bucket_minutes` | 10 | 그보다 오래된 자동 이력에서 최신 하나를 남기는 구간 |

- 시간과 변경 글자 수 조건을 **모두** 만족해야 자동 이력을 만듭니다. 전송은 이 조건을 기다리지 않습니다.
- 처음 등록한 문서는 기준 이력을 하나 만듭니다. 이후 입력이 없거나 원래 본문으로 돌아오면 자동 이력이 생기지 않습니다.
- 수동 기록은 두 조건을 적용하지 않습니다. 직전 이력과 같으면 새 이력을 만들지 않고 수동 표시를 추가합니다.
- 복원은 현재 원고를 먼저 보존하고, 선택한 내용을 새 문서 버전으로 저장합니다.
- 스마트 정리는 UTC 기준 고정 구간을 사용하며 수동 저장·복원 기록을 합치지 않습니다. 구간의 최신 자동 이력을 남기고 나머지를 삭제합니다.
- 기간 정리를 켜도 문서별 최신 이력 하나는 유지합니다. 정리 때문에 최신 원고·동기화 버전·변경 커서가 바뀌지 않습니다.
- 서버가 10초마다 이력 생성 조건을 확인하고 1시간마다 보관 규칙을 적용합니다. 이는 외부 저장소를 조회하는 주기가 아닙니다.
- 설정은 서버의 서재 단위로 공유합니다. 설정 버전을 확인하여 다른 기기의 변경을 덮어쓰지 않습니다.

## 개발·운영

API 요청 예제와 재연결 순서는 [API.md](API.md)를 참고합니다.

```powershell
$env:PYTHONIOENCODING='utf-8'
.venv\Scripts\python -m pip install -r requirements-dev.lock
.venv\Scripts\python -m unittest discover -s tests -v
```

실행 중인 서버는 SQLite 온라인 백업으로 보관합니다. 기존 백업 파일은 덮어쓰지 않습니다.

```sh
docker compose exec daemon python -m lof_daemon.backup /data/backup-20261018.sqlite
docker compose cp daemon:/data/backup-20261018.sqlite ./backup-20261018.sqlite
```

복구할 때는 데몬을 멈추고 백업을 새 데이터 볼륨의 `library.sqlite`로 복사한 뒤 실행합니다.
실행 중인 DB 파일 하나만 복사하면 WAL의 최근 변경을 빠뜨릴 수 있으므로 위 백업 명령을 사용합니다.

현재는 개인 서버용 공통 토큰 인증과 단일 작업자 구성입니다. 데이터 폴더를 공유하는 여러 프로세스는 실행을 거부합니다.
첨부 자료·충돌 사본·변경 알림·재전송 영수증은 자동 삭제하지 않습니다. 이력 정리는 이력 스냅샷에만 적용하며, SQLite의 빈 공간은 이후 저장에 재사용합니다.
