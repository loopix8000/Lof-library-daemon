# Iroh 파일 동기화 프로토콜

ALPN: `lof/realtime-files/1`. QUIC의 인증된 공개키를 피어 ID로 사용합니다. 연결·암호화·주소 조회·릴레이는 Iroh 1.3.0의 N0 프리셋을 사용합니다. 그룹 알림은 같은 엔드포인트의 iroh-gossip 0.101.0(`/iroh-gossip/1`)을 사용합니다.

## 프레임

양방향 QUIC 스트림마다 요청 하나와 응답 하나를 교환합니다. big-endian u32 헤더 길이, JSON `{meta,length}`, 원본 파일 바이트 순서입니다. 파일 본문은 JSON·Base64로 변환하지 않습니다. 헤더는 8MB, 파일은 50MB, 요청은 90초로 제한합니다.

## 연결

서재 하나는 동기화 그룹 하나에 속합니다. 그룹은 32바이트 비밀값으로 정의하며, 그룹 ID는 `SHA-256("lof-group-id/1"‖비밀값)`의 앞 16바이트(16진수), gossip 주제는 `SHA-256("lof-group-topic/1"‖비밀값)`입니다.

구성원 기록은 `{endpoint,vault,name,storage,version,removed,by,sig}`입니다. `by` 기기의 키로 JSON `["lof-group-member/1",그룹 ID,endpoint,vault,name,storage,version,removed,by]`에 Ed25519 서명합니다. 서명이 맞고 `by`가 현재 구성원인 기록만 반영하며 버전이 높은 기록, 같은 버전이면 제거 기록이 이깁니다. 그룹은 32대까지입니다.

페어링 코드 `lof-group://<base64url(JSON)>`은 `{version:2,group,addr,token}`을 담습니다. 비밀값은 넣지 않습니다. 토큰은 10분 동안 한 번만 사용할 수 있고 발급 기기의 메모리에 보관합니다. 이전 `lof-peer://` 코드는 거부합니다. `join`은 `{op:"join",group,vault,token,name,storage}`를 보내며 `vault`는 발신 서재 UUID, `storage`는 Library 여부입니다. 초대 기기는 가입 기기의 구성원 기록을 서명해 추가하고 `{group,secret,members,vault,name,storage}`를 응답합니다. 가입 기기는 비밀값에서 계산한 ID가 코드와 같은지 확인합니다.

이후 모든 요청은 `vault`(발신 서재), `target`(수신 서재)을 포함합니다. 수신 피어는 인증된 공개키가 대상 서재 그룹의 현재 구성원이고 발신 서재 UUID가 기록과 같은지 매번 확인합니다. 파일이 존재해도 선택한 서재 외의 파일에는 접근할 수 없습니다.

## 그룹 알림

- `hello`·`members` 요청은 `members` 목록을 함께 보냅니다. 받는 피어는 허가 확인 전에 목록을 반영하므로, 다른 구성원이 추가한 새 기기도 처음 접속할 때 인증됩니다. `hello` 응답에도 목록을 넣습니다.
- 제거된 기기의 `hello`에는 `{removed:true,members}`를 응답합니다. 자신이 제거된 기록을 받은 기기는 해당 서재의 그룹 정보를 지웁니다.
- gossip 메시지는 `{type:"members",members}`와 `{type:"changed",endpoint}`뿐이며 원고 파일은 보내지 않습니다. `changed`와 새 이웃 발견은 재시도 대기만 건너뛰게 합니다.

## 파일 교환

- `hello`: 연결 확인과 구성원 목록 교환
- `notify`: 해당 서재의 변경 알림
- `begin`: 서재별 동기화 임대 획득
- `inventory`: 경로별 `{size,modified,etag}` 목록. ETag는 파일 SHA-256
  - `hello`의 `capabilities`에 `inventory-delta-v1`과 `inventory-index-v1`을 알립니다.
  - 요청에 `delta:true`와 이전 목록의 요약값 `known`을 보내면 `{format:"inventory-delta-v1",digest,base,changed,removed}`로 바뀐 항목과 삭제된 경로만 응답합니다.
  - 첫 요청이나 기준 목록이 다르면 `{format:"inventory-delta-v1",digest,files}`로 전체 목록을 보냅니다. 요약값은 경로순 목록과 Writer의 `{size,modified,etag,unavailable}` 필드 순서로 직렬화한 JSON의 SHA-256입니다.
  - 받은 목록의 요약값이 다르면 전체 목록을 한 번 다시 요청합니다. 이전 버전과는 기존 전체 목록 형식으로 통신합니다. 캐시는 메모리에만 보관하며 재시작 시 첫 목록은 전체를 받습니다.
  - `index:true` 요청에는 항목별 `index:{version,identity}`를 포함합니다. `version`은 ETag와 같은 원본 SHA-256이며, `identity`는 `document:문서ID`, `project:작품ID` 또는 null입니다. 같은 원본을 받은 기기는 같은 버전을 유지합니다.
  - `delta:true,index:true`의 응답 형식은 `inventory-index-v1`입니다. 요약값은 기존 필드 뒤에 `index`를 포함해 계산합니다. `index`를 요청하지 않은 구형 피어에는 추가 필드를 보내지 않습니다.
  - 새 피어는 구형 피어의 `inventory-delta-v1`과 전체 목록 응답도 처리합니다. 색인 정보가 없거나 버전이 ETag와 맞지 않는 항목만 기존 원본 읽기·해시 비교를 사용합니다.
  - 문서 헤더·내용 해시·정밀한 파일 상태는 각 서재의 `.lof/sync-index.sqlite`에 보관합니다. 파일 상태나 변경 알림이 달라진 항목만 다시 읽고, 실제 쓰기·삭제 직전에는 기존 원본 해시를 다시 검증합니다. 관리 색인은 동기화하지 않습니다.
- `read`: `path` 원본 바이트
- `write`: `path`, `expected`(이전 SHA-256 또는 null), 원본 바이트
- `remove`: `path`, `expected` SHA-256
- `touch`: 임대 갱신
- `end`: 임대 해제와 동기화 완료

`.lof-sync-id`는 서재 UUID를 읽는 가상 항목이며 파일로 쓰지 않습니다. 임대는 180초이며 요청·작업 진행 시 갱신합니다. 쓰기와 삭제 직전 내용 해시를 확인하고 다르면 비교를 다시 시도합니다. 연결 해제 시 임대를 해제합니다.

Writer와 Library 사이에서는 Writer가 파일 비교를 담당합니다. Library끼리는 공개키 문자열 순서가 작은 피어가 비교합니다. 모든 구성원은 서로 직접 연결합니다. 처음 연결한 피어는 양쪽 파일을 합치며, 이후에는 피어별 로컬 체크포인트로 삭제를 판단합니다. 이전 1:1 연결(`realtime-v1`의 `peers`)과 `.lof/replicas` 체크포인트는 업데이트 시 한 번 지웁니다.

## 호스트 관리

네트워크 관리 서버는 없습니다. Windows는 원격 연결을 거부하는 명명 파이프, Unix는 권한 0600의 소켓을 사용합니다. 요청은 줄바꿈으로 끝나는 JSON `{action,args}`이며 응답은 `{ok,data}` 또는 `{ok:false,error}`입니다. 코드 발급·그룹 가입(`pair`)·그룹 나가기(`leave`)·서재·기기 관리에는 동일한 본체를 사용합니다.

`delete`는 `{id:"서재 UUID"}`를 받습니다. 진행 중인 파일 작업이 끝나면 그룹 탈퇴를 저장한 뒤 해당 Library의 서재 폴더와 등록 정보를 삭제합니다. 다른 기기로 파일 삭제 요청을 보내지 않습니다. 폴더 삭제에 실패하면 그룹에서 탈퇴한 서재 항목을 유지해 다시 시도할 수 있습니다.
