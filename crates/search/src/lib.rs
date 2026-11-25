//! Search over folders, written for Linework from the ground up: the walk, the
//! ignore rules and (next) the matching are this crate's own.
pub mod account;
pub mod ignore;
#[cfg(windows)]
pub mod walk;
#[cfg(windows)]
pub mod sweep;
