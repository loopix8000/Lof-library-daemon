import argparse
import os
from pathlib import Path
import sqlite3


def backup(data_dir: Path, destination: Path):
    source = (data_dir / "library.sqlite").resolve()
    destination = destination.resolve()
    if not source.is_file():
        raise ValueError("백업할 데이터베이스가 없습니다.")
    # 기존 파일을 덮어쓰지 않으며 원본은 읽기 전용으로 연다.
    handle = os.open(destination, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    os.close(handle)
    origin = sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)
    target = sqlite3.connect(destination)
    try:
        origin.backup(target)
        if target.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            raise RuntimeError("백업 파일의 무결성을 확인하지 못했습니다.")
    finally:
        target.close()
        origin.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="실행 중인 서재 데몬의 SQLite 온라인 백업")
    parser.add_argument("destination", type=Path)
    parser.add_argument("--data-dir", type=Path, default=Path(os.environ.get("LOF_DATA_DIR", "data")))
    arguments = parser.parse_args()
    backup(arguments.data_dir, arguments.destination)
    print("서재 백업을 완료했습니다.")
