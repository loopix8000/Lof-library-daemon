# Lof Library Daemon

Lof Writer용 개인 중앙 동기화 서버입니다. Rust(axum)와 SQLite로 만들었으며 Docker 컨테이너나 Windows 설치형 앱으로 실행합니다.
두 배포 방식은 같은 서버 본체를 사용합니다. Lof Writer 앱에서 서버를 연결하는 기능은 후속 작업입니다.

설치형·Docker형 배포와 기기 연결 개선 계획은 [구현 TODO](TODO.md)에 정리했습니다.

## 구성

- **동기화**: 최신 원고와 문서 속성을 저장하고 SSE로 변경을 즉시 알립니다. 변경된 문서만 받아 오며, 끊겼던 연결은 변경 커서로 이어받습니다.
- **충돌 보존**: 같은 버전을 여러 기기가 수정하면 서버 원고와 충돌 사본을 각각 보존합니다. 동일한 요청을 재전송해도 중복 저장하지 않습니다.
- **이력 관리**: 전송과 분리된 작업자가 서재별 공유 규칙으로 복구 시점을 만듭니다. 수동 기록·이전 원고 조회·복원·스마트 정리를 제공합니다.
- **첨부 자료**: SHA-256으로 구분한 불변 파일을 저장합니다. 같은 자료를 다시 올려도 중복 이벤트를 만들지 않습니다.
- **기기 연결**: 관리자가 발급한 일회용 연결 코드로 기기를 등록하고, 기기마다 별도 인증 키를 발급합니다.

외부 DB나 Git 설치는 필요하지 않습니다. 서버 이력은 SQLite의 별도 스냅샷으로 저장하며, 앱의 로컬 Git 이관과는 별개입니다.

| 폴더 | 내용 |
| --- | --- |
| `crates/lof-library-core` | 서버 본체(API, 저장소, 이력, 인증) |
| `crates/lof-library-server` | Docker·명령줄용 실행 파일 |
| `desktop` | Windows 설치형 트레이 앱(Tauri) |

## 인증과 기기 연결

- **서버 키**: 최초 기동 때 자동으로 만들어 DB에 보관합니다. 재기동·업데이트·백업 복구 후에도 기존 기기 연결이 유지됩니다.
- **관리**: 설치형 앱은 서버를 실행하는 PC가 곧 관리자이므로 로그인 없이 앱에서 관리합니다. Docker는 웹 관리용 **아이디·비밀번호**로 로그인하며, 호스트 터미널에서는 명령으로 바로 관리할 수 있습니다.
- **연결 코드**: 관리자가 발급하며 10분 동안 한 번만 쓸 수 있습니다. 서버 주소와 함께 `lof-library://connect?server=…&code=…` 형식의 연결 정보·QR로 전달합니다.
- **기기 인증 키**: 연결 코드로 등록할 때 기기마다 발급합니다. 기기 키로는 동기화만 할 수 있고, 관리자가 연결을 해제하면 즉시 사용할 수 없습니다.
- 잘못된 코드·아이디·비밀번호를 반복해서 입력하면 잠시 요청을 거부합니다.

## Docker로 실행

```sh
docker compose up -d --build
docker compose logs daemon
```

로그의 `최초 설정 코드 XXXX-XXXX`로 웹 관리자 아이디와 비밀번호를 만듭니다(`POST /v1/setup`). 이후에는 아이디와 비밀번호로 로그인합니다.

호스트 터미널에서는 로그인 없이 다음 명령으로 관리합니다. 서버를 멈출 필요는 없습니다.

```sh
# 연결 코드 발급. --server-url을 주면 연결 정보와 QR도 표시합니다.
docker compose exec daemon lof-library-server pair --name 노트북 --server-url http://192.168.0.10:8787
# 연결 기기 목록과 연결 해제
docker compose exec daemon lof-library-server devices
docker compose exec daemon lof-library-server revoke <기기 ID>
# 웹 관리자 계정을 지우고 새 최초 설정 코드 발급(비밀번호를 잊었을 때)
docker compose exec daemon lof-library-server reset-admin
```

기본 포트는 `8787`입니다. 원고·자료·설정·이력·인증 정보는 `library-data` 영구 볼륨에 함께 보관합니다.
서버 이름과 포트는 `.env.example`을 `.env`로 복사해 바꿀 수 있습니다.
인터넷에 연결할 때는 HTTPS 역방향 프록시를 앞에 둡니다. 프록시의 SSE 응답 버퍼링을 끄고 읽기 제한 시간을 15초보다 길게 설정합니다.

## Windows 설치형 앱

`desktop` 앱은 서버를 앱 안에서 실행하고 트레이에 상주합니다. Python 등 별도 런타임이 필요 없습니다.

- 창을 닫아도 서버는 계속 실행되며, 트레이 메뉴의 **종료**로 끝냅니다. 두 번 실행하면 기존 창을 엽니다.
- 로그인 없이 관리 창에서 서버 시작·중지, 접속 주소 확인, 연결 코드·QR 발급, 연결 기기 이름 변경·해제를 합니다. 네트워크로 여는 관리 API는 없습니다.
- 접속 주소는 같은 네트워크·VPN(Tailscale 등)·인터넷(공인 IP)·이 PC로 나눠 보여 줍니다. 공인 IP는 외부 조회 서비스(ipify, icanhazip)로 확인하며, 인터넷에서 접속하려면 공유기에서 포트 포워딩을 설정해야 합니다.
- 포트, 다른 기기 접속 허용, PC 시작 시 자동 실행을 설정합니다. 자동 실행 때는 창 없이 트레이에서 시작합니다.
- 데이터는 `%APPDATA%\local.lof.library\data`에 저장합니다. 업데이트·재설치해도 유지되며, 설치 제거 때 데이터 삭제 여부를 선택합니다.
- 시작할 때와 6시간마다 새 버전을 확인합니다. **설정 → 업데이트**에서 직접 확인하거나 설치할 수 있습니다. 다운로드와 서명 검증이 끝난 뒤 서버를 종료하고 업데이트를 설치합니다.

배포본 만들기(Node.js와 Rust 1.90 이상 필요):

```powershell
cd desktop
npm install
npm run package:dir
npm run package:nsis
```

`package:dir`은 `C:\Projects\lof-library-daemon\release\win-unpacked`에 실행 파일 하나로 된 폴더 배포본을 만듭니다.
`package:nsis`는 같은 폴더 배포본과 함께 설치 파일을 `C:\Projects\lof-library-daemon\release`에 복사합니다.

### GitHub에서 업데이트 배포

저장소의 **Settings → Secrets and variables → Actions**에 다음 값을 등록합니다.

| 구분 | 이름 | 값 |
| --- | --- | --- |
| Secret | `TAURI_SIGNING_PRIVATE_KEY` | 업데이트 서명 비밀키 파일 전체 내용 |
| Secret(암호를 설정한 경우) | `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 비밀키 암호 |
| Variable | `TAURI_UPDATER_PUBLIC_KEY` | 앱에 설정한 공개키와 동일한 공개키 파일 전체 내용 |

버전은 루트 `Cargo.toml`, `desktop/package.json`, `desktop/src-tauri/tauri.conf.json`을 함께 바꾸고 공개할 변경 사항을 `RELEASE_NOTES.md`에 작성합니다.
**Actions → Windows 업데이트 배포 → Run workflow**를 실행하면 서명된 NSIS 설치 파일, `.sig`, `latest.json`이 포함된 초안 릴리즈를 만듭니다.
초안의 내용을 확인하고 정식 릴리즈로 공개하면 기존 앱이 새 버전을 확인할 수 있습니다. 베타 등 사전 릴리즈는 정식 업데이트 경로에 노출되지 않습니다.
첫 배포는 설치 파일을 직접 설치해야 하며, 이후 현재 앱보다 높은 버전의 정식 릴리즈부터 업데이트할 수 있습니다.
비밀키는 소스에 포함하지 않고 계속 보관합니다. 기존 앱의 업데이트를 이어가려면 같은 서명키를 사용해야 합니다.

## 외부 연결 확인

브라우저에서 `http://서버주소:8787/status`를 열거나 다음과 같이 확인합니다. 두 상태 API는 인증이 필요 없습니다.

```sh
curl http://서버주소:8787/status
curl http://서버주소:8787/healthz
```

`/status`는 서버 이름·ID·버전, UTC 시각, 가동 시간과 DB 상태를 반환합니다.
정상이면 HTTP `200`과 `"status":"ok"`, DB를 사용할 수 없으면 HTTP `503`을 반환합니다.
외부 확인에는 실행 PC의 `localhost` 대신 NAS 주소나 공개 도메인을 사용하고 포트·프록시를 연결해야 합니다.

## 로컬 실행

Rust 1.89 이상이 필요합니다.

```powershell
cargo run -p lof-library-server -- --port 8787 --data-dir data
```

| 옵션 | 환경 변수 | 기본값 |
| --- | --- | --- |
| `--host` | `LOF_HOST` | `0.0.0.0` |
| `--port` | `LOF_PORT` | `8787` |
| `--data-dir` | `LOF_DATA_DIR` | `data` |
| | `LOF_SERVER_NAME` | 컴퓨터 이름 |

로그 수준은 `RUST_LOG`로 바꿀 수 있습니다. API 요청 예제와 재연결 순서는 [API.md](API.md)를 참고합니다.

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
- 변경 글자 수는 앞뒤 공통 부분을 제외하고 비교합니다. 비교 구간이 2만 자를 넘으면 해당 구간 전체를 바뀐 글자로 계산합니다.
- 수동 기록은 두 조건을 적용하지 않습니다. 직전 이력과 같으면 새 이력을 만들지 않고 수동 표시를 추가합니다.
- 복원은 현재 원고를 먼저 보존하고, 선택한 내용을 새 문서 버전으로 저장합니다.
- 스마트 정리는 UTC 기준 고정 구간을 사용하며 수동 저장·복원 기록을 합치지 않습니다. 구간의 최신 자동 이력을 남기고 나머지를 삭제합니다.
- 기간 정리를 켜도 문서별 최신 이력 하나는 유지합니다. 정리 때문에 최신 원고·동기화 버전·변경 커서가 바뀌지 않습니다.
- 서버가 10초마다 이력 생성 조건을 확인하고 1시간마다 보관 규칙을 적용합니다. 이는 외부 저장소를 조회하는 주기가 아닙니다.
- 설정은 서버의 서재 단위로 공유합니다. 설정 버전을 확인하여 다른 기기의 변경을 덮어쓰지 않습니다.

## 개발·운영

```powershell
cargo test -p lof-library-core -p lof-library-server
```

실행 중인 서버는 SQLite 온라인 백업으로 보관합니다. 기존 백업 파일은 덮어쓰지 않습니다.

```sh
docker compose exec daemon lof-library-server backup /data/backup-20261018.sqlite
docker compose cp daemon:/data/backup-20261018.sqlite ./backup-20261018.sqlite
```

복구할 때는 서버를 멈추고 백업을 새 데이터 볼륨의 `library.sqlite`로 복사한 뒤 실행합니다.
실행 중인 DB 파일 하나만 복사하면 WAL의 최근 변경을 빠뜨릴 수 있으므로 위 백업 명령을 사용합니다.

데이터 폴더를 공유하는 여러 프로세스는 실행을 거부합니다.
첨부 자료·충돌 사본·변경 알림·재전송 영수증은 자동 삭제하지 않습니다. 이력 정리는 이력 스냅샷에만 적용하며, SQLite의 빈 공간은 이후 저장에 재사용합니다.
