//! Search over folders, written for Linework from the ground up: the walk, the
//! ignore rules and (next) the matching are this crate's own.
pub mod account;
pub mod ignore;
pub mod input;
#[cfg(target_os = "macos")]
pub mod kept;
pub mod matcher;
pub mod options;
pub mod pick;
pub mod plan;
pub mod searcher;
pub mod settings;
#[cfg(windows)]
pub mod walk;
#[cfg(target_os = "macos")]
#[path = "walk_mac.rs"]
pub mod walk;
#[cfg(any(windows, target_os = "macos"))]
pub mod sweep;
