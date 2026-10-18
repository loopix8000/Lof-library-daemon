use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};

/// 실행 중인 서버의 DB를 SQLite 온라인 백업으로 복사한다. 기존 파일은 덮어쓰지 않는다.
pub fn backup(data_dir: &Path, destination: &Path) -> Result<(), String> {
    let source = data_dir.join("library.sqlite");
    if !source.is_file() {
        return Err("백업할 데이터베이스가 없습니다.".into());
    }
    OpenOptions::new().write(true).create_new(true).open(destination).map_err(|error| match error.kind() {
        ErrorKind::AlreadyExists => "같은 이름의 백업 파일이 이미 있습니다. 다른 이름을 지정해 주세요.".to_string(),
        _ => format!("백업 파일을 만들지 못했습니다: {error}"),
    })?;
    let failed = |error: rusqlite::Error| format!("백업을 완료하지 못했습니다: {error}");
    // 원본은 읽기 전용으로 열어 실행 중인 서버의 쓰기와 WAL 내용을 함께 복사한다.
    let origin = Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
        .map_err(failed)?;
    let mut target = Connection::open(destination).map_err(failed)?;
    Backup::new(&origin, &mut target)
        .and_then(|backup| backup.run_to_completion(256, Duration::from_millis(10), None))
        .map_err(failed)?;
    let check: String = target.query_row("PRAGMA integrity_check", [], |row| row.get(0)).map_err(failed)?;
    if check != "ok" {
        return Err("백업 파일의 무결성을 확인하지 못했습니다.".into());
    }
    Ok(())
}
