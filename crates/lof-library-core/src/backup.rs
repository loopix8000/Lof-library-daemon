//! 중지된 피어의 전체 데이터를 새 폴더에 보관한다.
use crate::InstanceLock;
use std::{fs, path::Path, time::Duration};
pub fn backup(source: &Path, destination: &Path) -> Result<(), String> {
    let source = source.canonicalize().map_err(|e| e.to_string())?;
    let _lock = InstanceLock::acquire(&source)
        .map_err(|_| "동기화를 중지한 뒤 전체 데이터 폴더를 백업해 주세요.".to_string())?;
    let absolute = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(destination)
    };
    let parent = absolute
        .parent()
        .ok_or("백업 경로 오류")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let target = parent.join(absolute.file_name().ok_or("백업 폴더 이름 오류")?);
    if target.starts_with(&source) || source.starts_with(&target) {
        return Err("원본 데이터 폴더 밖의 새 경로를 지정해 주세요.".into());
    }
    fs::create_dir(&target).map_err(|e| format!("새 백업 폴더를 만들지 못했습니다: {e}"))?;
    fn copy(source: &Path, target: &Path) -> Result<(), String> {
        for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some(
                    "daemon.lock"
                        | "control.sock"
                        | "library.sqlite"
                        | "library.sqlite-wal"
                        | "library.sqlite-shm"
                )
            ) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("연결 파일은 백업에 포함할 수 없습니다.".into());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err("연결 폴더는 백업에 포함할 수 없습니다.".into());
                }
            }
            let to = target.join(name);
            if metadata.is_dir() {
                fs::create_dir(&to).map_err(|e| e.to_string())?;
                copy(&entry.path(), &to)?;
            } else if metadata.is_file() {
                fs::copy(entry.path(), to).map_err(|e| e.to_string())?;
            } else {
                return Err("일반 파일과 폴더만 백업할 수 있습니다.".into());
            }
        }
        Ok(())
    }
    copy(&source, &target)?;
    if source.join("library.sqlite").is_file() {
        let origin = rusqlite::Connection::open_with_flags(
            source.join("library.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| e.to_string())?;
        let mut out =
            rusqlite::Connection::open(target.join("library.sqlite")).map_err(|e| e.to_string())?;
        rusqlite::backup::Backup::new(&origin, &mut out)
            .and_then(|b| b.run_to_completion(256, Duration::from_millis(10), None))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
