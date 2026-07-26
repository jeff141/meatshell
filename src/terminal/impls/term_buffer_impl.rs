use crate::app::{
    build_row, cell_prefix, char_after_cell_end, char_at_cell_start, detect_scroll,
    highlight_plain_output, render_term_span, MAX_HISTORY, RAW_CAP,
};
use crate::terminal::{BuiltScreen, CsiState, Line, TermBuffer};
use crate::ui::TermMatch;
impl TermBuffer {
    // ---- Absolute-coordinate selection helpers (#18 follow-up) -------------
    //
    // The "combined" buffer is `history` (oldest first) followed by the live
    // screen rows.  A visible window of `rows` rows looks at a slice of it whose
    // top index depends on whether we're at the live bottom or scrolled up.

    /// Live screen rows plus the count of non-blank ones at the top.
    fn live_rows(&self) -> (Vec<Line>, usize) {
        let s = self.parser.screen();
        let (rows, cols) = s.size();
        let live: Vec<Line> = (0..rows).map(|r| build_row(s, r, cols)).collect();
        let used = live
            .iter()
            .rposition(|(_, runs, _)| !runs.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        (live, used)
    }

    /// Absolute combined-row index of the top visible row for the current view.
    fn view_top_abs(&self, _live_used: usize) -> usize {
        let rows = self.parser.screen().size().0 as usize;
        let hist_len = self.history.len();
        if self.view_offset == 0 {
            // Live view: visible row 0 is live screen row 0 = combined[hist_len].
            hist_len
        } else {
            // Include the screen's full row count (trailing blanks too) so this
            // mapping matches render()'s scroll window — keeping the live and
            // scrolled views continuous after a shrink/grow (#119-followup).
            let combined_len = hist_len + rows;
            combined_len.saturating_sub(rows + self.view_offset)
        }
    }

    /// Map a visible row (0..rows) to its absolute combined-row index.
    pub(crate) fn vis_to_abs(&self, vis_row: u16) -> usize {
        let (_, live_used) = self.live_rows();
        self.view_top_abs(live_used) + vis_row as usize
    }

    /// Highlight rectangles for the current selection, clipped to the visible
    /// window of the current view.
    pub(crate) fn selection_rects_visible(&self, cols: u16) -> Vec<TermMatch> {
        let ranges = if self.sel_ranges.is_empty() {
            match (self.sel_anchor, self.sel_focus) {
                (Some(anchor), Some(focus)) => vec![(anchor, focus)],
                _ => Vec::new(),
            }
        } else {
            self.sel_ranges.clone()
        };
        if ranges.is_empty() {
            return Vec::new();
        }
        let (_, live_used) = self.live_rows();
        let top = self.view_top_abs(live_used);
        let rows = self.parser.screen().size().0;
        let mut out = Vec::new();
        for ((ar, ac), (fr, fc)) in ranges {
            let (lo_r, lo_c, hi_r, hi_c) = if (ar, ac) <= (fr, fc) {
                (ar, ac, fr, fc)
            } else {
                (fr, fc, ar, ac)
            };
            if (lo_r, lo_c) == (hi_r, hi_c) {
                continue;
            }
            for vis in 0..rows {
                let abs = top + vis as usize;
                if abs < lo_r || abs > hi_r {
                    continue;
                }
                let (c0, c1) = if abs == lo_r && abs == hi_r {
                    (lo_c.min(hi_c), lo_c.max(hi_c))
                } else if abs == lo_r {
                    (lo_c, cols.saturating_sub(1))
                } else if abs == hi_r {
                    (0, hi_c)
                } else {
                    (0, cols.saturating_sub(1))
                };
                out.push(TermMatch {
                    row: vis as i32,
                    col: c0 as i32,
                    len: (c1.saturating_sub(c0) + 1) as i32,
                });
            }
        }
        out
    }

    /// If the current find query is outside the visible window, jump to the
    /// first matching row in scrollback/live content so old serial output can be
    /// found without manually scrolling back first (#233).
    pub(crate) fn scroll_to_first_find_match(&mut self, query: &str) -> bool {
        if query.is_empty() || self.parser.screen().alternate_screen() {
            return false;
        }
        let q = query.to_lowercase();
        let (live, _) = self.live_rows();
        let rows = self.parser.screen().size().0 as usize;
        let hist_len = self.history.len();
        let combined_len = hist_len + live.len();
        let Some(match_idx) = self
            .history
            .iter()
            .map(|line| &line.0)
            .chain(live.iter().map(|line| &line.0))
            .position(|line| line.to_lowercase().contains(&q))
        else {
            return false;
        };
        let top = match_idx.min(combined_len.saturating_sub(rows));
        let new_offset = combined_len.saturating_sub(rows + top);
        if self.view_offset == new_offset {
            return false;
        }
        self.view_offset = new_offset;
        true
    }

    /// Extract the selected text from the combined buffer (whole selection,
    /// even the parts currently scrolled out of view).
    pub(crate) fn extract_selection_text(&self) -> String {
        let ranges = if self.sel_ranges.is_empty() {
            match (self.sel_anchor, self.sel_focus) {
                (Some(anchor), Some(focus)) => vec![(anchor, focus)],
                _ => Vec::new(),
            }
        } else {
            self.sel_ranges.clone()
        };
        if ranges.is_empty() {
            return String::new();
        }
        ranges
            .iter()
            .map(|&(anchor, focus)| self.extract_range_text(anchor, focus))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn extract_range_text(&self, (ar, ac): (usize, u16), (fr, fc): (usize, u16)) -> String {
        let (lo_r, lo_c, hi_r, hi_c) = if (ar, ac) <= (fr, fc) {
            (ar, ac, fr, fc)
        } else {
            (fr, fc, ar, ac)
        };
        let (live, live_used) = self.live_rows();
        let hist_len = self.history.len();
        let combined_len = hist_len + live_used;
        // Clamp into real content so a focus parked on a blank row below the
        // prompt doesn't emit trailing empty lines.
        let hi_r = hi_r.min(combined_len.saturating_sub(1));
        let mut out = String::new();
        for r in lo_r..=hi_r {
            let line: &str = if r < hist_len {
                &self.history[r].0
            } else if r - hist_len < live.len() {
                &live[r - hist_len].0
            } else {
                ""
            };
            let chars: Vec<char> = line.chars().collect();
            // `c0`/`c1` are GRID COLUMNS (inclusive). The plain text keeps one
            // char per glyph, so wide (CJK) glyphs make char index != column;
            // map columns → char indices via the cell prefix so the copied text
            // doesn't drift by the number of wide glyphs before it (#132).
            let (c0, c1) = if r == lo_r && r == hi_r {
                (lo_c.min(hi_c), lo_c.max(hi_c))
            } else if r == lo_r {
                (lo_c, u16::MAX)
            } else if r == hi_r {
                (0, hi_c)
            } else {
                (0, u16::MAX)
            };
            let prefix = cell_prefix(&chars);
            let start = char_at_cell_start(&prefix, c0 as usize);
            let end = char_after_cell_end(&prefix, c1 as usize);
            let seg: String = if start < end {
                chars[start..end].iter().collect()
            } else {
                String::new()
            };
            out.push_str(seg.trim_end());
            let wrapped = if r < hist_len {
                self.history[r].2
            } else if r - hist_len < live.len() {
                live[r - hist_len].2
            } else {
                false
            };
            if r != hi_r && !wrapped {
                out.push('\n');
            }
        }
        out
    }

    /// Feed bytes to vt100 and capture scrolled-off lines into history.
    ///
    /// We detect scroll by diffing the screen before/after a `process`, which
    /// can only recover up to one screen of shift per call.  A single large
    /// burst can scroll many screens at once, so we split the input at newline
    /// boundaries into batches of at most ~half a screen of lines and capture
    /// after each — that way no batch ever scrolls more than the diff can see,
    /// and nothing is lost.  (Splitting only on `\n` is safe: VT escape
    /// sequences never contain a newline.)
    pub(crate) fn ingest(&mut self, input: &[u8]) {
        // Rewrite HVP (`ESC [ … f`) → CUP (`ESC [ … H`) so vt100 (which only
        // implements `H`) honours btop/htop's absolute cursor positioning.
        let bytes = self.rewrite_hvp(input);
        // Retain the (post-rewrite) stream, capped, so a resize can replay it at
        // the new width and reflow already-printed output (#169).
        self.raw.extend(bytes.iter().copied());
        self.cap_raw();
        self.feed_batched(&bytes);
    }

    /// Feed a (already HVP-rewritten) byte slice to vt100 in newline-bounded
    /// batches, capturing scrolled-off lines into history after each (see the
    /// `ingest` doc comment). Does NOT touch `self.raw`, so it is reused by both
    /// live ingest and resize-reflow replay.
    fn feed_batched(&mut self, bytes: &[u8]) {
        let rows = self.parser.screen().size().0 as usize;
        let batch_lines = (rows / 2).max(1);
        let mut start = 0usize;
        let mut nl = 0usize;
        for i in 0..bytes.len() {
            if bytes[i] == b'\n' {
                nl += 1;
                if nl >= batch_lines {
                    self.ingest_chunk(&bytes[start..=i]);
                    start = i + 1;
                    nl = 0;
                }
            }
        }
        if start < bytes.len() {
            self.ingest_chunk(&bytes[start..]);
        }
    }

    /// Trim the retained stream to `RAW_CAP`, dropping from the front up to the
    /// next line boundary so a replay never starts mid-escape / mid-wrapped-line.
    fn cap_raw(&mut self) {
        if self.raw.len() <= RAW_CAP {
            return;
        }
        let overflow = self.raw.len() - RAW_CAP;
        self.raw.drain(0..overflow);
        while let Some(&b) = self.raw.front() {
            self.raw.pop_front();
            if b == b'\n' {
                break;
            }
        }
    }

    /// Resize-reflow (#169): rebuild the screen + scrollback at a new width by
    /// replaying the retained byte stream through a fresh parser. vt100 itself
    /// can't reflow (`set_size` just truncates/pads each row), and we only keep
    /// rendered grid rows in `history`, so replaying the raw stream is what lets
    /// long lines rewrap to the new width like FinalShell. Used only on the normal
    /// screen — alt-screen programs (tmux/vim) get a SIGWINCH redraw from the
    /// remote instead.
    pub(crate) fn reflow(&mut self, new_rows: u16, new_cols: u16) {
        let stream: Vec<u8> = self.raw.iter().copied().collect();
        self.parser = vt100::Parser::new(new_rows, new_cols, 5000);
        self.history.clear();
        self.prev.clear();
        self.view_offset = 0;
        // Scrollback line count changes, so absolute selection coords no longer map.
        self.sel_anchor = None;
        self.sel_focus = None;
        self.sel_ranges.clear();
        self.feed_batched(&stream);
    }

    /// Translate every CSI sequence terminated by `f` (HVP) into the identical
    /// sequence terminated by `H` (CUP).  The scanner state persists across
    /// calls, so a sequence split across read chunks is still handled.  Only the
    /// final byte of a CSI sequence is ever touched; text bytes pass through.
    fn rewrite_hvp(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            match self.csi_state {
                CsiState::Normal => {
                    if b == 0x1b {
                        self.csi_state = CsiState::Esc;
                    }
                    out.push(b);
                }
                CsiState::Esc => {
                    if b == b'[' {
                        self.csi_state = CsiState::Csi;
                    } else {
                        // Not a CSI (could be another ESC, OSC, etc.).  Re-arm on
                        // a fresh ESC, otherwise fall back to normal text.
                        self.csi_state = if b == 0x1b {
                            CsiState::Esc
                        } else {
                            CsiState::Normal
                        };
                    }
                    out.push(b);
                }
                CsiState::Csi => {
                    // Final bytes are 0x40..=0x7e; params/intermediates are
                    // 0x20..=0x3f.  Rewrite an `f` final into `H`.
                    if (0x40..=0x7e).contains(&b) {
                        out.push(if b == b'f' { b'H' } else { b });
                        self.csi_state = CsiState::Normal;
                    } else {
                        out.push(b);
                    }
                }
            }
        }
        out
    }

    /// Process one bounded batch and capture any lines that scrolled off the top
    /// (skipped for alt-screen programs like vim/nano).
    fn ingest_chunk(&mut self, bytes: &[u8]) {
        // Detect full-screen-clear sequences *before* processing so we can
        // suppress history for programs that redraw without alt-screen (e.g.
        // btop configured with `alt-screen = false`).
        // We look for \033[H (cursor-home) and \033[2J / \033[J (erase display)
        // as indicators that the program is doing a full-screen refresh.
        let has_cursor_home = bytes.windows(3).any(|w| w == b"\x1b[H");
        let has_erase_display =
            bytes.windows(4).any(|w| w == b"\x1b[2J") || bytes.windows(3).any(|w| w == b"\x1b[J");
        let is_fullscreen_refresh = has_cursor_home && has_erase_display;

        self.parser.process(bytes);
        let (is_alt, rows, cols) = {
            let s = self.parser.screen();
            let (r, c) = s.size();
            (s.alternate_screen(), r, c)
        };
        if is_alt {
            // Snap to live view whenever we're on the alt screen — this
            // prevents old history (accumulated before alt-screen was entered)
            // from mixing with the full-screen program's output after a scroll.
            self.view_offset = 0;
            self.prev.clear();
            return;
        }
        if is_fullscreen_refresh {
            // Non-alt-screen full-screen refresh (btop, htop with alt disabled…).
            // Don't capture lines into history; they'd mix with the next frame.
            self.view_offset = 0;
            self.prev.clear();
            return;
        }
        let curr: Vec<Line> = {
            let s = self.parser.screen();
            (0..rows).map(|r| build_row(s, r, cols)).collect()
        };
        if !self.prev.is_empty() {
            let k = detect_scroll(&self.prev, &curr);
            for line in self.prev.iter().take(k) {
                self.history.push_back(line.clone());
            }
            while self.history.len() > MAX_HISTORY {
                self.history.pop_front();
            }
        }
        self.prev = curr;
    }

    /// Render the terminal grid for the current scrollback `view_offset`
    /// (0 = live).  Caches the displayed plain text for find/selection.
    pub(crate) fn render(&mut self) -> BuiltScreen {
        let (is_alt, rows, cols, cur_row, cur_col) = {
            let s = self.parser.screen();
            let (r, c) = s.size();
            let (cr, cc) = s.cursor_position();
            (s.alternate_screen(), r, c, cr, cc)
        };

        // --- Live view (also alt-screen): render the current grid -----------
        if is_alt || self.view_offset == 0 {
            let mut spans = Vec::new();
            let mut displayed = Vec::with_capacity(rows as usize);
            let mut last_content = 0i32;
            let s = self.parser.screen();
            for r in 0..rows {
                let (plain, runs, _wrapped) = build_row(s, r, cols);
                let runs = if is_alt {
                    runs
                } else {
                    highlight_plain_output(
                        runs,
                        self.output_highlight,
                        &self.custom_highlight_rules,
                    )
                };
                if !runs.is_empty() {
                    last_content = r as i32;
                }
                for hs in runs {
                    spans.extend(render_term_span(&hs, r as i32, self.is_dark));
                }
                displayed.push(plain.trim_end().to_string());
            }
            self.displayed_text = displayed;
            let rows_used = if is_alt {
                rows as i32
            } else {
                last_content + 1
            };
            return BuiltScreen {
                spans,
                cursor_row: cur_row as i32,
                cursor_col: cur_col as i32,
                rows_used,
                is_alt,
                scroll_max: if is_alt { 0 } else { self.history.len() as i32 },
                scroll_offset: 0,
            };
        }

        // --- Scrolled view: window into history ++ live content -------------
        let live: Vec<Line> = {
            let s = self.parser.screen();
            (0..rows).map(|r| build_row(s, r, cols)).collect()
        };
        let hist_len = self.history.len();
        // Include the screen's trailing blank rows in the scroll range so this
        // scrolled view stays continuous with the live view (view_offset 0).
        // Trimming to only the used rows made the two views misalign after a
        // shrink-then-grow (dragging the SFTP panel over the terminal and back),
        // so scrolling back jumped at the bottom instead of moving line-by-line
        // (#119-followup).
        let combined_len = hist_len + live.len();
        let win = rows as usize;
        let start = combined_len.saturating_sub(win + self.view_offset);
        let end = (start + win).min(combined_len);

        let mut spans = Vec::new();
        let mut displayed = Vec::with_capacity(win);
        for (d, idx) in (start..end).enumerate() {
            let line: &Line = if idx < hist_len {
                &self.history[idx]
            } else {
                &live[idx - hist_len]
            };
            let runs = highlight_plain_output(
                line.1.clone(),
                self.output_highlight,
                &self.custom_highlight_rules,
            );
            for hs in &runs {
                spans.extend(render_term_span(hs, d as i32, self.is_dark));
            }
            displayed.push(line.0.trim_end().to_string());
        }
        while displayed.len() < win {
            displayed.push(String::new());
        }
        self.displayed_text = displayed;
        BuiltScreen {
            spans,
            cursor_row: -1, // hide the live cursor while viewing history
            cursor_col: 0,
            rows_used: win as i32,
            is_alt: false,
            scroll_max: self.history.len() as i32,
            scroll_offset: self.view_offset as i32,
        }
    }
}
