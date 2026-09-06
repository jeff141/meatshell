use super::*;

fn choose_download_conflict(remote: &str, local_dir: &str) -> Option<DownloadConflict> {
    let target = download_target_path(remote, local_dir);
    if !target.is_file() {
        return Some(DownloadConflict::Replace);
    }
    let replace = t("替换", "Replace");
    let keep_both = t("共存", "Keep both");
    let cancel = t("取消", "Cancel");
    let result = rfd::MessageDialog::new()
        .set_title(t("文件已存在", "File already exists"))
        .set_description(format!(
            "{}\n{}",
            target.display(),
            t(
                "请选择替换现有文件，或保留两份文件。",
                "Choose whether to replace the existing file or keep both files."
            )
        ))
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
            replace.to_string(),
            keep_both.to_string(),
            cancel.to_string(),
        ))
        .show();
    match result {
        rfd::MessageDialogResult::Yes => Some(DownloadConflict::Replace),
        rfd::MessageDialogResult::No => Some(DownloadConflict::KeepBoth),
        rfd::MessageDialogResult::Custom(value) if value == replace => {
            Some(DownloadConflict::Replace)
        }
        rfd::MessageDialogResult::Custom(value) if value == keep_both => {
            Some(DownloadConflict::KeepBoth)
        }
        _ => None,
    }
}

pub(super) fn wire_sftp_callbacks(
    window: &AppWindow,
    sftp_handles: SftpHandles,
    sftp_last_cwd: SftpLastCwd,
) {
    // Navigate to a remote path (or ".." to go up one level).
    {
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let weak = window.as_weak();
        window.on_sftp_navigate(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            // A pasted path may carry trailing whitespace / newline (#54).
            let path = path.trim();
            let resolved = if path == ".." {
                let current = weak.upgrade().and_then(|w| {
                    let terminals_rc = w.get_terminals();
                    let terminals = terminals_rc
                        .as_any()
                        .downcast_ref::<VecModel<TerminalState>>()?;
                    for i in 0..terminals.row_count() {
                        if let Some(row) = terminals.row_data(i) {
                            if row.id.as_str() == tab_id {
                                return Some(row.sftp_path.to_string());
                            }
                        }
                    }
                    None
                });
                parent_path(&current.unwrap_or_else(|| "/".to_string()))
            } else {
                path.to_string()
            };
            // Forget the followed cwd so the next OSC 7 — even at an unchanged
            // directory — snaps the panel back to the shell's cwd; manual
            // navigation never permanently disables cd-follow.
            sftp_last_cwd.lock().unwrap().remove(&tab_id);
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab_id) {
                    h.list_dir(resolved);
                }
            }
        });
    }

    // Download a remote file.  If a download folder is preset in settings, save
    // straight there; otherwise fall back to a native folder picker.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_download(move |tab_id: SharedString, remote_path: SharedString| {
            let tab_id = tab_id.to_string();
            let remote_path = remote_path.to_string();
            // If the user has checked 2+ entries, ANY download (right-click,
            // row button or the toolbar) packs the whole checked set into one
            // archive (#100) — this matches "download these together". A single
            // checked item (or none) downloads the clicked file as-is.
            let (arc_dir, arc_names) = weak
                .upgrade()
                .and_then(|w| {
                    let terminals = w.get_terminals();
                    let tm = terminals
                        .as_any()
                        .downcast_ref::<VecModel<TerminalState>>()?;
                    let paths = collect_sftp_selected(tm, &tab_id);
                    if paths.len() >= 2 {
                        let dir = active_sftp_path(&w, &tab_id);
                        let names: Vec<String> = paths
                            .iter()
                            .map(|p| {
                                p.trim_end_matches('/')
                                    .rsplit(['/', '\\'])
                                    .next()
                                    .unwrap_or(p)
                                    .to_string()
                            })
                            .collect();
                        clear_sftp_selection(tm, &tab_id);
                        Some((dir, names))
                    } else {
                        None
                    }
                })
                .map(|(d, n)| (Some(d), n))
                .unwrap_or((None, Vec::new()));
            // "Always ask" (#87) forces the folder picker, ignoring the preset.
            let (preset, always_ask) = weak
                .upgrade()
                .map(|w| {
                    (
                        w.get_download_dir().to_string(),
                        w.get_download_always_ask(),
                    )
                })
                .unwrap_or_default();
            if !always_ask && !preset.is_empty() {
                if let Ok(handles) = sftp_handles.lock() {
                    if let Some(h) = handles.get(&tab_id) {
                        if let Some(ref dir) = arc_dir {
                            h.download_archive(dir.clone(), arc_names.clone(), preset);
                        } else if let Some(conflict) =
                            choose_download_conflict(&remote_path, &preset)
                        {
                            h.download(remote_path, preset, conflict);
                        }
                        // Pop the transfers panel so progress is visible (user
                        // request: any download opens the download popup).
                        if let Some(w) = weak.upgrade() {
                            w.set_download_open(true);
                        }
                    }
                }
                return;
            }
            let sftp_handles = sftp_handles.clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let local_dir = dir.to_string_lossy().to_string();
                    if let Ok(handles) = sftp_handles.lock() {
                        if let Some(h) = handles.get(&tab_id) {
                            if let Some(ref rdir) = arc_dir {
                                h.download_archive(rdir.clone(), arc_names.clone(), local_dir);
                            } else if let Some(conflict) =
                                choose_download_conflict(&remote_path, &local_dir)
                            {
                                h.download(remote_path, local_dir, conflict);
                            }
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(|w| w.set_download_open(true));
                }
            });
        });
    }

    // Upload a local file into the current remote directory.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_upload_clicked(
            move |tab_id: SharedString, remote_dir: SharedString, folder: bool| {
                let tab_id = tab_id.to_string();
                let remote_dir = remote_dir.to_string();
                let sftp_handles = sftp_handles.clone();
                // Session-sync upload (#sync): when both the sync toggle and the
                // "sync upload" setting are on, mirror the upload to every other
                // online session — each into *that session's own* current SFTP
                // directory (paths differ between sessions, e.g. /home/jeff vs
                // /home/root, so the active session's path can't be reused).
                // Gather targets on the UI thread (Slint models aren't Send).
                let sync_targets: Vec<(String, String)> = weak
                    .upgrade()
                    .filter(|w| w.get_sync_input() && w.get_sync_upload_enabled())
                    .map(|w| {
                        let paths = terminal_sftp_paths(&w);
                        let handles = sftp_handles.lock().ok();
                        handles
                            .iter()
                            .flat_map(|h| h.keys())
                            .filter(|id| *id != &tab_id)
                            .filter_map(|id| paths.get(id).map(|dir| (id.clone(), dir.clone())))
                            .filter(|(_, dir)| !dir.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                std::thread::spawn(move || {
                    // The remote SFTP upload handles a file or a whole directory;
                    // only the local picker differs (#85). Folder uploads one dir;
                    // file mode allows selecting several at once.
                    let locals: Vec<std::path::PathBuf> = if folder {
                        rfd::FileDialog::new()
                            .pick_folder()
                            .map(|p| vec![p])
                            .unwrap_or_default()
                    } else {
                        rfd::FileDialog::new()
                            .pick_files()
                            .map(|v| v.into_iter().collect())
                            .unwrap_or_default()
                    };
                    if locals.is_empty() {
                        return;
                    }
                    if let Ok(handles) = sftp_handles.lock() {
                        if let Some(h) = handles.get(&tab_id) {
                            for local in &locals {
                                h.upload(local.clone(), remote_dir.clone());
                            }
                        }
                        // Mirror to the other online sessions, each into its own
                        // current SFTP directory.
                        for (id, dir) in &sync_targets {
                            if let Some(h) = handles.get(id) {
                                for local in &locals {
                                    h.upload(local.clone(), dir.clone());
                                }
                            }
                        }
                    }
                });
            },
        );
    }

    // Refresh the current directory listing.
    {
        let sftp_handles = sftp_handles.clone();
        // Added (#sftp-refresh-selection): also need `weak` so we can drop the
        // selection counter back to zero after the file list is rebuilt. Without
        // this, the toolbar kept showing the old `sftp_selected_count` (e.g. "2")
        // even though the freshly loaded entries were all unchecked, and the
        // batch-download / batch-delete buttons stayed visible until the user
        // manually toggled a checkbox to force a recount.
        let weak = window.as_weak();
        window.on_sftp_refresh(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            let path = path.to_string();
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab_id) {
                    // Refresh re-syncs the left tree too, not just the file list (#189).
                    h.refresh_dir(path);
                }
            }
            // Reset the multi-select counter to match the new (empty) selection.
            // The list itself gets replaced by `refresh_dir` with fresh entries
            // whose `selected=false`, so this only really needs to clamp the
            // cached count, but reusing `clear_sftp_selection` keeps the
            // post-refresh state identical to what other actions (navigate,
            // download-selected, delete-selected, copy-selected-to-target) leave
            // behind — single source of truth (#sftp-refresh-selection).
            if let Some(w) = weak.upgrade() {
                let terminals = w.get_terminals();
                if let Some(tm) =
                    terminals.as_any().downcast_ref::<VecModel<TerminalState>>()
                {
                    clear_sftp_selection(tm, &tab_id);
                }
            }
        });
    }

    // Toggle tree node expand/collapse and navigate to that directory.
    {
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        window.on_sftp_tree_expand(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            let path = path.to_string();
            // Forget the followed cwd (see on_sftp_navigate): tree navigation
            // must never permanently disable cd-follow.
            sftp_last_cwd.lock().unwrap().remove(&tab_id);
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab_id) {
                    h.toggle_tree_node(path.clone());
                    h.list_dir(path);
                }
            }
        });
    }

    // Context menu → 删除 a remote file. The irreversible-delete confirmation
    // (#28) is handled by the in-app ConfirmDialog in the UI layer, so by the
    // time this fires the user has already confirmed.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_delete(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.delete(path.to_string());
                }
            }
        });
    }

    // SFTP file-list sorting (#248): click a header to cycle asc -> desc -> default.
    {
        let weak = window.as_weak();
        window.on_sftp_sort_request(move |tab_id: SharedString, key: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            update_terminal_row(tm, tab_id.as_str(), |row| {
                let key = key.to_string();
                let next_dir = if row.sftp_sort_key.as_str() != key || row.sftp_sort_dir == 0 {
                    1
                } else if row.sftp_sort_dir > 0 {
                    -1
                } else {
                    0
                };
                let next_key = if next_dir == 0 { String::new() } else { key };
                row.sftp_entries =
                    sorted_sftp_entries_from_model(&row.sftp_entries, &next_key, next_dir);
                row.sftp_sort_key = next_key.into();
                row.sftp_sort_dir = next_dir;
            });
        });
    }
    {
        let weak = window.as_weak();
        window.on_sftp_clear_sort(move |tab_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            update_terminal_row(tm, tab_id.as_str(), |row| {
                row.sftp_entries = sorted_sftp_entries_from_model(&row.sftp_entries, "", 0);
                row.sftp_sort_key = "".into();
                row.sftp_sort_dir = 0;
            });
        });
    }

    // SFTP multi-select: toggle a row's checkbox + recount (#100).
    {
        let weak = window.as_weak();
        window.on_sftp_toggle_select(move |tab_id: SharedString, idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            for ti in 0..tm.row_count() {
                let Some(row) = tm.row_data(ti) else { continue };
                if row.id.as_str() != tab_id.as_str() {
                    continue;
                }
                if let Some(em) = row
                    .sftp_entries
                    .as_any()
                    .downcast_ref::<VecModel<SftpEntry>>()
                {
                    let i = idx as usize;
                    if let Some(mut e) = em.row_data(i) {
                        e.selected = !e.selected;
                        em.set_row_data(i, e);
                    }
                    let mut n = 0;
                    for ei in 0..em.row_count() {
                        if em.row_data(ei).map(|x| x.selected).unwrap_or(false) {
                            n += 1;
                        }
                    }
                    let mut r = row.clone();
                    r.sftp_selected_count = n;
                    tm.set_row_data(ti, r);
                }
                break;
            }
        });
    }
    // SFTP multi-select: download all checked entries into one folder (#100).
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_download_selected(move |tab_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            let paths = collect_sftp_selected(tm, tab_id.as_str());
            if paths.is_empty() {
                return;
            }
            // Single selection downloads as a plain file (no compression, #100.3);
            // multiple selections are tar-packed into one archive on the remote
            // (#100.2) — this also avoids the concurrent-transfer races (#100.1).
            let single = paths.len() == 1;
            let remote_dir = active_sftp_path(&w, tab_id.as_str());
            let names: Vec<String> = paths
                .iter()
                .map(|p| {
                    p.trim_end_matches('/')
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(p)
                        .to_string()
                })
                .collect();
            let preset = w.get_download_dir().to_string();
            let always_ask = w.get_download_always_ask();
            if !always_ask && !preset.is_empty() {
                if let Ok(handles) = sftp_handles.lock() {
                    if let Some(h) = handles.get(tab_id.as_str()) {
                        if single {
                            if let Some(conflict) =
                                choose_download_conflict(&paths[0], &preset)
                            {
                                h.download(paths[0].clone(), preset.clone(), conflict);
                            }
                        } else {
                            h.download_archive(remote_dir.clone(), names.clone(), preset.clone());
                        }
                    }
                }
                w.set_download_open(true);
            } else {
                let sftp_handles = sftp_handles.clone();
                let weak2 = weak.clone();
                let tab = tab_id.to_string();
                std::thread::spawn(move || {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        let dir = dir.to_string_lossy().to_string();
                        if let Ok(handles) = sftp_handles.lock() {
                            if let Some(h) = handles.get(&tab) {
                                if single {
                                    if let Some(conflict) =
                                        choose_download_conflict(&paths[0], &dir)
                                    {
                                        h.download(paths[0].clone(), dir.clone(), conflict);
                                    }
                                } else {
                                    h.download_archive(
                                        remote_dir.clone(),
                                        names.clone(),
                                        dir.clone(),
                                    );
                                }
                            }
                        }
                        let _ = weak2.upgrade_in_event_loop(|w| w.set_download_open(true));
                    }
                });
            }
            clear_sftp_selection(tm, tab_id.as_str());
        });
    }
    // SFTP multi-select: delete all checked entries (confirmed in the UI) (#100).
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_delete_selected(move |tab_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            let paths = collect_sftp_selected(tm, tab_id.as_str());
            if paths.is_empty() {
                return;
            }
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    for p in &paths {
                        h.delete(p.clone());
                    }
                }
            }
            clear_sftp_selection(tm, tab_id.as_str());
        });
    }

    // Context menu → 查看 (read-only) / 编辑 (editable). Both load the file's
    // text into the built-in editor instead of an external app (#70).
    // SFTP remote-to-remote copy (#203): stage through a local temp directory,
    // then upload into the target session's current SFTP directory.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_copy_to_target(
            move |tab_id: SharedString, remote_path: SharedString, target_id: SharedString| {
                let Some(w) = weak.upgrade() else { return };
                let paths = vec![remote_path.to_string()];
                let dirs = terminal_sftp_paths(&w);
                let target_dir = dirs
                    .get(target_id.as_str())
                    .cloned()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "/".to_string());
                if let Ok(handles) = sftp_handles.lock() {
                    let Some(src) = handles.get(tab_id.as_str()) else {
                        return;
                    };
                    let Some(dst) = handles.get(target_id.as_str()) else {
                        return;
                    };
                    src.copy_to(paths, dst.commands.clone(), target_dir);
                    w.set_download_open(true);
                }
            },
        );
    }
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_copy_selected_to_target(
            move |tab_id: SharedString, target_id: SharedString| {
                let Some(w) = weak.upgrade() else { return };
                let terminals = w.get_terminals();
                let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                    return;
                };
                let paths = collect_sftp_selected(tm, tab_id.as_str());
                if paths.is_empty() {
                    return;
                }
                let dirs = terminal_sftp_paths(&w);
                let target_dir = dirs
                    .get(target_id.as_str())
                    .cloned()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "/".to_string());
                if let Ok(handles) = sftp_handles.lock() {
                    let Some(src) = handles.get(tab_id.as_str()) else {
                        return;
                    };
                    let Some(dst) = handles.get(target_id.as_str()) else {
                        return;
                    };
                    src.copy_to(paths, dst.commands.clone(), target_dir);
                    w.set_download_open(true);
                }
                clear_sftp_selection(tm, tab_id.as_str());
            },
        );
    }
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_view(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.read_text(path.to_string(), false);
                }
            }
        });
    }
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_edit(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.read_text(path.to_string(), true);
                }
            }
        });
    }
    // Open / edit with an external program (#81): download to a temp file and
    // hand it to the OS default app. Edit mode watches the temp copy and
    // re-uploads on every change.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_open_external(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.open_temp(path.to_string(), false);
                }
            }
        });
    }
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_edit_external(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(tab_id.as_str()) {
                    h.open_temp(path.to_string(), true);
                }
            }
        });
    }

    // Context-menu extensions (#69): one prompt dialog covers rename / chmod /
    // mkdir / touch; copy-path goes straight to the system clipboard.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_prompt_submit(
            move |tab_id: SharedString,
                  kind: SharedString,
                  target: SharedString,
                  value: SharedString| {
                let value = value.to_string();
                let value = value.trim();
                if value.is_empty() {
                    return;
                }
                let target = target.to_string();
                let handles = match sftp_handles.lock() {
                    Ok(h) => h,
                    Err(_) => return,
                };
                let Some(h) = handles.get(tab_id.as_str()) else {
                    return;
                };
                match kind.as_str() {
                    "rename" => {
                        let to =
                            format!("{}/{}", parent_path(&target).trim_end_matches('/'), value);
                        h.rename(target, to);
                    }
                    "mkdir" => {
                        h.mkdir(format!("{}/{}", target.trim_end_matches('/'), value));
                    }
                    "touch" => {
                        h.touch(format!("{}/{}", target.trim_end_matches('/'), value));
                    }
                    _ => {}
                }
            },
        );
    }
    {
        // Run on a spawned thread, NOT the Slint UI thread. clipboard_set_text
        // (arboard) can block when the OS clipboard is held by another process
        // (Windows OpenClipboard / Wayland wait()); doing it synchronously here
        // freezes the whole UI event loop (#sftp-copy-path-freeze). Every other
        // caller in app.rs wraps clipboard writes in std::thread::spawn — this
        // one was the only outlier.
        window.on_sftp_copy_path(move |path: SharedString| {
            std::thread::spawn(move || clipboard_set_text(path.to_string()));
        });
    }

    // Visual chmod dialog (#84): decompose the current mode into nine bools on
    // open, recompose on apply (Slint has no bitwise ops).
    {
        let weak = window.as_weak();
        window.on_sftp_chmod_open(
            move |tab: SharedString, path: SharedString, name: SharedString, mode: i32| {
                let Some(w) = weak.upgrade() else { return };
                let m = mode as u32;
                w.set_chmod_tab(tab);
                w.set_chmod_path(path);
                w.set_chmod_name(name);
                w.set_chmod_or(m & 0o400 != 0);
                w.set_chmod_ow(m & 0o200 != 0);
                w.set_chmod_ox(m & 0o100 != 0);
                w.set_chmod_gr(m & 0o040 != 0);
                w.set_chmod_gw(m & 0o020 != 0);
                w.set_chmod_gx(m & 0o010 != 0);
                w.set_chmod_tr(m & 0o004 != 0);
                w.set_chmod_tw(m & 0o002 != 0);
                w.set_chmod_tx(m & 0o001 != 0);
                w.set_chmod_open(true);
            },
        );
    }
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_chmod_apply(move || {
            let Some(w) = weak.upgrade() else { return };
            let mode = (w.get_chmod_or() as u32) << 8
                | (w.get_chmod_ow() as u32) << 7
                | (w.get_chmod_ox() as u32) << 6
                | (w.get_chmod_gr() as u32) << 5
                | (w.get_chmod_gw() as u32) << 4
                | (w.get_chmod_gx() as u32) << 3
                | (w.get_chmod_tr() as u32) << 2
                | (w.get_chmod_tw() as u32) << 1
                | (w.get_chmod_tx() as u32);
            let path = w.get_chmod_path().to_string();
            let tab = w.get_chmod_tab().to_string();
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab) {
                    h.chmod(path, mode);
                }
            }
        });
    }

    // Rebuild the editor's line-number gutter after each edit (#81). The text
    // comes straight from the TextInput so we don't re-read the property.
    {
        let weak = window.as_weak();
        window.on_editor_recount(move |text: SharedString| {
            if let Some(w) = weak.upgrade() {
                w.set_editor_lines(editor_lines_for(text.as_str()));
            }
        });
    }

    // Built-in editor: save (Ctrl+S / button) writes the text back to the
    // remote file (#70). Read-only (view) sessions never save.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_save_file(move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_editor_readonly() {
                return;
            }
            let path = w.get_editor_path().to_string();
            let content = w.get_editor_content().to_string();
            let tab_id = w.get_active_tab_id().to_string();
            if let Ok(handles) = sftp_handles.lock() {
                if let Some(h) = handles.get(&tab_id) {
                    h.write_text(path, content);
                }
            }
            w.set_editor_dirty(false);
        });
    }
    // Closing the editor discards unsaved edits. Saving is an explicit action
    // (button / Ctrl+S); silently uploading on X or Esc is surprising and can
    // overwrite a remote file the user only meant to inspect (#287).
    {
        let weak = window.as_weak();
        window.on_close_editor(move || {
            let Some(w) = weak.upgrade() else { return };
            w.set_editor_open(false);
            w.set_editor_dirty(false);
            w.set_editor_find_query("".into());
            w.set_editor_replace_text("".into());
            w.set_editor_match_count(0);
            w.set_editor_find_position(-1);
        });
    }

    // Built-in editor find/replace (#287). Keep matching literal and
    // case-sensitive, which is predictable for configuration/source files.
    window.on_editor_count_matches(|content: SharedString, query: SharedString| {
        if query.is_empty() {
            0
        } else {
            content
                .matches(query.as_str())
                .count()
                .min(i32::MAX as usize) as i32
        }
    });
    window.on_editor_find_step(
        |content: SharedString, query: SharedString, current: i32, reverse: bool| {
            if query.is_empty() {
                return EditorFindResult {
                    start: 0,
                    end: 0,
                    count: 0,
                };
            }
            let positions: Vec<usize> = content
                .match_indices(query.as_str())
                .map(|(index, _)| index)
                .collect();
            let selected = if reverse {
                positions
                    .iter()
                    .rev()
                    .copied()
                    .find(|position| current < 0 || *position < current as usize)
                    .or_else(|| positions.last().copied())
            } else {
                positions
                    .iter()
                    .copied()
                    .find(|position| current < 0 || *position > current as usize)
                    .or_else(|| positions.first().copied())
            };
            let start = selected.unwrap_or(0).min(i32::MAX as usize) as i32;
            EditorFindResult {
                start,
                end: start.saturating_add(query.len().min(i32::MAX as usize) as i32),
                count: positions.len().min(i32::MAX as usize) as i32,
            }
        },
    );
    {
        let weak = window.as_weak();
        window.on_editor_replace_all(move |query: SharedString, replacement: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            if w.get_editor_readonly() || query.is_empty() {
                return;
            }
            let replaced = w
                .get_editor_content()
                .replace(query.as_str(), replacement.as_str());
            w.set_editor_content(replaced.clone().into());
            w.set_editor_dirty(true);
            w.set_editor_lines(editor_lines_for(&replaced));
            w.set_editor_match_count(0);
        });
    }
}

// ---------------------------------------------------------------------------
// Raw keystroke forwarding and PTY resize
// ---------------------------------------------------------------------------
