//! Zesty Backup: create `.tar.zst` backups of a server and ship them to cloud storage.
//!
//! The `zesty-backup` binary (src/main.rs) is a thin CLI over this library.

pub mod alerts;
pub mod archive;
pub mod backup;
pub mod config;
pub mod database;
pub mod doctor;
pub mod drill;
pub mod encryption;
pub mod providers;
pub mod report;
