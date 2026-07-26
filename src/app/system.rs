use std::rc::Rc;
use std::sync::OnceLock;

use i_slint_backend_winit::WinitWindowAccessor;
use slint::{ModelRc, SharedString, VecModel, Weak};

use crate::i18n::t;
use crate::resource::system::{format_bytes_per_sec, SystemSnapshot};
use crate::resource::{LocalGpuInfo, LocalHardwareInfo};
use crate::ssh::{format_size, ProcInfo, SystemDetails};
use crate::ui::*;

/// Push a value into a fixed-length ring buffer (newest at the end).
pub(crate) fn push_ring(buf: &mut Vec<f32>, val: f32) {
    if buf.len() != super::NET_HISTORY_LEN {
        *buf = vec![0.0; super::NET_HISTORY_LEN];
    }
    buf.remove(0);
    buf.push(val);
}

/// Auto-scale a raw bytes/sec history to 0..1 against its own window peak so the
/// sparkline always uses the full height (like FinalShell's relative graph).
pub(crate) fn normalized_model(buf: &[f32]) -> ModelRc<f32> {
    let max = buf.iter().cloned().fold(1.0_f32, f32::max);
    let scaled: Vec<f32> = buf.iter().map(|v| (v / max).clamp(0.0, 1.0)).collect();
    ModelRc::from(Rc::new(VecModel::from(scaled)))
}

/// Build the filesystem-usage model (path, "avail/total", used fraction).
pub(crate) fn disk_rows(disks: &[(String, u64, u64)]) -> Vec<DiskInfo> {
    disks
        .iter()
        .map(|(mount, avail, total)| {
            let used = total.saturating_sub(*avail);
            let percent = if *total > 0 {
                used as f32 / *total as f32
            } else {
                0.0
            };
            DiskInfo {
                path: mount.clone().into(),
                detail: format!("{}/{}", format_size(*avail), format_size(*total)).into(),
                percent,
            }
        })
        .collect()
}

pub(crate) fn disk_model(disks: &[(String, u64, u64)]) -> ModelRc<DiskInfo> {
    ModelRc::from(Rc::new(VecModel::from(disk_rows(disks))))
}

/// Build the process-monitor model for the popup (#23). `cpu`/`mem` are
/// pre-formatted to one decimal; `cpu_frac` (0..1) drives the row's load bar.
pub(crate) fn set_process_action_error(weak: &Weak<ProcWindow>, message: &str) {
    if let Some(window) = weak.upgrade() {
        window.set_action_busy(false);
        window.set_action_error(true);
        window.set_action_status(message.into());
    }
}

/// A root login can signal any process directly. Non-root logins may signal
/// only their own processes; root and other users' processes require `su`.
pub(crate) fn process_needs_root(current_user: &str, process_user: &str) -> bool {
    current_user != "root" && process_user != current_user
}

pub(crate) fn proc_rows(procs: &[ProcInfo], current_user: &str, tab_id: &str) -> Vec<ProcRow> {
    procs
        .iter()
        .map(|p| ProcRow {
            tab_id: tab_id.into(),
            pid: p.pid.to_string().into(),
            user: p.user.clone().into(),
            cpu: format!("{:.1}", p.cpu).into(),
            mem: format!("{:.1}", p.mem).into(),
            command: p.command.clone().into(),
            cpu_frac: (p.cpu / 100.0).clamp(0.0, 1.0),
            own_process: !process_needs_root(current_user, &p.user),
        })
        .collect()
}

#[cfg(test)]
mod process_row_tests {
    use super::*;

    #[test]
    fn marks_owner_and_preserves_source_tab() {
        let input = vec![
            ProcInfo { pid: 10, user: "alice".into(), cpu: 1.0, mem: 2.0, command: "own".into() },
            ProcInfo { pid: 11, user: "root".into(), cpu: 3.0, mem: 4.0, command: "other".into() },
        ];
        let rows = proc_rows(&input, "alice", "term-a");
        assert!(rows[0].own_process);
        assert!(!rows[1].own_process);
        assert!(rows.iter().all(|row| row.tab_id.as_str() == "term-a"));
    }

    #[test]
    fn privilege_rules_match_effective_login_user() {
        assert!(!process_needs_root("alice", "alice"));
        assert!(process_needs_root("alice", "root"));
        assert!(process_needs_root("alice", "bob"));
        assert!(!process_needs_root("root", "root"));
        assert!(!process_needs_root("root", "alice"));
    }
}

pub(crate) fn metric_rows(
    cpu: f32,
    mem: f32,
    swap: f32,
    mem_detail: impl Into<SharedString>,
    swap_detail: impl Into<SharedString>,
) -> Vec<SysMetricRow> {
    vec![
        SysMetricRow {
            label: "CPU".into(),
            percent: cpu,
            detail: "".into(),
            kind: 0,
        },
        SysMetricRow {
            label: t("内存", "Memory").into(),
            percent: mem,
            detail: mem_detail.into(),
            kind: 1,
        },
        SysMetricRow {
            label: t("交换", "Swap").into(),
            percent: swap,
            detail: swap_detail.into(),
            kind: 2,
        },
    ]
}

pub(crate) fn net_rows(net: &[(String, u64, u64)]) -> Vec<SysNetRow> {
    net.iter()
        .map(|(name, rx, tx)| SysNetRow {
            name: name.clone().into(),
            up: format_bytes_per_sec(*tx).into(),
            down: format_bytes_per_sec(*rx).into(),
        })
        .collect()
}

pub(crate) fn pairs_to_overview_rows(pairs: &[(String, String)]) -> Vec<SysInfoRow> {
    pairs
        .chunks(2)
        .map(|chunk| {
            let first = &chunk[0];
            let second = chunk.get(1);
            SysInfoRow {
                c1: first.0.clone().into(),
                c2: first.1.clone().into(),
                c3: second.map(|p| p.0.clone()).unwrap_or_default().into(),
                c4: second.map(|p| p.1.clone()).unwrap_or_default().into(),
                c5: "".into(),
            }
        })
        .collect()
}

pub(crate) fn pairs_to_one_row(pairs: &[(String, String)]) -> Vec<SysInfoRow> {
    let value = |idx: usize| {
        pairs
            .get(idx)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "-".to_string())
    };
    vec![SysInfoRow {
        c1: value(0).into(),
        c2: value(1).into(),
        c3: value(2).into(),
        c4: value(3).into(),
        c5: value(4).into(),
    }]
}

pub(crate) fn pairs_to_rows(pairs: &[(String, String)], width: usize) -> Vec<SysInfoRow> {
    pairs
        .chunks(width)
        .filter(|chunk| chunk.iter().any(|(_, v)| !v.trim().is_empty() && v.trim() != "-"))
        .map(|chunk| {
            let value = |idx: usize| {
                chunk
                    .get(idx)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| "-".to_string())
            };
            SysInfoRow {
                c1: value(0).into(),
                c2: value(1).into(),
                c3: value(2).into(),
                c4: value(3).into(),
                c5: value(4).into(),
            }
        })
        .collect()
}

pub(crate) fn cpu_usage_detail_rows(pairs: &[(String, String)]) -> Vec<SysInfoRow> {
    let value = |idx: usize| {
        pairs
            .get(idx)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "0.0%".to_string())
    };
    let extra = pairs
        .iter()
        .skip(4)
        .map(|(k, v)| format!("{k} {v}"))
        .collect::<Vec<_>>()
        .join(" / ");
    vec![SysInfoRow {
        c1: value(0).into(),
        c2: value(2).into(),
        c3: value(1).into(),
        c4: value(3).into(),
        c5: extra.into(),
    }]
}

pub(crate) fn nonempty_or_dash(value: impl Into<String>) -> String {
    let value = value.into();
    if value.trim().is_empty() {
        "-".to_string()
    } else {
        value
    }
}

pub(crate) fn local_hardware_info() -> &'static LocalHardwareInfo {
    static INFO: OnceLock<LocalHardwareInfo> = OnceLock::new();
    INFO.get_or_init(|| {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_all();
        let first_cpu = sys.cpus().first();
        let mut info = LocalHardwareInfo {
            os: sysinfo::System::long_os_version()
                .or_else(sysinfo::System::name)
                .unwrap_or_else(|| std::env::consts::OS.to_string()),
            kernel: sysinfo::System::name().unwrap_or_else(|| std::env::consts::FAMILY.to_string()),
            kernel_version: sysinfo::System::kernel_version().unwrap_or_default(),
            arch: std::env::consts::ARCH.to_string(),
            hostname: sysinfo::System::host_name().unwrap_or_default(),
            cpu_name: first_cpu
                .map(|cpu| cpu.brand().to_string())
                .unwrap_or_default(),
            cpu_vendor: first_cpu
                .map(|cpu| cpu.vendor_id().to_string())
                .unwrap_or_default(),
            cpu_cores: sys.cpus().len().to_string(),
            cpu_frequency: first_cpu
                .map(|cpu| {
                    let mhz = cpu.frequency();
                    if mhz == 0 {
                        String::new()
                    } else if mhz >= 1000 {
                        format!("{:.2} GHz", mhz as f64 / 1000.0)
                    } else {
                        format!("{mhz} MHz")
                    }
                })
                .unwrap_or_default(),
            ..Default::default()
        };
        fill_local_gpu_info(&mut info);
        info
    })
}

#[cfg(target_os = "windows")]
pub(crate) fn fill_local_gpu_info(info: &mut LocalHardwareInfo) {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "$controllers = @(Get-CimInstance Win32_VideoController | Select-Object Name,AdapterCompatibility,DriverVersion,AdapterRAM); $regs = @(Get-ChildItem 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}' -ErrorAction SilentlyContinue | ForEach-Object { $p = Get-ItemProperty $_.PsPath -ErrorAction SilentlyContinue; if ($p.DriverDesc) { [pscustomobject]@{ Name=$p.DriverDesc; Vendor=$p.ProviderName; Driver=$p.DriverVersion; Memory=$p.'HardwareInformation.qwMemorySize' } } }); [pscustomobject]@{ Controllers=$controllers; Registry=$regs } | ConvertTo-Json -Compress -Depth 4",
        ])
        .output();
    let Ok(output) = output else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return;
    };
    let registry_values = value
        .get("Registry")
        .map(json_values)
        .unwrap_or_default();
    let controller_values = value
        .get("Controllers")
        .map(json_values)
        .unwrap_or_else(|| json_values(&value));
    let registry_gpus: Vec<LocalGpuInfo> = registry_values
        .iter()
        .filter_map(gpu_from_registry_json)
        .collect();
    info.gpus = controller_values
        .iter()
        .filter_map(|gpu| {
            let get_str = |key: &str| {
                gpu.get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            let name = get_str("Name");
            if name.is_empty() {
                return None;
            }
            let matched = registry_gpus
                .iter()
                .find(|item| item.name.eq_ignore_ascii_case(&name))
                .or_else(|| {
                    registry_gpus
                        .iter()
                        .find(|item| !item.name.is_empty() && name.contains(&item.name))
                });
            Some(LocalGpuInfo {
                name,
                vendor: nonempty_prefer(
                    matched.map(|item| item.vendor.as_str()).unwrap_or_default(),
                    &get_str("AdapterCompatibility"),
                ),
                driver: nonempty_prefer(
                    matched.map(|item| item.driver.as_str()).unwrap_or_default(),
                    &get_str("DriverVersion"),
                ),
                memory: nonempty_prefer(
                    matched.map(|item| item.memory.as_str()).unwrap_or_default(),
                    &gpu
                        .get("AdapterRAM")
                        .and_then(|v| v.as_u64())
                        .filter(|bytes| *bytes > 0)
                        .map(format_size)
                        .unwrap_or_default(),
                ),
            })
        })
        .collect();
}

#[cfg(target_os = "windows")]
pub(crate) fn json_values(value: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(items) = value.as_array() {
        items.clone()
    } else if value.is_null() {
        Vec::new()
    } else {
        vec![value.clone()]
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn nonempty_prefer(primary: &str, fallback: &str) -> String {
    if primary.trim().is_empty() {
        fallback.trim().to_string()
    } else {
        primary.trim().to_string()
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn gpu_from_registry_json(gpu: &serde_json::Value) -> Option<LocalGpuInfo> {
    let get_str = |key: &str| {
        gpu.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let name = get_str("Name");
    if name.is_empty() {
        return None;
    }
    Some(LocalGpuInfo {
        name,
        vendor: get_str("Vendor"),
        driver: get_str("Driver"),
        memory: gpu
            .get("Memory")
            .and_then(|v| {
                v.as_u64().or_else(|| {
                    v.as_array().and_then(|bytes| {
                        let mut raw = [0u8; 8];
                        let mut any = false;
                        for (idx, b) in bytes.iter().take(8).enumerate() {
                            if let Some(n) = b.as_u64() {
                                raw[idx] = n as u8;
                                any = true;
                            }
                        }
                        any.then(|| u64::from_le_bytes(raw))
                    })
                })
            })
            .filter(|bytes| *bytes > 0)
            .map(format_size)
            .unwrap_or_default(),
    })
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn fill_local_gpu_info(_info: &mut LocalHardwareInfo) {}

pub(crate) fn local_system_details(snap: &SystemSnapshot) -> SystemDetails {
    let mem_used = snap.mem_used_mib.saturating_mul(1024 * 1024);
    let mem_total = snap.mem_total_mib.saturating_mul(1024 * 1024);
    let swap_used = snap.swap_used_mib.saturating_mul(1024 * 1024);
    let swap_total = snap.swap_total_mib.saturating_mul(1024 * 1024);
    let info = local_hardware_info();
    SystemDetails {
        overview: vec![
            (t("操作系统", "Operating system").to_string(), nonempty_or_dash(&info.os)),
            (
                t("内核版本", "Kernel version").to_string(),
                nonempty_or_dash(&info.kernel_version),
            ),
            (t("主机名称", "Hostname").to_string(), nonempty_or_dash(&info.hostname)),
            (t("内核", "Kernel").to_string(), nonempty_or_dash(&info.kernel)),
            (t("硬件架构", "Architecture").to_string(), nonempty_or_dash(&info.arch)),
            (t("连接", "Connection").to_string(), t("本机", "Local").to_string()),
        ],
        cpu_info: vec![
            (t("名称", "Name").to_string(), nonempty_or_dash(&info.cpu_name)),
            (t("核心数", "Cores").to_string(), nonempty_or_dash(&info.cpu_cores)),
            (t("频率", "Frequency").to_string(), nonempty_or_dash(&info.cpu_frequency)),
            (t("缓存", "Cache").to_string(), "-".to_string()),
            ("BogoMips".to_string(), nonempty_or_dash(&info.cpu_vendor)),
        ],
        gpu_info: info
            .gpus
            .iter()
            .flat_map(|gpu| {
                [
                    (t("名称", "Name").to_string(), nonempty_or_dash(&gpu.name)),
                    (t("厂商", "Vendor").to_string(), nonempty_or_dash(&gpu.vendor)),
                    (t("驱动", "Driver").to_string(), nonempty_or_dash(&gpu.driver)),
                    (t("内存", "Memory").to_string(), nonempty_or_dash(&gpu.memory)),
                ]
            })
            .collect(),
        cpu_usage: vec![
            (t("用户", "User").to_string(), format!("{:.1}%", snap.cpu_percent * 100.0)),
            ("Nice".to_string(), "-".to_string()),
            (t("系统", "System").to_string(), "-".to_string()),
            (t("空闲", "Idle").to_string(), "-".to_string()),
        ],
        memory: vec![
            (t("总计", "Total").to_string(), format_size(mem_total)),
            (t("已使用", "Used").to_string(), format_size(mem_used)),
            (
                t("剩余", "Free").to_string(),
                format_size(mem_total.saturating_sub(mem_used)),
            ),
            (t("已用", "Usage").to_string(), format!("{:.1}%", snap.mem_percent * 100.0)),
            (t("缓存", "Cached").to_string(), "-".to_string()),
        ],
        swap: vec![
            (t("总计", "Total").to_string(), format_size(swap_total)),
            (t("已使用", "Used").to_string(), format_size(swap_used)),
            (
                t("剩余", "Free").to_string(),
                format_size(swap_total.saturating_sub(swap_used)),
            ),
            (t("已用", "Usage").to_string(), format!("{:.1}%", snap.swap_percent * 100.0)),
        ],
        networks: vec![(
            t("本机", "Local").to_string(),
            "-".to_string(),
            "-".to_string(),
            format_bytes_per_sec(snap.net_tx_per_sec),
            format_bytes_per_sec(snap.net_rx_per_sec),
        )],
        filesystems: snap
            .disks
            .iter()
            .map(|(mount, avail, total)| {
                let used = total.saturating_sub(*avail);
                let pct = if *total == 0 {
                    "-".to_string()
                } else {
                    format!("{:.1}%", used as f64 * 100.0 / *total as f64)
                };
                (
                    mount.clone(),
                    format_size(*total),
                    pct,
                    format_size(*avail),
                    mount.clone(),
                )
            })
            .collect(),
    }
}

/// Mirror the main window's theme/scale/UI-font onto the detached process
/// window. Theme is a per-window Slint global, so a detached window keeps its
/// compile-time (dark) defaults until we copy these across (#23).
pub(crate) fn sync_proc_theme(main: &AppWindow, proc: &ProcWindow) {
    proc.set_dark_mode(main.get_dark_mode());
    proc.set_ui_scale(main.get_ui_scale());
    proc.set_ui_font_family(main.get_ui_font_family());
    // Mirror the immersive wallpaper so the detached window shares the frosted
    // backdrop instead of a flat panel.
    proc.set_wallpaper_img(main.get_wallpaper_img());
    proc.set_wallpaper_active(main.get_wallpaper_active());
    proc.set_wp_accent(main.get_wp_accent());
    proc.set_wp_tint(main.get_wp_tint());
}

pub(crate) fn sync_system_info_theme(main: &AppWindow, sys: &SystemInfoWindow) {
    sys.set_dark_mode(main.get_dark_mode());
    sys.set_ui_scale(main.get_ui_scale());
    sys.set_ui_font_family(main.get_ui_font_family());
    sys.set_wallpaper_img(main.get_wallpaper_img());
    sys.set_wallpaper_active(main.get_wallpaper_active());
    sys.set_wp_accent(main.get_wp_accent());
    sys.set_wp_tint(main.get_wp_tint());
}

pub(crate) fn place_system_info_window(main: &AppWindow, sys: &SystemInfoWindow) {
    use i_slint_backend_winit::winit::dpi::{LogicalPosition, LogicalSize};

    let Some((mon_x, mon_y, mon_w, mon_h, scale)) = main
        .window()
        .with_winit_window(|ww| {
            let scale = ww.scale_factor().max(0.01);
            let monitor = ww.current_monitor().or_else(|| ww.primary_monitor())?;
            let pos = monitor.position();
            let size = monitor.size();
            Some((
                pos.x as f64 / scale,
                pos.y as f64 / scale,
                size.width as f64 / scale,
                size.height as f64 / scale,
                scale,
            ))
        })
        .flatten()
    else {
        return;
    };

    let target_w = (mon_w * 0.5).clamp(760.0, (mon_w - 24.0).max(760.0));
    let target_h = (mon_h * 0.5).clamp(520.0, (mon_h - 24.0).max(520.0));
    let x = mon_x + (mon_w - target_w).max(0.0) / 2.0;
    let y = mon_y + (mon_h - target_h).max(0.0) / 2.0;

    sys.window().with_winit_window(|ww| {
        let _ = ww.request_inner_size(LogicalSize::new(target_w, target_h));
        ww.set_outer_position(LogicalPosition::new(x, y));
        let _ = scale; // documents that all values above are already logical.
    });
}

/// Center the process monitor on the same physical monitor as the main window.
/// Physical coordinates avoid logical/physical rounding errors when the two
/// displays use different DPI scale factors. Keep the user's current process
/// window size; opening it should reposition, not reset a manual resize.
pub(crate) fn place_process_window(main: &AppWindow, process: &ProcWindow) {
    use i_slint_backend_winit::winit::dpi::PhysicalPosition;

    let monitor = main
        .window()
        .with_winit_window(|ww| ww.current_monitor().or_else(|| ww.primary_monitor()))
        .flatten();
    let Some(monitor) = monitor else { return };
    let origin = monitor.position();
    let monitor_size = monitor.size();

    process.window().with_winit_window(|ww| {
        let window_size = ww.outer_size();
        let x = origin.x + monitor_size.width.saturating_sub(window_size.width) as i32 / 2;
        let y = origin.y + monitor_size.height.saturating_sub(window_size.height) as i32 / 2;
        ww.set_outer_position(PhysicalPosition::new(x, y));
    });
}
