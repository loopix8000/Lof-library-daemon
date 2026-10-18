# Iroh 파일 동기화 프로토콜

ALPN: `lof/realtime-files/1`. QUIC의 인증된 공개키를 피어 ID로 사용합니다. 연결·암호화·주소 조회·릴레이는 Iroh 1.3.0의 N0 프리셋을 사용합니다.

## 프레임

양방향 QUIC 스트림마다 요청 하나와 응답 하나를 교환합니다. big-endian u32 헤더 길이, JSON `{meta,length}`, 원본 파일 바이트 순서입니다. 파일 본문은 JSON·Base64로 변환하지 않습니다. 헤더는 8MB, 파일은 50MB, 요청은 90초로 제한합니다.

## 연결

`lof-peer://<base64url(JSON)>` 초대는 `{version:1,addr,vault,token}`을 담습니다. 토큰은 10분 동안 한 번만 사용할 수 있고 발급 기기의 메모리에 보관합니다. `pair`는 `{op:"pair",vault,token,name,storage}`를 보내며 `vault`는 발신 서재 UUID, `storage`는 Library 여부입니다. 응답은 `{vault,name,storage}`입니다.

이후 모든 요청은 `vault`(발신 서재), `target`(수신 서재)을 포함합니다. 수신 피어는 인증된 공개키와 두 UUID의 조합으로 접근 권한을 매번 확인합니다. 파일이 존재해도 선택한 서재 외의 파일에는 접근할 수 없습니다.

## 파일 교환

- `hello`: 연결 확인
- `notify`: 해당 서재의 변경 알림
- `begin`: 서재별 동기화 임대 획득
- `inventory`: 경로별 `{size,modified,etag}` 목록. ETag는 파일 SHA-256
- `read`: `path` 원본 바이트
- `write`: `path`, `expected`(이전 SHA-256 또는 null), 원본 바이트
- `remove`: `path`, `expected` SHA-256
- `touch`: 임대 갱신
- `end`: 임대 해제와 동기화 완료

`.lof-sync-id`는 서재 UUID를 읽는 가상 항목이며 파일로 쓰지 않습니다. 임대는 180초이며 요청·작업 진행 시 갱신합니다. 쓰기와 삭제 직전 내용 해시를 확인하고 다르면 비교를 다시 시도합니다. 연결 해제 시 임대를 해제합니다.

Writer와 Library 사이에서는 Writer가 파일 비교를 담당합니다. Library끼리는 공개키 문자열 순서가 작은 피어가 비교합니다. 처음 연결한 피어는 양쪽 파일을 합치며, 이후에는 피어별 로컬 체크포인트로 삭제를 판단합니다.

## 호스트 관리

네트워크 관리 서버는 없습니다. Windows는 원격 연결을 거부하는 명명 파이프, Unix는 권한 0600의 소켓을 사용합니다. 요청은 줄바꿈으로 끝나는 JSON `{action,args}`이며 응답은 `{ok,data}` 또는 `{ok:false,error}`입니다. 코드 발급·연결·서재·기기 관리에는 동일한 본체를 사용합니다.
