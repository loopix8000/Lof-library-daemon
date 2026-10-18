//! 장치 키·서재·허가된 피어를 보관하는 로컬 관리 DB.
use crate::error::StartError;
use rusqlite::{Connection, TransactionBehavior};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};
pub struct Store {
    connection: Mutex<Connection>,
    pub path: PathBuf,
}
impl Store {
    pub fn open(directory: &Path) -> Result<Self, StartError> {
        std::fs::create_dir_all(directory)?;
        let path = directory.join("library.sqlite");
        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS server (key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        Ok(Self {
            connection: Mutex::new(connection),
            path,
        })
    }
    fn transaction<T>(
        &self,
        behavior: TransactionBehavior,
        action: impl FnOnce(&Connection) -> Result<T, StartError>,
    ) -> Result<T, StartError> {
        let mut connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let transaction = connection.transaction_with_behavior(behavior)?;
        let result = action(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }
    pub fn read<T>(
        &self,
        action: impl FnOnce(&Connection) -> Result<T, StartError>,
    ) -> Result<T, StartError> {
        self.transaction(TransactionBehavior::Deferred, action)
    }
    pub fn write<T>(
        &self,
        action: impl FnOnce(&Connection) -> Result<T, StartError>,
    ) -> Result<T, StartError> {
        self.transaction(TransactionBehavior::Immediate, action)
    }
}
