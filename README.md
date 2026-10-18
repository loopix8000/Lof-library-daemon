# Lof Library

Writer와 다른 Library를 연결하는 **실시간 파일 동기화 앱**입니다. Windows 앱 또는 Docker·명령줄로 실행합니다. 서재의 Markdown 원본·작품 정보·이미지·첨부 파일을 폴더 구조 그대로 보관합니다.

## 설치와 업데이트

Windows 설치 파일은 [GitHub 릴리즈](https://github.com/loopix8000/Lof-library-daemon/releases)에서 받습니다. 앱은 시작할 때와 6시간마다 새 버전을 확인하고, 설치는 사용자가 선택합니다.

## Windows에서 연결

1. 앱의 **서재 → 서재 만들기**에서 보관할 서재를 만듭니다.
2. 다른 기기를 초대하려면 **연결 코드 만들기**에서 서재를 선택하고 QR·코드를 발급합니다. 다른 Writer나 Library의 **다른 기기 연결** 화면에 입력합니다.
3. 이 Library에서 먼저 연결하려면 **다른 기기 연결**에서 내 서재를 고르고 상대 기기의 코드를 입력합니다.
4. **연결된 기기**에서 실제 연결 상태와 최근 동기화, 기기 이름을 확인하거나 연결을 해제합니다.

초대는 서재별로 발급하며 10분 동안 한 번만 사용할 수 있습니다.

Writer는 구름 메뉴에서 연결합니다. 실시간 동기화는 Writer 1.0.4 베타 1 이상이 필요하며, Library에서 Writer의 코드를 입력하는 연결에는 보관 피어 역할을 지원하는 최신 Writer 빌드가 필요합니다.

## 파일 보관과 동기화

- 데이터: `data/realtime/<서재 UUID>/`에 원본 파일을 보관합니다. Windows에서는 설정의 데이터 폴더 열기 또는 서재별 폴더 열기로 확인할 수 있습니다.
- 파일 감시 이벤트를 모아 5초 단위로 변경을 전달하고, 변경이 없으면 파일 검사와 동기화 요청을 반복하지 않습니다. 연결 유지·주소 조회는 Iroh가 담당합니다.
- 연결이 끊기면 5초부터 시작해 실패할 때마다 두 배씩(최대 60초) 간격을 늘려 다시 연결하고, 각 피어에 저장한 마지막 파일 해시로 추가·수정·삭제를 비교합니다. 상대 기기가 변경을 알리면 대기를 끊고 바로 확인합니다.
- 동시에 수정한 파일은 내용 해시로 주본을 일치시키고 다른 원본은 비교한 피어의 `.lof/file-conflicts`에 보관합니다. 삭제와 편집이 겹치면 편집한 파일을 보존합니다.
- Library의 덮어쓰기·삭제 전 원본은 해당 서재의 `.lof/history`에 보관합니다. 기기별 관리 DB·설정·숨김 파일은 공유하지 않습니다.
- 피어 연결은 선택한 서재에만 허용합니다. 연결을 해제해도 이미 상대에게 전달한 파일은 상대 장치에 남습니다.

이전 프로토콜의 API·로그인·페어링은 제거했습니다. 웹 관리 화면은 새 방식으로 다시 제공합니다. 예전 DB와 원고 파일을 자동으로 삭제하거나 새 서재에 합치지 않습니다.

## Docker와 웹 관리 화면

```sh
docker compose up -d --build
docker compose logs daemon
```

1. 로그의 `웹 관리 화면 최초 설정 코드: XXXX-XXXX`를 확인합니다.
2. 브라우저에서 `http://서버주소:8787`을 열고 최초 설정 코드와 관리자 아이디·비밀번호(8자 이상)를 입력해 계정을 만듭니다.
3. 이후에는 Windows 앱과 같은 화면에서 서재 만들기, QR·코드 초대, 다른 기기 연결, 기기 이름 변경·연결 해제를 합니다.

웹 관리 화면은 Windows 앱의 관리 화면을 그대로 사용하되, 서버 PC에서만 의미 있는 기능(동기화 시작·중지, 자동 실행, 업데이트, 폴더·로그 열기)은 제공하지 않습니다. 동기화는 컨테이너가 실행되는 동안 계속됩니다.

- 계정은 데이터 폴더의 `web-auth.json`에 비밀번호 해시로 저장합니다. 비밀번호를 잊었다면 `docker compose exec daemon lof-library-server web-reset`으로 계정을 지우고 `docker compose restart daemon` 뒤 새 최초 설정 코드로 다시 만듭니다.
- 로그인은 12시간 유지되며, 로그인에 10번 연속 실패하면 1분 동안 잠깁니다.
- 웹 화면은 HTTP입니다. 인터넷에 공개하려면 HTTPS를 처리하는 리버스 프록시 뒤에 두고, 같은 PC에서만 쓰려면 `compose.yaml`의 포트를 `127.0.0.1:8787:8787`로 바꿉니다. 바이너리를 직접 실행할 때는 `--web-addr 0.0.0.0:8787`(또는 `LOF_WEB_ADDR`)을 지정해야 웹 화면이 켜집니다. 지정하지 않으면 열리지 않습니다.

웹 화면 대신 명령줄로도 관리할 수 있습니다.

```sh
docker compose exec daemon lof-library-server create '내 서재'
docker compose exec daemon lof-library-server invite <서재 UUID>
```

다른 피어에 먼저 연결하려면 표준 입력으로 연결 코드를 전달합니다.

```sh
docker compose exec daemon lof-library-server pair <내 서재 UUID>
# 연결 코드를 입력한 뒤 Enter
```

명령줄 관리는 컨테이너 내부의 Unix 소켓을 사용합니다. Windows 명령줄 관리에는 같은 사용자 전용 명명 파이프를 사용합니다. 웹 관리 화면 외의 네트워크 관리 API는 제공하지 않습니다. UDP 직접 연결이 가능한 환경에서는 자동으로 직접 연결을 사용하며, 외부 포트를 노출하지 않은 Docker에서도 릴레이를 통해 연결할 수 있습니다.

## 명령줄

```text
lof-library-server --data-dir <폴더>                 피어 시작
lof-library-server --data-dir <폴더> status          실행 상태
lof-library-server --data-dir <폴더> libraries       서재 목록
lof-library-server --data-dir <폴더> create <이름>    서재 생성
lof-library-server --data-dir <폴더> invite <서재>    QR·초대 코드
lof-library-server --data-dir <폴더> pair <서재>      다른 기기의 코드로 연결
lof-library-server --data-dir <폴더> devices         연결된 기기
lof-library-server --data-dir <폴더> revoke <서재> <기기 공개키>
lof-library-server --data-dir <폴더> cancel-invite <서재>  발급한 초대 취소
lof-library-server --data-dir <폴더> rename <서재> <이름>  서재 이름 변경
lof-library-server --data-dir <폴더> healthcheck     실행 상태 점검(Docker 헬스체크)
lof-library-server --data-dir <폴더> web-reset       웹 관리자 계정 삭제
lof-library-server --data-dir <폴더> --web-addr <주소:포트>  웹 관리 화면과 함께 시작
lof-library-server --data-dir <폴더> stop            동기화 중지
lof-library-server --data-dir <폴더> backup <새 폴더> 전체 데이터 백업
```

`--data-dir`을 생략하면 `LOF_DATA_DIR` 환경 변수를, 둘 다 없으면 `data` 폴더를 사용합니다. Docker 이미지는 `/data`를 사용합니다.

백업 전에 피어를 중지해야 합니다. 원본·파일 이력·관리 DB를 포함한 새 폴더를 만들며 기존 경로는 덮어쓰지 않습니다.

## 개발

Rust 1.91 이상이 필요합니다. Windows 관리 앱에는 Tauri 2 빌드 환경과 Node.js가 필요합니다.

```sh
cargo check --workspace --locked
cd desktop
npm ci
npm run package:dir
```

관리 앱의 폴더 배포본은 `C:\Projects\lof-library-daemon\release\win-unpacked`를 갱신합니다. 앱 버전은 명시적으로 변경할 때만 올립니다.

프로토콜은 [API.md](API.md)에 정리했습니다. Iroh의 공개 릴레이는 별도 서비스이며 Library가 릴레이 서버가 되는 것은 아닙니다.
