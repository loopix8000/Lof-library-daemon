use std::fmt;
#[derive(Debug)]
pub struct StartError(pub String);
impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for StartError {}
impl From<std::io::Error> for StartError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}
impl From<rusqlite::Error> for StartError {
    fn from(error: rusqlite::Error) -> Self {
        Self(format!("관리 DB 처리 실패: {error}"))
    }
}
