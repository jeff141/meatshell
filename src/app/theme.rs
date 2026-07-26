//! Theme, wallpaper, output-highlight and sidebar helpers.

use std::rc::Rc;

use slint::{Model, ModelRc, SharedString, VecModel};

use crate::config::{ConfigStore, OutputHighlightRule};
use crate::i18n::t;
use crate::resource::system::{format_bytes_per_sec, format_mem};
use crate::resource::{LocalSnap, NetHist, TabStatus, TabStatuses};
use crate::ssh::{ProcInfo, SystemDetails};
use crate::terminal::{CompiledOutputRule, OutputHighlightPreset, TermBuffers};
use crate::ui::*;

use super::{
    cpu_usage_detail_rows, disk_model, disk_rows, metric_rows, net_rows, normalized_model,
    pairs_to_one_row, pairs_to_overview_rows, pairs_to_rows, proc_rows, rebuild_tab_display,
    tuple5_rows,
};

pub(crate) fn compile_output_rules(rules: &[OutputHighlightRule]) -> Vec<CompiledOutputRule> {
    rules
        .iter()
        .filter(|rule| rule.enabled && !rule.pattern.trim().is_empty())
        .filter_map(|rule| {
            let pattern = if rule.regex {
                rule.pattern.clone()
            } else {
                regex::escape(&rule.pattern)
            };
            let matcher = regex::RegexBuilder::new(&pattern)
                .case_insensitive(!rule.case_sensitive)
                .build()
                .ok()?;
            Some(CompiledOutputRule {
                matcher,
                whole_line: rule.whole_line,
                ansi_index: highlight_color_index(&rule.color),
            })
        })
        .collect()
}

pub(crate) fn highlight_color_index(color: &str) -> u8 {
    match color {
        "yellow" => 11,
        "green" => 10,
        "cyan" => 14,
        "magenta" => 13,
        "gray" => 8,
        _ => 9,
    }
}

/// Resolve the user's saved theme preference to a dark/light bool (mirrors the
/// startup logic): "light"/"dark" win; otherwise ask the OS, defaulting to dark.
pub(crate) fn theme_pref_is_dark(store: &ConfigStore) -> bool {
    match store.theme_pref() {
        "light" => false,
        "dark" => true,
        _ => match dark_light::detect() {
            dark_light::Mode::Light => false,
            dark_light::Mode::Dark => true,
            dark_light::Mode::Default => true, // undetectable → dark
        },
    }
}

/// Flip the whole app between light and dark. Setting `Theme.dark` alone only
/// recolours the Slint chrome — each terminal bakes its ANSI/default colours
/// from a per-buffer `is_dark` flag at render time, so we must also update every
/// buffer and re-render it. Both the theme toggle and wallpaper switching route
/// through here (the proc-window mirror stays with the toggle).
pub(crate) fn apply_dark_mode(window: &AppWindow, bufs: &TermBuffers, dark: bool) {
    window.set_dark_mode(dark);
    {
        let handles: Vec<_> = bufs.lock().unwrap().values().cloned().collect();
        for h in handles {
            h.lock().unwrap().is_dark = dark;
        }
    }
    let tab_ids: Vec<String> = bufs.lock().unwrap().keys().cloned().collect();
    for tid in tab_ids {
        rebuild_tab_display(window, bufs, &tid);
    }
}

pub(crate) fn apply_output_highlight(
    window: &AppWindow,
    bufs: &TermBuffers,
    enabled: bool,
    preset: &str,
) {
    let mode = OutputHighlightPreset::from_settings(enabled, preset);
    {
        let handles: Vec<_> = bufs.lock().unwrap().values().cloned().collect();
        for handle in handles {
            handle.lock().unwrap().output_highlight = mode;
        }
    }
    let tab_ids: Vec<String> = bufs.lock().unwrap().keys().cloned().collect();
    for tab_id in tab_ids {
        rebuild_tab_display(window, bufs, &tab_id);
    }
}

pub(crate) fn apply_custom_output_rules(
    window: &AppWindow,
    bufs: &TermBuffers,
    rules: &[OutputHighlightRule],
) {
    let compiled = compile_output_rules(rules);
    {
        let handles: Vec<_> = bufs.lock().unwrap().values().cloned().collect();
        for handle in handles {
            handle.lock().unwrap().custom_highlight_rules = compiled.clone();
        }
    }
    let tab_ids: Vec<String> = bufs.lock().unwrap().keys().cloned().collect();
    for tab_id in tab_ids {
        rebuild_tab_display(window, bufs, &tab_id);
    }
}

/// Apply a wallpaper id to the window: load the image + derived palette, push the
/// immersive Theme overrides (accent / tint / image) and set `dark` from the
/// image luminance. An empty or undecodable id turns immersive mode off and
/// restores the user's saved light/dark theme.
pub(crate) fn apply_wallpaper(window: &AppWindow, store: &ConfigStore, bufs: &TermBuffers, id: &str, apply_builtin_theme: bool) {
    match crate::wallpaper::load(id) {
        Some(wp) => {
            let (ar, ag, ab) = wp.palette.accent;
            let (tr, tg, tb) = wp.palette.tint;
            window.set_wallpaper_img(wp.image);
            window.set_wp_accent(slint::Color::from_rgb_u8(ar, ag, ab));
            window.set_wp_tint(slint::Color::from_rgb_u8(tr, tg, tb));
            // Only the built-ins (designed as a light/dark pair) auto-set the
            // theme. A custom photo keeps the user's light/dark choice so the
            // theme toggle still governs text contrast — a light/white wallpaper
            // reads best in light mode (crisp dark text) rather than being forced
            // dark and greying the text out (#wallpaper).
            if apply_builtin_theme && crate::wallpaper::is_builtin(id) {
                apply_dark_mode(window, bufs, wp.palette.is_dark);
            }
            window.set_wallpaper_active(true);
            window.set_current_wallpaper(id.into());
            let name = if crate::wallpaper::is_builtin(id) {
                String::new()
            } else {
                std::path::Path::new(id)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            window.set_custom_wallpaper_name(name.into());
        }
        None => {
            window.set_wallpaper_active(false);
            window.set_current_wallpaper("".into());
            window.set_custom_wallpaper_name("".into());
            apply_dark_mode(window, bufs, theme_pref_is_dark(store));
        }
    }
}

/// Resolve which interface drives the top sparkline: the user's selection if it
/// still exists, otherwise the busiest (the list is sorted busiest-first).
/// Returns (name, rx_bps, tx_bps).
pub(crate) fn selected_iface(st: &TabStatus) -> (String, u64, u64) {
    if !st.selected_iface.is_empty() {
        if let Some(e) = st.net.iter().find(|e| e.0 == st.selected_iface) {
            return e.clone();
        }
    }
    st.net.first().cloned().unwrap_or_default()
}

/// The copyable IP/host from a `user@host` connection label (#192): the part
/// after the last `@`, trimmed. Falls back to the whole string when there's no
/// `@` (already a bare host/IP).
pub(crate) fn conn_ip(host: &str) -> String {
    host.rsplit('@').next().unwrap_or(host).trim().to_string()
}

/// Recompute the whole sidebar (status dot + CPU/mem/swap + dual network panel)
/// for whichever tab is active. Welcome tab → local machine; a session tab →
/// that server. The bottom network graph is always the local machine.
/// Must run on the Slint event loop thread.
pub(crate) fn refresh_sidebar(
    win: &AppWindow,
    statuses: &TabStatuses,
    local: &LocalSnap,
    local_net_hist: &NetHist,
) {
    let pct = |used: u64, total: u64| -> f32 {
        if total > 0 {
            used as f32 / total as f32
        } else {
            0.0
        }
    };
    let snap = local.lock().unwrap().clone();

    // --- Bottom network graph: always the local machine --------------------
    win.set_net_bot_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
    win.set_net_bot_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
    win.set_net_bot_history(normalized_model(&local_net_hist.lock().unwrap()));

    let set_top_local = |win: &AppWindow| {
        win.set_net_top_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
        win.set_net_top_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
        win.set_net_top_history(normalized_model(&local_net_hist.lock().unwrap()));
        win.set_net_show_selector(false);
        win.set_net_selected("".into());
        win.set_net_ifaces(ModelRc::from(Rc::new(VecModel::<SharedString>::default())));
        // Non-connected tabs show the local machine's filesystems.
        win.set_disks(disk_model(&snap.disks));
    };
    let show_local_res = |win: &AppWindow| {
        win.set_resource_title(t("本机资源", "Local resources").into());
        win.set_cpu_percent(snap.cpu_percent);
        win.set_mem_percent(snap.mem_percent);
        win.set_swap_percent(snap.swap_percent);
        win.set_mem_detail(format_mem(snap.mem_used_mib, snap.mem_total_mib).into());
        win.set_swap_detail(format_mem(snap.swap_used_mib, snap.swap_total_mib).into());
    };
    let clear_stats = |win: &AppWindow| {
        win.set_cpu_percent(0.0);
        win.set_mem_percent(0.0);
        win.set_swap_percent(0.0);
        win.set_mem_detail("".into());
        win.set_swap_detail("".into());
    };

    // Process monitor (#23) lives in a shared model (the AppWindow and the
    // detachable ProcWindow point at the same VecModel), so mutate it in place
    // instead of replacing — replacing would break the sharing. Only a live
    // remote session has process data; default to empty and let the connected
    // branch below fill it in.
    let set_procs = |win: &AppWindow, procs: &[ProcInfo], current_user: &str, tab_id: &str| {
        if let Some(vm) = win
            .get_proc_list()
            .as_any()
            .downcast_ref::<VecModel<ProcRow>>()
        {
            vm.set_vec(proc_rows(procs, current_user, tab_id));
        }
    };
    let set_system_models =
        |win: &AppWindow,
         cpu: f32,
         mem: f32,
         swap: f32,
         mem_detail: SharedString,
         swap_detail: SharedString,
         nets: Vec<SysNetRow>,
         disks: Vec<DiskInfo>,
         sys: SystemDetails| {
            if let Some(vm) = win
                .get_sys_metrics()
                .as_any()
                .downcast_ref::<VecModel<SysMetricRow>>()
            {
                vm.set_vec(metric_rows(cpu, mem, swap, mem_detail, swap_detail));
            }
            if let Some(vm) = win
                .get_sys_net_rows()
                .as_any()
                .downcast_ref::<VecModel<SysNetRow>>()
            {
                vm.set_vec(nets);
            }
            if let Some(vm) = win
                .get_sys_disks()
                .as_any()
                .downcast_ref::<VecModel<DiskInfo>>()
            {
                vm.set_vec(disks);
            }
            if let Some(vm) = win
                .get_sys_overview_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_overview_rows(&sys.overview));
            }
            if let Some(vm) = win
                .get_sys_cpu_info_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_one_row(&sys.cpu_info));
            }
            if let Some(vm) = win
                .get_sys_gpu_info_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_rows(&sys.gpu_info, 4));
            }
            if let Some(vm) = win
                .get_sys_cpu_usage_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(cpu_usage_detail_rows(&sys.cpu_usage));
            }
            if let Some(vm) = win
                .get_sys_memory_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_one_row(&sys.memory));
            }
            if let Some(vm) = win
                .get_sys_swap_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(pairs_to_one_row(&sys.swap));
            }
            if let Some(vm) = win
                .get_sys_network_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(tuple5_rows(&sys.networks));
            }
            if let Some(vm) = win
                .get_sys_filesystem_rows()
                .as_any()
                .downcast_ref::<VecModel<SysInfoRow>>()
            {
                vm.set_vec(tuple5_rows(&sys.filesystems));
            }
        };
    win.set_proc_available(false);
    win.set_system_info_available(false);
    set_procs(win, &[], "", "");

    let active = win.get_active_tab_id().to_string();
    let status = if active == "welcome" {
        None
    } else {
        statuses.lock().unwrap().get(&active).cloned()
    };

    match status {
        // A live session tab → remote resources + remote NIC on top.
        Some(st) if st.state == 1 => {
            win.set_conn_state(1);
            win.set_connection_state(st.host.clone().into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            win.set_cpu_percent(st.cpu);
            win.set_mem_percent(pct(st.mem_used_kib, st.mem_total_kib));
            win.set_swap_percent(pct(st.swap_used_kib, st.swap_total_kib));
            win.set_mem_detail(format_mem(st.mem_used_kib / 1024, st.mem_total_kib / 1024).into());
            win.set_swap_detail(
                format_mem(st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
            );
            let (name, rx, tx) = selected_iface(&st);
            win.set_net_top_up(format_bytes_per_sec(tx).into());
            win.set_net_top_down(format_bytes_per_sec(rx).into());
            win.set_net_top_history(normalized_model(&st.net_hist));
            win.set_net_show_selector(!st.net.is_empty());
            win.set_net_selected(name.into());
            let ifaces: Vec<SharedString> = st.net.iter().map(|e| e.0.clone().into()).collect();
            win.set_net_ifaces(ModelRc::from(Rc::new(VecModel::from(ifaces))));
            win.set_disks(disk_model(&st.disks));
            win.set_proc_available(true);
            win.set_system_info_available(true);
            set_procs(win, &st.procs, &st.user, &active);
            set_system_models(
                win,
                st.cpu,
                pct(st.mem_used_kib, st.mem_total_kib),
                pct(st.swap_used_kib, st.swap_total_kib),
                format_mem(st.mem_used_kib / 1024, st.mem_total_kib / 1024).into(),
                format_mem(st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
                net_rows(&st.net),
                disk_rows(&st.disks),
                st.sys.clone(),
            );
        }
        // Disconnected / timed-out session.
        Some(st) if st.state == 2 => {
            win.set_conn_state(2);
            win.set_connection_state(format!("{} {}", st.host, t("已断开", "disconnected")).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win);
            set_system_models(
                win,
                0.0,
                0.0,
                0.0,
                "".into(),
                "".into(),
                Vec::new(),
                Vec::new(),
                SystemDetails::default(),
            );
        }
        // Still connecting.
        Some(st) => {
            win.set_conn_state(0);
            win.set_connection_state(format!("{} {}", t("连接中", "Connecting"), st.host).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win);
            set_system_models(
                win,
                0.0,
                0.0,
                0.0,
                "".into(),
                "".into(),
                Vec::new(),
                Vec::new(),
                SystemDetails::default(),
            );
        }
        // Welcome tab (or unknown) → local machine top + bottom.
        None => {
            win.set_conn_state(0);
            win.set_connection_state(t("未连接", "Not connected").into());
            win.set_conn_host("".into());
            show_local_res(win);
            set_top_local(win);
            set_system_models(
                win,
                snap.cpu_percent,
                snap.mem_percent,
                snap.swap_percent,
                format_mem(snap.mem_used_mib, snap.mem_total_mib).into(),
                format_mem(snap.swap_used_mib, snap.swap_total_mib).into(),
                vec![SysNetRow {
                    name: t("本机", "Local").into(),
                    up: format_bytes_per_sec(snap.net_tx_per_sec).into(),
                    down: format_bytes_per_sec(snap.net_rx_per_sec).into(),
                }],
                Vec::new(),
                SystemDetails::default(),
            );
        }
    }
}
