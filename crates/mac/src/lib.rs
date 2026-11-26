//! Linework for macOS: an AppKit front end on the same editor, storage and
//! command table as the Windows program.
#![cfg(target_os = "macos")]
mod app;
#[path = "../../../src/commands.rs"]
#[allow(dead_code)]
mod commands;
mod menu;
#[path = "../../../src/selection.rs"]
#[allow(dead_code)]
mod selection;

pub use app::{App, run};
