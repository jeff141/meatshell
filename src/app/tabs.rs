//! Tab/pane rendering and scheduling helpers.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::layout::Layout;
use crate::ssh::SessionHandle;
use crate::terminal::{RenderGates, TermBuffers};
use super::*;

/// Tab ids currently shown in a pane (`term.id == pane.active-id` in Slint).
pub(crate) fn visible_tab_ids(win: &AppWindow) -> HashSet<String> {
    use slint::Model as _;
    let mut out = HashSet::new();
    let panes = win.get_panes();
    if let Some(pm) = panes.as_any().downcast_ref::<VecModel<PaneInfo>>() {
        for i in 0..pm.row_count() {
            if let Some(pane) = pm.row_data(i) {
                out.insert(pane.active_id.to_string());
            }
        }
    }
    out
}

pub(crate) fn request_tab_render(
    weak: slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gates: &RenderGates,
) {
    let gate = {
        let m = gates.lock().unwrap();
        m.get(tab_id).cloned()
    };
    let Some(gate) = gate else { return };
    gate.pending.store(true, Ordering::Release);
    if gate.scheduled.swap(true, Ordering::AcqRel) {
        return;
    }

    let weak2 = weak.clone();
    let tid = tab_id.to_string();
    let bufs2 = bufs.clone();
    let gates2 = gates.clone();
    // Always bounce through the event loop from pump / worker threads.
    // Never call invoke_from_event_loop from inside a UI callback — that
    // deadlocks Slint (opening a second tab then froze the whole app).
    let _ = slint::invoke_from_event_loop(move || {
        run_coalesced_tab_render(&weak2, &tid, &bufs2, &gates2);
    });
}

/// UI-thread entry: honour the throttle, then render. Timer must be created
/// here — not on pump threads (#209).
pub(crate) fn run_coalesced_tab_render(
    weak: &slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gates: &RenderGates,
) {
    let gate = {
        let m = gates.lock().unwrap();
        m.get(tab_id).cloned()
    };
    let Some(gate) = gate else { return };

    let delay = {
        let last = *gate.last_render.lock().unwrap();
        RENDER_MIN_INTERVAL.saturating_sub(last.elapsed())
    };

    let weak2 = weak.clone();
    let tid = tab_id.to_string();
    let bufs2 = bufs.clone();
    let gates2 = gates.clone();

    if delay.is_zero() {
        do_tab_render_flush(&weak2, &tid, &bufs2, &gates2);
    } else {
        slint::Timer::single_shot(delay, move || {
            do_tab_render_flush(&weak2, &tid, &bufs2, &gates2);
        });
    }
}

/// UI-thread only: push the vt100 screen into Slint, then reschedule if more
/// output arrived while we were rendering (#209).
pub(crate) fn do_tab_render_flush(
    weak: &slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gates: &RenderGates,
) {
    let gate = {
        let m = gates.lock().unwrap();
        m.get(tab_id).cloned()
    };
    let Some(gate) = gate else { return };
    gate.scheduled.store(false, Ordering::Release);

    if let Some(win) = weak.upgrade() {
        if visible_tab_ids(&win).contains(tab_id) {
            rebuild_tab_display(&win, bufs, tab_id);
            *gate.last_render.lock().unwrap() = std::time::Instant::now();
        }
    }

    if gate.pending.swap(false, Ordering::AcqRel) {
        if !gate.scheduled.swap(true, Ordering::AcqRel) {
            run_coalesced_tab_render(weak, tab_id, bufs, gates);
        }
    }
}

/// Apply a settled terminal size to the PTY + vt100 grid. Factored out of the
/// resize callback so that callback can debounce — a layout reflow can briefly
/// report a near-zero width, collapsing term-cols to its 10-col floor; applying
/// that to the remote PTY reflows vt100 and garbles running output like a
/// `git clone` progress meter (#163). Debouncing means only the settled size
/// ever reaches the server.
pub(crate) fn apply_terminal_resize(
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: &TermBuffers,
    last_term_size: &Arc<Mutex<(u32, u32)>>,
    tab_id: &str,
    cols: u32,
    rows: u32,
) {
    *last_term_size.lock().unwrap() = (cols, rows);
    if let Some(handle) = handles.borrow().get(tab_id) {
        handle.resize(cols, rows);
    }
    if let Some(h) = super::term_buf(bufs, tab_id) {
        let mut buf = h.lock().unwrap();
        let (old_rows, old_cols) = buf.parser.screen().size();
        let (new_rows, new_cols) = (rows as u16, cols as u16);
        if (new_rows, new_cols) != (old_rows, old_cols) {
            if buf.parser.screen().alternate_screen() {
                // Alt-screen (tmux/vim/btop): the remote redraws the whole screen
                // on SIGWINCH, so just resize the grid and let that redraw fill it.
                buf.parser.set_size(new_rows, new_cols);
            } else {
                // Reflow already-printed output to the new width by replaying the
                // byte stream — vt100's set_size only truncates/pads (#169).
                buf.reflow(new_rows, new_cols);
            }
            // The pre/post-resize screens differ; drop the scroll-detection
            // snapshot so the next output isn't mis-read as a scroll.
            buf.prev.clear();
        }
    }
}

/// Recompute spans + cursor + find/selection highlights for one tab from its
/// current vt100 screen (respecting scrollback) and push them to the model.
/// Used by scroll + selection callbacks (Output has its own equivalent inline).
pub(crate) fn rebuild_tab_display(win: &AppWindow, bufs: &TermBuffers, tab_id: &str) {
    let data = with_term_buf(bufs, tab_id, |buf| {
        let cols = buf.parser.screen().size().1;
        let b = buf.render(); // also refreshes buf.displayed_text
        let matches = compute_find_matches(&buf.displayed_text, &buf.find_query);
        let sel = buf.selection_rects_visible(cols);
        (b, matches, sel)
    });
    let Some((b, matches, sel)) = data else {
        return;
    };
    let spans = ModelRc::from(Rc::new(VecModel::from(b.spans)));
    let fm = ModelRc::from(Rc::new(VecModel::from(matches)));
    let sm = ModelRc::from(Rc::new(VecModel::from(sel)));
    let (cr, cc, ru, alt) = (b.cursor_row, b.cursor_col, b.rows_used, b.is_alt);
    let (smax, soff) = (b.scroll_max, b.scroll_offset);
    set_terminal_row(win, tab_id, move |row| {
        row.spans = spans.clone();
        row.cursor_row = cr;
        row.cursor_col = cc;
        row.rows_used = ru;
        row.is_alt_screen = alt;
        row.find_matches = fm.clone();
        row.selection = sm.clone();
        row.scroll_max = smax;
        row.scroll_offset = soff;
    });
    win.window().request_redraw();
}

/// True when two tab sub-models hold the same ids in the same order.
pub(crate) fn tabs_eq(a: &ModelRc<TabInfo>, b: &ModelRc<TabInfo>) -> bool {
    if a.row_count() != b.row_count() {
        return false;
    }
    (0..a.row_count()).all(|i| match (a.row_data(i), b.row_data(i)) {
        (Some(x), Some(y)) => x.id == y.id,
        _ => false,
    })
}

/// Find the terminal row with `tab_id`, apply `mutator`, and write it back.
pub(crate) fn update_terminal_row(
    model: &VecModel<TerminalState>,
    tab_id: &str,
    mutator: impl FnOnce(&mut TerminalState),
) {
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id.as_str() == tab_id {
                mutator(&mut row);
                model.set_row_data(i, row);
                return;
            }
        }
    }
}

pub(crate) fn refresh_panes(
    window: &AppWindow,
    layout: &Layout,
    content: (f32, f32),
    tabs_model: &VecModel<TabInfo>,
    panes_model: &VecModel<PaneInfo>,
    splitters_model: &VecModel<SplitterInfo>,
) {
    let (cw, ch) = (content.0.max(1.0), content.1.max(1.0));
    let (panes, splits) = layout.flatten(0.0, 0.0, cw, ch);

    let pane_infos: Vec<PaneInfo> = panes
        .iter()
        .map(|p| {
            // Map this pane's tab ids to their TabInfo rows (skipping any not yet
            // in the model).
            let tabs: Vec<TabInfo> = p
                .tabs
                .iter()
                .filter_map(|tid| {
                    (0..tabs_model.row_count()).find_map(|i| {
                        let row = tabs_model.row_data(i)?;
                        (row.id.as_str() == tid.as_str()).then_some(row)
                    })
                })
                .collect();
            // Only the pane touching the top-right corner keeps room for the
            // floating toolbar icons (#122).
            let top_right = p.x + p.w >= cw - 0.5 && p.y <= 0.5;
            PaneInfo {
                id: p.id as i32,
                x: p.x,
                y: p.y,
                w: p.w,
                h: p.h,
                active_id: p.active.clone().into(),
                focused: p.focused,
                reserve_right: if top_right { 140.0 } else { 0.0 },
                tabs: ModelRc::from(Rc::new(VecModel::from(tabs))),
            }
        })
        .collect();

    // Update the models IN PLACE rather than replacing them, so the `for pane` /
    // `for sp` elements are reused: this keeps terminals from being recreated on
    // every refresh AND preserves the splitter's pointer-grab during a drag (a
    // fresh model would destroy the element mid-drag and drop the grab). When the
    // structure changes (split/close → different row count) a full rebuild is fine
    // since no drag is in flight.
    if panes_model.row_count() == pane_infos.len() {
        for (i, mut r) in pane_infos.into_iter().enumerate() {
            if let Some(old) = panes_model.row_data(i) {
                // Reuse the existing tab sub-model when the tabs are unchanged so a
                // geometry-only refresh doesn't churn the tab strips.
                if old.id == r.id && tabs_eq(&old.tabs, &r.tabs) {
                    r.tabs = old.tabs;
                }
            }
            panes_model.set_row_data(i, r);
        }
    } else {
        panes_model.set_vec(pane_infos);
    }

    let split_infos: Vec<SplitterInfo> = splits
        .iter()
        .map(|s| SplitterInfo {
            split_id: s.split_id as i32,
            x: s.x,
            y: s.y,
            w: s.w,
            h: s.h,
            vertical: s.vertical,
        })
        .collect();
    if splitters_model.row_count() == split_infos.len() {
        for (i, r) in split_infos.into_iter().enumerate() {
            splitters_model.set_row_data(i, r);
        }
    } else {
        splitters_model.set_vec(split_infos);
    }

    if let Some(fp) = panes.iter().find(|p| p.focused) {
        if window.get_active_tab_id().as_str() != fp.active.as_str() {
            window.set_active_tab_id(fp.active.clone().into());
        }
    }
}

/// Hit-test a drag point (pane-area coords) to a target pane + drop zone, plus
/// the highlight rect the dropped tab would affect. Zone is one of
/// "tabstrip"/"left"/"right"/"up"/"down"/"center"; `None` when the point is
/// outside every pane. The 30% edge bands trigger a split; the tab strip and
/// middle drop into the pane's tab group.
pub(crate) fn drag_target(
    layout: &Layout,
    content: (f32, f32),
    x: f32,
    y: f32,
) -> Option<(u64, &'static str, (f32, f32, f32, f32))> {
    const STRIP: f32 = 36.0;
    const EDGE: f32 = 0.30;
    let (cw, ch) = (content.0.max(1.0), content.1.max(1.0));
    let (panes, _) = layout.flatten(0.0, 0.0, cw, ch);
    let p = panes
        .iter()
        .find(|p| x >= p.x && x < p.x + p.w && y >= p.y && y < p.y + p.h)?;
    let body_top = p.y + STRIP;
    if y < body_top {
        let ix = x.clamp(p.x + 3.0, p.x + p.w - 3.0) - 3.0;
        return Some((p.id, "tabstrip", (ix, p.y + 4.0, 6.0, STRIP - 8.0)));
    }
    let bw = p.w.max(1.0);
    let bh = (p.h - STRIP).max(1.0);
    let rx = (x - p.x) / bw;
    let ry = (y - body_top) / bh;
    let (dl, dr, dt, db) = (rx, 1.0 - rx, ry, 1.0 - ry);
    let m = dl.min(dr).min(dt).min(db);
    let (zone, rect) = if m > EDGE {
        ("center", (p.x, p.y, p.w, p.h))
    } else if m == dl {
        ("left", (p.x, p.y, p.w * 0.5, p.h))
    } else if m == dr {
        ("right", (p.x + p.w * 0.5, p.y, p.w * 0.5, p.h))
    } else if m == dt {
        ("up", (p.x, p.y, p.w, p.h * 0.5))
    } else {
        ("down", (p.x, p.y + p.h * 0.5, p.w, p.h * 0.5))
    };
    Some((p.id, zone, rect))
}

/// Mutate the `TerminalState` whose id matches `tab_id` in the live model.
/// Must run on the Slint event loop thread.
pub(crate) fn set_terminal_row(win: &AppWindow, tab_id: &str, mutator: impl Fn(&mut TerminalState)) {
    let terminals = win.get_terminals();
    let Some(model) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
        return;
    };
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id.as_str() == tab_id {
                mutator(&mut row);
                model.set_row_data(i, row);
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tab callbacks
// ---------------------------------------------------------------------------

pub(crate) fn wire_tab_callbacks(window: &AppWindow, ctx: Rc<AppContext>) {
    let tabs_model = ctx.tabs_model.clone();
    let terminals_model = ctx.terminals_model.clone();
    let layout = ctx.layout.clone();
    let content_size = ctx.content_size.clone();
    let panes_model = ctx.panes_model.clone();
    let splitters_model = ctx.splitters_model.clone();
    let handles = ctx.handles.clone();
    let bufs = ctx.bufs.clone();
    let render_gates = ctx.render_gates.clone();
    let sftp_handles = ctx.sftp_handles.clone();
    let sftp_last_cwd = ctx.sftp_last_cwd.clone();
    // Select a tab inside a pane: make it that pane's active tab and focus the
    // pane. refresh_panes propagates active-tab-id (→ sidebar refresh).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        let bufs_tab_sel = bufs.clone();
        window.on_pane_tab_selected(move |pane_id: i32, id: SharedString| {
            let id = id.to_string();
            {
                let mut lay = layout.borrow_mut();
                lay.focused = pane_id as u64;
                if let Some(l) = lay.leaf_mut(pane_id as u64) {
                    if l.tabs.iter().any(|t| t == &id) {
                        l.active = id.clone();
                    }
                }
            }
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
                // Tab just became visible — render any output ingested while it
                // was in the background (e.g. another session was unzipping).
                rebuild_tab_display(&w, &bufs_tab_sel, &id);
            }
        });
    }

    // Drag-to-reorder within a pane's strip: move the tab at `from` one slot in
    // `dir`. Only the pane's own tab order changes; content shows by active id.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_tab_reorder(move |pane_id: i32, from: i32, dir: i32| {
            {
                let mut lay = layout.borrow_mut();
                if let Some(l) = lay.leaf_mut(pane_id as u64) {
                    let n = l.tabs.len() as i32;
                    if n <= 1 {
                        return;
                    }
                    let from = from.clamp(0, n - 1);
                    let to = (from + dir).clamp(0, n - 1);
                    if from == to {
                        return;
                    }
                    let item = l.tabs.remove(from as usize);
                    l.tabs.insert(to as usize, item);
                }
            }
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }

    // Close a tab: tear down its session / buffers, drop it from the models, then
    // remove it from the split tree (which re-homes the pane's active tab and
    // collapses the pane if it becomes empty).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let terminals_model = terminals_model.clone();
        let handles = handles.clone();
        let bufs = bufs.clone();
        let render_gates = render_gates.clone();
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_tab_closed(move |_pane_id: i32, id: SharedString| {
            let id = id.to_string();
            if id == "welcome" {
                return;
            }
            if let Some(handle) = handles.borrow_mut().remove(&id) {
                handle.close();
            }
            if let Some(sftp) = sftp_handles.lock().unwrap().remove(&id) {
                sftp.close();
            }
            sftp_last_cwd.lock().unwrap().remove(&id);
            bufs.lock().unwrap().remove(&id);
            render_gates.lock().unwrap().remove(&id);

            // Remove from tabs + terminals models.
            let mut idx = None;
            for i in 0..tabs_model.row_count() {
                if tabs_model
                    .row_data(i)
                    .map(|r| r.id.as_str() == id)
                    .unwrap_or(false)
                {
                    idx = Some(i);
                    break;
                }
            }
            if let Some(i) = idx {
                tabs_model.remove(i);
            }
            let mut tidx = None;
            for i in 0..terminals_model.row_count() {
                if terminals_model
                    .row_data(i)
                    .map(|r| r.id.as_str() == id)
                    .unwrap_or(false)
                {
                    tidx = Some(i);
                    break;
                }
            }
            if let Some(i) = tidx {
                terminals_model.remove(i);
            }

            layout.borrow_mut().remove_tab(&id);
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }

    // "+" in a pane's strip: focus the welcome page (there is a single welcome
    // tab; move focus to whichever pane owns it and make it active).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_new_tab(move |pane_id: i32| {
            // In welcome-as-sidebar mode there is no welcome tab — the session list
            // lives in the left panel, so "+" has nothing to open.
            if weak
                .upgrade()
                .map(|w| w.get_welcome_as_sidebar())
                .unwrap_or(false)
            {
                return;
            }
            {
                let mut lay = layout.borrow_mut();
                if let Some(owner) = lay.leaf_of_tab("welcome") {
                    lay.focused = owner;
                    if let Some(l) = lay.leaf_mut(owner) {
                        l.active = "welcome".into();
                    }
                } else {
                    lay.focused = pane_id as u64;
                    lay.add_tab("welcome".into());
                }
            }
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }

    // Click anywhere in a pane → focus it (drives which terminal the sidebar and
    // key routing follow). A single pane is always focused, so this is a no-op
    // until splits exist.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_focus(move |pane_id: i32| {
            {
                let mut lay = layout.borrow_mut();
                if lay.leaf(pane_id as u64).is_some() {
                    lay.focused = pane_id as u64;
                }
            }
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }

    // Drag a splitter to re-balance the two panes it divides. `pos` is the new
    // boundary position in content coordinates along the split's axis; we look
    // the split's axis window up from a fresh flatten and convert it to a ratio.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_splitter_drag(move |split_id: i32, pos: f32, _vertical: bool| {
            {
                let mut lay = layout.borrow_mut();
                let (cw, ch) = content_size.get();
                let extent = {
                    let (_, splits) = lay.flatten(0.0, 0.0, cw.max(1.0), ch.max(1.0));
                    splits
                        .iter()
                        .find(|s| s.split_id == split_id as u64)
                        .map(|s| (s.axis_start, s.axis_len))
                };
                if let Some((start, len)) = extent {
                    lay.set_ratio(split_id as u64, start, len, pos);
                }
            }
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }

    // Split a pane: peel `tab-id` out of pane `pane-id` into a new pane on the
    // given side ("left"/"right"/"up"/"down"). Needs >1 tab so the source pane
    // doesn't empty and immediately collapse back.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_split(
            move |pane_id: i32, tab_id: SharedString, dir: SharedString| {
                let tab_id = tab_id.to_string();
                {
                    let mut lay = layout.borrow_mut();
                    let can = lay
                        .leaf(pane_id as u64)
                        .map(|l| l.tabs.len() > 1 && l.tabs.iter().any(|t| t == &tab_id))
                        .unwrap_or(false);
                    if !can {
                        return;
                    }
                    let (d, before) = match dir.as_str() {
                        "left" => (crate::layout::Dir::Horizontal, true),
                        "right" => (crate::layout::Dir::Horizontal, false),
                        "up" => (crate::layout::Dir::Vertical, true),
                        _ => (crate::layout::Dir::Vertical, false), // "down"
                    };
                    lay.split(pane_id as u64, d, &tab_id, before);
                }
                if let Some(w) = weak.upgrade() {
                    refresh_panes(
                        &w,
                        &layout.borrow(),
                        content_size.get(),
                        &tabs_model,
                        &panes_model,
                        &splitters_model,
                    );
                }
            },
        );
    }

    // Merge a split pane back into another pane. The source pane's tabs are
    // appended to the first remaining pane, then the emptied source collapses.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_merge(move |pane_id: i32| {
            {
                let mut lay = layout.borrow_mut();
                lay.merge_leaf_into_other(pane_id as u64);
            }
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }

    // Drag-to-split: while a tab is dragged over the pane area, highlight the
    // drop zone the cursor is in (an edge band → split, the middle → move).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        window.on_tab_drag_move(move |_tab_id: SharedString, x: f32, y: f32| {
            if let Some(w) = weak.upgrade() {
                match drag_target(&layout.borrow(), content_size.get(), x, y) {
                    Some((_, _, (hx, hy, hw, hh))) => {
                        w.set_drag_active(true);
                        w.set_drag_hl_x(hx);
                        w.set_drag_hl_y(hy);
                        w.set_drag_hl_w(hw);
                        w.set_drag_hl_h(hh);
                    }
                    None => w.set_drag_active(false),
                }
            }
        });
    }

    // Drop: split the target pane toward the dropped-on edge (peeling the tab
    // into the new pane), or drop into another pane's tab group from the middle
    // / tab strip (IDEA-style merge by dragging onto the tab row).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_tab_drag_drop(move |tab_id: SharedString, x: f32, y: f32| {
            let tab_id = tab_id.to_string();
            let target = drag_target(&layout.borrow(), content_size.get(), x, y);
            if let Some((pane, zone, _)) = target {
                let mut lay = layout.borrow_mut();
                let src = lay.leaf_of_tab(&tab_id);
                match zone {
                    "left" => {
                        lay.split(pane, crate::layout::Dir::Horizontal, &tab_id, true);
                    }
                    "right" => {
                        lay.split(pane, crate::layout::Dir::Horizontal, &tab_id, false);
                    }
                    "up" => {
                        lay.split(pane, crate::layout::Dir::Vertical, &tab_id, true);
                    }
                    "down" => {
                        lay.split(pane, crate::layout::Dir::Vertical, &tab_id, false);
                    }
                    "tabstrip" => {
                        if src != Some(pane) {
                            lay.move_tab(&tab_id, pane);
                        }
                    }
                    _ => {
                        if src != Some(pane) {
                            lay.move_tab(&tab_id, pane);
                        }
                    }
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_drag_active(false);
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }
}
