//! 상시 보관 피어의 경로 검증과 원본 파일 보관. 관리 파일은 공유하지 않는다.
use crate::{Result, MAX_FILE};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

pub fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn excluded(part: &str) -> bool {
    let lower = part.to_ascii_lowercase();
    part.starts_with('.')
        || lower.ends_with(".tmp")
        || lower.ends_with(".lofmv")
        || lower.ends_with(".lofnew")
        || lower == "project.backup.json"
}
pub fn allowed(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').count() <= 64
        && path.split('/').all(|part| {
            let upper = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            !part.is_empty()
                && !excluded(part)
                && !part.ends_with([' ', '.'])
                && part.encode_utf16().count() <= 240
                && !part
                    .chars()
                    .any(|c| c.is_control() || "\\:<>\"|?*".contains(c))
                && !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(upper.len() == 4
                    && (upper.starts_with("COM") || upper.starts_with("LPT"))
                    && matches!(upper.as_bytes()[3], b'1'..=b'9'))
        })
}
pub(crate) fn linked(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}
pub fn path(root: &Path, relative: &str) -> Result<PathBuf> {
    if !allowed(relative) {
        return Err("공유할 수 없는 파일 경로입니다.".into());
    }
    let mut target = root.to_path_buf();
    for part in std::iter::once("").chain(relative.split('/')) {
        if !part.is_empty() {
            target.push(part);
        }
        match fs::symlink_metadata(&target) {
            Ok(meta) if linked(&meta) => {
                return Err("연결 폴더 또는 심볼릭 링크는 공유할 수 없습니다.".into())
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(target)
}
pub fn read(root: &Path, relative: &str) -> Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path(root, relative)?) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FILE {
        return Err("파일은 50MB 이하여야 합니다.".into());
    }
    Ok(Some(bytes))
}
pub fn inventory(root: &Path) -> Result<Value> {
    fn walk(
        root: &Path,
        dir: &str,
        out: &mut BTreeMap<String, Value>,
        folded: &mut HashSet<String>,
    ) -> Result<()> {
        let target = if dir.is_empty() {
            root.to_path_buf()
        } else {
            path(root, dir)?
        };
        for item in fs::read_dir(target).map_err(|e| e.to_string())? {
            let item = item.map_err(|e| e.to_string())?;
            let name = item
                .file_name()
                .into_string()
                .map_err(|_| "파일 이름 인코딩 오류")?;
            let rel = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            if rel.split('/').any(excluded) {
                continue;
            }
            if !allowed(&rel) {
                return Err("공유할 수 없는 파일 이름이 있습니다.".into());
            }
            let full = path(root, &rel)?;
            let meta = fs::symlink_metadata(full).map_err(|e| e.to_string())?;
            if meta.is_dir() {
                walk(root, &rel, out, folded)?;
            } else if meta.is_file() {
                if out.len() >= 40_000 || !folded.insert(rel.to_lowercase()) {
                    return Err("파일 개수 또는 대소문자 경로 충돌을 확인해 주세요.".into());
                }
                let bytes = read(root, &rel)?.ok_or("목록을 읽는 중 파일이 변경되었습니다.")?;
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|t| t.as_secs());
                out.insert(
                    rel,
                    json!({"size":bytes.len(),"modified":modified,"etag":hash(&bytes)}),
                );
            } else {
                return Err("일반 파일만 공유할 수 있습니다.".into());
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, "", &mut out, &mut HashSet::new())?;
    Ok(json!(out))
}
pub fn atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(target.parent().ok_or("파일 경로 오류")?).map_err(|e| e.to_string())?;
    let temp = target.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    fs::rename(temp, target).map_err(|e| e.to_string())
}
pub fn apply(
    root: &Path,
    relative: &str,
    data: Option<&[u8]>,
    expected: Option<&str>,
) -> Result<()> {
    let target = path(root, relative)?;
    let old = read(root, relative)?;
    if old.as_deref().map(hash).as_deref() != expected {
        return Err("파일이 변경되어 다시 비교합니다.".into());
    }
    if let Some(bytes) = old.as_deref() {
        let history = root
            .join(".lof")
            .join("history")
            .join(hash(bytes))
            .join(relative);
        let mut ancestor = Some(history.as_path());
        while let Some(p) = ancestor {
            if p == root {
                break;
            }
            if let Ok(meta) = fs::symlink_metadata(p) {
                if linked(&meta) {
                    return Err("이력 경로에 연결 폴더가 있습니다.".into());
                }
            }
            ancestor = p.parent();
        }
        if !history.exists() {
            atomic(&history, bytes)?;
        }
    }
    match data {
        Some(bytes) => atomic(&target, bytes),
        None => {
            if old.is_some() {
                fs::remove_file(target).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}
