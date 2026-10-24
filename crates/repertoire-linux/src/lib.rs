//! The Linux-only parts of répertoire: fanotify, file handles, the mount
//! table and I/O priority. Everything else is platform-neutral and lives in
//! `repertoire-core`. Empty on other platforms.
#![cfg(target_os = "linux")]

pub mod access;
pub mod fanotify;
pub mod handle;
pub mod mounts;
pub mod prio;
