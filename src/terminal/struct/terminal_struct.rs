use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::ui::TermSpan;

/// Per-terminal state used by normal and alternate-screen rendering.
pub(crate) struct TermBuffer {
    pub(crate) parser: vt100::Parser,
    pub(crate) find_query: String,
    pub(crate) is_dark: bool,
    pub(crate) output_highlight: OutputHighlightPreset,
    pub(crate) custom_highlight_rules: Vec<CompiledOutputRule>,
    pub(crate) sel_anchor: Option<(usize, u16)>,
    pub(crate) sel_focus: Option<(usize, u16)>,
    pub(crate) sel_ranges: Vec<((usize, u16), (usize, u16))>,
    pub(crate) history: VecDeque<Line>,
    pub(crate) prev: Vec<Line>,
    pub(crate) view_offset: usize,
    pub(crate) displayed_text: Vec<String>,
    pub(crate) csi_state: CsiState,
    pub(crate) raw: VecDeque<u8>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CsiState {
    Normal,
    Esc,
    Csi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputHighlightPreset {
    Off,
    Log,
    DevOps,
}

#[derive(Clone)]
pub(crate) struct CompiledOutputRule {
    pub(crate) matcher: regex::Regex,
    pub(crate) whole_line: bool,
    pub(crate) ansi_index: u8,
}

pub(crate) type TermBufferHandle = Arc<Mutex<TermBuffer>>;
pub(crate) type TermBuffers = Arc<Mutex<HashMap<String, TermBufferHandle>>>;

/// Coalesces render requests for one terminal tab.
pub(crate) struct TabRenderGate {
    pub(crate) scheduled: AtomicBool,
    pub(crate) pending: AtomicBool,
    pub(crate) last_render: Mutex<std::time::Instant>,
}

pub(crate) type RenderGates = Arc<Mutex<HashMap<String, Arc<TabRenderGate>>>>;

/// A coloured, cursor-annotated snapshot ready for the Slint terminal grid.
pub(crate) struct BuiltScreen {
    pub(crate) spans: Vec<TermSpan>,
    pub(crate) cursor_row: i32,
    pub(crate) cursor_col: i32,
    pub(crate) rows_used: i32,
    pub(crate) is_alt: bool,
    pub(crate) scroll_max: i32,
    pub(crate) scroll_offset: i32,
}

/// One coloured run within a terminal line.
#[derive(Clone)]
pub(crate) struct HistSpan {
    pub(crate) text: String,
    pub(crate) fg: vt100::Color,
    pub(crate) bg: vt100::Color,
    pub(crate) bold: bool,
    pub(crate) inverse: bool,
    pub(crate) col: i32,
    pub(crate) cells: i32,
}

pub(crate) type Line = (String, Vec<HistSpan>, bool);
