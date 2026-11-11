//! The Linux-only parts of répertoire: fanotify, file handles, the mount
//! table, I/O priority and disk read counters. Everything else is
//! platform-neutral and lives in `repertoire-core`. Empty on other platforms.
#![cfg(target_os = "linux")]

pub mod access;
pub mod diskio;
pub mod fanotify;
pub mod handle;
pub mod mounts;
pub mod prio;
