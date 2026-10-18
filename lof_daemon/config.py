from dataclasses import dataclass
import os
from pathlib import Path
import socket


def valid_token(value):
    return len(value) >= 32 and value.isascii() and not any(char.isspace() for char in value)


@dataclass(frozen=True)
class Config:
    data_dir: Path
    # 이전 버전의 공통 토큰. 설정하면 일반 기기 권한으로만 계속 허용한다.
    api_token: str = ""
    # 이 컴퓨터에서 데몬을 실행한 관리 앱만 아는 실행별 관리자 키.
    local_admin_token: str = ""
    server_name: str = "Lof Library"
    history_tick_seconds: float = 10
    maintenance_seconds: float = 3600
    heartbeat_seconds: float = 15
    max_request_bytes: int = 8 * 1024 * 1024

    def __post_init__(self):
        if self.api_token and not valid_token(self.api_token):
            raise ValueError("LOF_API_TOKEN을 사용하려면 공백 없는 32자 이상의 값을 설정해 주세요.")
        if self.local_admin_token and not valid_token(self.local_admin_token):
            raise ValueError("LOF_LOCAL_ADMIN_TOKEN은 공백 없는 32자 이상이어야 합니다.")
        if not self.server_name.strip() or len(self.server_name) > 100:
            raise ValueError("LOF_SERVER_NAME은 1~100자로 설정해 주세요.")
        if min(self.history_tick_seconds, self.maintenance_seconds, self.heartbeat_seconds) <= 0:
            raise ValueError("작업 주기는 0보다 커야 합니다.")

    @classmethod
    def from_env(cls, data_dir=None):
        return cls(
            data_dir=Path(data_dir or os.environ.get("LOF_DATA_DIR", "data")).resolve(),
            api_token=os.environ.get("LOF_API_TOKEN", "").strip(),
            local_admin_token=os.environ.pop("LOF_LOCAL_ADMIN_TOKEN", "").strip(),
            server_name=(os.environ.get("LOF_SERVER_NAME", "").strip() or socket.gethostname() or "Lof Library")[:100],
        )


class InstanceLock:
    """같은 데이터 폴더에서 작업자와 알림 서버가 중복 실행되는 것을 막는다."""

    def __init__(self, directory: Path):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.file = (directory / "daemon.lock").open("a+b")
        try:
            if os.name == "nt":
                import msvcrt

                self.file.seek(0, 2)
                if not self.file.tell():
                    self.file.write(b"0")
                    self.file.flush()
                self.file.seek(0)
                msvcrt.locking(self.file.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl

                fcntl.flock(self.file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            self.file.close()
            raise RuntimeError("이 데이터 폴더를 사용하는 데몬이 이미 실행 중입니다.") from error

    def close(self):
        self.file.close()
