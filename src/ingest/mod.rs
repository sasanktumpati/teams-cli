pub mod extract;
pub mod indexeddb;
pub mod leveldb;
pub mod render;
pub mod sync;
pub mod v8;

use std::fmt;

#[derive(Debug, Clone)]
pub struct SourceError(pub String);

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SourceError {}

#[derive(Debug, Clone)]
pub struct SyncBusy(pub String);

impl fmt::Display for SyncBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SyncBusy {}
