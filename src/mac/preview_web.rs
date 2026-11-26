//! The document preview's web view on macOS. Not built yet: the pane says so.
use super::{Page, Signal};
use crate::{state::PreviewCues, win::*};
use std::path::PathBuf;

pub(super) struct View {
    pub h: H,
}
pub(super) fn cue_json(cues: &PreviewCues) -> serde_json::Value {
    let _ = cues;
    serde_json::Value::Null
}
impl View {
    pub fn create(_: H, _: bool) -> Option<Self> {
        let _ = std::marker::PhantomData::<Signal>;
        None
    }
    pub fn switch_document(&self, _: u64, _: &Option<PathBuf>) {}
    pub fn show(&self, _: Page, _: &PreviewCues, _: bool, _: serde_json::Value) {}
    pub fn run(&self, _: &str) -> bool {
        false
    }
    pub fn page_offset(&self, _: u64, _: u64, _: u64) -> Option<u64> {
        None
    }
    pub fn page_span(&self, _: u64, _: u64, _: u64, _: u64, _: u64) -> Option<(u64, u64, i64)> {
        None
    }
    pub fn locate(&self) -> bool {
        false
    }
}
