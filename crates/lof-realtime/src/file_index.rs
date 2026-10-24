//! 파일 상태와 문서 헤더를 SQLite에 보관해 바뀐 파일만 다시 읽는다.
use crate::{storage, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

pub const FORMAT: &str = "inventory-index-v1";

fn changes() -> &'static Mutex<HashMap<PathBuf, HashSet<String>>> {
    static CHANGES: OnceLock<Mutex<HashMap<PathBuf, HashSet<String>>>> = OnceLock::new();
    CHANGES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn changed(root: &Path, relative: &str) {
    let mut changes = changes().lock().unwrap();
    let paths = changes.entry(root.to_path_buf()).or_default();
    if !paths.contains("") {
        paths.insert(relative.to_string());
    }
    if paths.len() > 40_000 {
        paths.clear();
        paths.insert(String::new());
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Version {
    pub version: String,
    pub identity: Option<String>,
}
impl Version {
    pub fn matches(&self, hash: Option<&str>) -> bool {
        hash == Some(self.version.as_str())
            && self.version.len() == 64
            && self.version.bytes().all(|b| b.is_ascii_hexdigit())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub index: Version,
    pub header: Value,
    pub size: u64,
    pub modified: Option<i64>,
}

pub struct Index {
    db: Connection,
}

fn stamp(meta: &fs::Metadata) -> Option<String> {
    let modified = meta.modified().ok()?;
    let created = meta.created().ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!(
            "{}:{modified:?}:{created:?}:{}:{}:{}:{}",
            meta.len(),
            meta.dev(),
            meta.ino(),
            meta.ctime(),
            meta.ctime_nsec()
        ))
    }
    #[cfg(not(unix))]
    {
        Some(format!("{}:{modified:?}:{created:?}", meta.len()))
    }
}

impl Index {
    pub fn open(root: &Path) -> Result<Self> {
        let directory = root.join(".lof");
        for path in [
            root.to_path_buf(),
            directory.clone(),
            directory.join("sync-index.sqlite"),
            directory.join("sync-index.sqlite-wal"),
            directory.join("sync-index.sqlite-shm"),
        ] {
            match fs::symlink_metadata(path) {
                Ok(meta) if storage::linked(&meta) => {
                    return Err("색인 경로에 연결 폴더가 있습니다.".into())
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let db =
            Connection::open(directory.join("sync-index.sqlite")).map_err(|e| e.to_string())?;
        db.busy_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            CREATE TABLE IF NOT EXISTS files (path TEXT PRIMARY KEY, stamp TEXT NOT NULL, entry TEXT NOT NULL);")
            .map_err(|e| e.to_string())?;
        let index = Self { db };
        let paths = changes().lock().unwrap().remove(root).unwrap_or_default();
        for path in &paths {
            if let Err(error) = index.invalidate(path) {
                for path in &paths {
                    changed(root, path);
                }
                return Err(error);
            }
        }
        Ok(index)
    }

    pub fn inspect(
        &self,
        root: &Path,
        relative: &str,
        header: impl FnOnce(&[u8]) -> Result<(Option<String>, Value)>,
    ) -> Result<Entry> {
        let path = storage::path(root, relative)?;
        let before = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !before.is_file() || storage::linked(&before) {
            return Err("일반 파일만 색인할 수 있습니다.".into());
        }
        let before_stamp = stamp(&before);
        let cached: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT stamp,entry FROM files WHERE path=?1",
                [relative],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((cached_stamp, entry)) = cached {
            if before_stamp.as_ref() == Some(&cached_stamp) {
                if let Ok(entry) = serde_json::from_str::<Entry>(&entry) {
                    if entry.index.matches(Some(&entry.index.version)) {
                        return Ok(entry);
                    }
                }
            }
        }
        let bytes = storage::read(root, relative)?.ok_or("색인 중 파일이 변경되었습니다.")?;
        let (identity, header) = header(&bytes)?;
        let after = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !after.is_file()
            || storage::linked(&after)
            || before_stamp != stamp(&after)
            || before.len() != bytes.len() as u64
        {
            return Err("색인 중 파일이 변경되었습니다.".into());
        }
        let modified = before
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|time| i64::try_from(time.as_secs()).ok());
        let entry = Entry {
            index: Version {
                version: storage::hash(&bytes),
                identity,
            },
            header,
            size: before.len(),
            modified,
        };
        if let Some(stamp) = before_stamp {
            self.db
                .execute(
                    "INSERT INTO files VALUES(?1,?2,?3) ON CONFLICT(path)
                DO UPDATE SET stamp=excluded.stamp,entry=excluded.entry",
                    params![
                        relative,
                        stamp,
                        serde_json::to_string(&entry).map_err(|e| e.to_string())?
                    ],
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(entry)
    }

    pub fn invalidate(&self, relative: &str) -> Result<()> {
        self.db
            .execute(
                "DELETE FROM files WHERE ?1='' OR path=?1 OR substr(path,1,length(?1)+1)=?1||'/'",
                [relative],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn retain(&self, paths: impl Iterator<Item = impl AsRef<str>>) -> Result<()> {
        let paths: std::collections::HashSet<String> =
            paths.map(|path| path.as_ref().to_string()).collect();
        let mut query = self
            .db
            .prepare("SELECT path FROM files")
            .map_err(|e| e.to_string())?;
        let old = query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let removed: Vec<_> = old
            .into_iter()
            .filter(|path| !paths.contains(path))
            .collect();
        if removed.is_empty() {
            return Ok(());
        }
        let transaction = self.db.unchecked_transaction().map_err(|e| e.to_string())?;
        for path in removed {
            transaction
                .execute("DELETE FROM files WHERE path=?1", [path])
                .map_err(|e| e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())
    }
}

pub fn header(relative: &str, bytes: &[u8]) -> Result<(Option<String>, Value)> {
    let (prefix, value) = if relative.ends_with("/project.json") {
        (
            "project",
            serde_json::from_slice::<Value>(bytes).map_err(|e| e.to_string())?,
        )
    } else if relative.to_ascii_lowercase().ends_with(".md") {
        let raw = std::str::from_utf8(bytes)
            .map_err(|e| e.to_string())?
            .trim_start_matches('\u{feff}');
        let mut lines = raw.lines();
        let value = if lines.next().is_some_and(|line| line.trim_end() == "---") {
            let yaml = lines
                .take_while(|line| line.trim_end() != "---")
                .collect::<Vec<_>>()
                .join("\n");
            serde_yaml::from_str(&yaml).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        ("document", value)
    } else {
        return Ok((None, Value::Null));
    };
    let identity = value["id"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 80
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .map(|id| format!("{prefix}:{id}"));
    Ok((identity, value))
}
