#![forbid(unsafe_code)]

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod cli;
pub mod daemon;
pub mod doctor;
pub mod formatters;
pub mod identity;
pub mod ingest;
pub mod json_codec;
pub mod launchd;
pub mod live;
pub mod paths;
pub mod query;
pub mod storage;
pub mod timeutil;
pub mod update;

#[cfg(test)]
mod tests;
