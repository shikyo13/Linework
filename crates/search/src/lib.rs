//! Search over folders, written for Linework from the ground up: the walk, the
//! ignore rules and (next) the matching are this crate's own.
pub mod account;
pub mod ignore;
pub mod input;
pub mod matcher;
pub mod pick;
pub mod searcher;
#[cfg(windows)]
pub mod walk;
#[cfg(windows)]
pub mod sweep;
