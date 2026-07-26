use std::rc::Rc;

use slint::{ModelRc, SharedString, VecModel};

use crate::config::ConfigStore;
use crate::i18n::t;
use crate::ui::*;

/// Every quick-command group name (used to start with all groups collapsed, #55):
/// "default" when any ungrouped command exists, plus explicit quick-groups and any
/// group referenced by a command.
pub(crate) fn all_quick_group_names(store: &ConfigStore) -> std::collections::HashSet<String> {
    let cmds = store.quick_commands();
    let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
    if cmds.iter().any(|c| c.group.trim().is_empty()) {
        set.insert("default".to_string());
    }
    for g in store.quick_groups() {
        set.insert(g.clone());
    }
    for c in cmds {
        let g = c.group.trim();
        if !g.is_empty() {
            set.insert(g.to_string());
        }
    }
    set
}

/// Build the quick-command model for the command bar + manage dialog (#55).
///
/// Grouped like the welcome session list: the implicit "default" group (entries
/// with an empty group) comes first, then named groups alphabetically. Within a
/// group, entries keep their saved order. `group_header` is set on the first row
/// of each group; `collapsed` reflects `collapsed_groups` (runtime-only state);
/// `orig_index` points back into the stored vec so deletes target the right entry
/// even though the display order differs.
pub(crate) fn quick_cmd_model(
    store: &ConfigStore,
    collapsed_groups: &std::collections::HashSet<String>,
) -> ModelRc<QuickCmd> {
    let cmds = store.quick_commands();

    let has_default = cmds.iter().any(|c| c.group.trim().is_empty());
    // Named groups = explicit quick-groups ∪ groups referenced by commands.
    let mut named: Vec<String> = store
        .quick_groups()
        .iter()
        .cloned()
        .chain(
            cmds.iter()
                .map(|c| c.group.trim().to_string())
                .filter(|g| !g.is_empty()),
        )
        .collect();
    named.sort_by_key(|g| g.to_lowercase());
    named.dedup();

    let mut groups: Vec<String> = Vec::new();
    if has_default {
        groups.push("default".to_string());
    }
    groups.extend(named);

    let mut rows: Vec<QuickCmd> = Vec::new();
    for group in &groups {
        let is_collapsed = collapsed_groups.contains(group);
        let members: Vec<(usize, &crate::config::QuickCommand)> = cmds
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let g = c.group.trim();
                if group == "default" {
                    g.is_empty()
                } else {
                    g == group
                }
            })
            .collect();
        if members.is_empty() {
            // Header-only placeholder for an empty group (orig_index -1) so it can
            // still be renamed / deleted, matching empty session folders.
            rows.push(QuickCmd {
                name: "".into(),
                command: "".into(),
                group: group.clone().into(),
                group_header: group.clone().into(),
                collapsed: is_collapsed,
                orig_index: -1,
                send_enter: true,
            });
        } else {
            for (i, (orig_idx, c)) in members.iter().enumerate() {
                rows.push(QuickCmd {
                    name: c.name.clone().into(),
                    command: c.command.clone().into(),
                    group: group.clone().into(),
                    group_header: if i == 0 {
                        group.clone().into()
                    } else {
                        "".into()
                    },
                    collapsed: is_collapsed,
                    orig_index: *orig_idx as i32,
                    send_enter: c.send_enter,
                });
            }
        }
    }
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(crate) fn blank_forward_draft() -> PortFwd {
    PortFwd {
        kind: "local".into(),
        name: "".into(),
        bind_addr: "127.0.0.1".into(),
        bind_port: "".into(),
        host: "".into(),
        host_port: "".into(),
    }
}

pub(crate) fn forward_drafts(forwards: &[crate::config::PortForward]) -> Vec<PortFwd> {
    forwards
        .iter()
        .map(|forward| PortFwd {
            kind: forward.kind.clone().into(),
            name: forward.name.clone().into(),
            bind_addr: if forward.bind_addr.trim().is_empty() {
                "127.0.0.1".into()
            } else {
                forward.bind_addr.trim().into()
            },
            bind_port: forward.bind_port.to_string().into(),
            host: forward.host.clone().into(),
            host_port: if forward.kind == "dynamic" {
                "".into()
            } else {
                forward.host_port.to_string().into()
            },
        })
        .collect()
}

pub(crate) fn forward_model(forwards: &[PortFwd]) -> ModelRc<PortFwd> {
    ModelRc::from(Rc::new(VecModel::from(forwards.to_vec())))
}

pub(crate) fn validated_port_forwards(
    drafts: &[PortFwd],
) -> std::result::Result<Vec<crate::config::PortForward>, String> {
    let mut forwards = Vec::new();
    for draft in drafts {
        let is_blank = draft.name.trim().is_empty()
            && draft.bind_port.trim().is_empty()
            && draft.host.trim().is_empty()
            && draft.host_port.trim().is_empty();
        if is_blank {
            continue;
        }

        let bind_port = draft
            .bind_port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| {
                t(
                    "请输入有效的监听端口（1-65535）",
                    "Enter a valid listen port (1-65535).",
                )
                .to_string()
            })?;
        let kind = draft.kind.as_str();
        let (host, host_port) = if kind == "dynamic" {
            (String::new(), 0)
        } else {
            let host = draft.host.trim();
            let host_port = draft
                .host_port
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0);
            if host.is_empty() || host_port.is_none() {
                return Err(t(
                    "请输入目标主机和有效的目标端口（1-65535）",
                    "Enter a target host and a valid target port (1-65535).",
                )
                .to_string());
            }
            (host.to_string(), host_port.unwrap())
        };

        forwards.push(crate::config::PortForward {
            kind: kind.to_string(),
            name: draft.name.trim().to_string(),
            bind_addr: if draft.bind_addr.trim().is_empty() {
                "127.0.0.1".to_string()
            } else {
                draft.bind_addr.trim().to_string()
            },
            bind_port,
            host,
            host_port,
        });
    }
    Ok(forwards)
}

#[cfg(test)]
mod port_forward_draft_tests {
    use super::{blank_forward_draft, validated_port_forwards};

    #[test]
    fn blank_rows_are_ignored_when_saving() {
        assert!(validated_port_forwards(&[blank_forward_draft()])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn filled_rows_are_saved_without_an_add_step() {
        let mut local = blank_forward_draft();
        local.bind_port = "8080".into();
        local.host = "service.internal".into();
        local.host_port = "80".into();

        let mut dynamic = blank_forward_draft();
        dynamic.kind = "dynamic".into();
        dynamic.bind_port = "1080".into();

        let forwards = validated_port_forwards(&[local, dynamic]).unwrap();
        assert_eq!(forwards.len(), 2);
        assert_eq!(forwards[0].bind_port, 8080);
        assert_eq!(forwards[0].host, "service.internal");
        assert_eq!(forwards[1].kind, "dynamic");
        assert_eq!(forwards[1].host_port, 0);
    }

    #[test]
    fn partially_filled_rows_block_saving() {
        let mut draft = blank_forward_draft();
        draft.bind_port = "8080".into();
        assert!(validated_port_forwards(&[draft]).is_err());
    }
}

/// Build the command-history model in storage order (oldest first, newest
/// last). The dropdown shows the most-recently-used command at the bottom
/// (nearest the input) and ↑ recalls it first (#55, #113).
pub(crate) fn history_model(store: &ConfigStore) -> ModelRc<SharedString> {
    let rows: Vec<SharedString> = store
        .command_history()
        .iter()
        .map(|s| s.clone().into())
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(crate) fn output_highlight_rule_model(store: &ConfigStore) -> ModelRc<OutputRuleItem> {
    let rows: Vec<OutputRuleItem> = store
        .output_highlight_rules()
        .iter()
        .map(|rule| OutputRuleItem {
            pattern: rule.pattern.clone().into(),
            regex: rule.regex,
            case_sensitive: rule.case_sensitive,
            whole_line: rule.whole_line,
            color: match rule.color.as_str() {
                "yellow" | "green" | "cyan" | "magenta" | "gray" => rule.color.clone(),
                _ => "red".to_string(),
            }
            .into(),
            enabled: rule.enabled,
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(crate) fn parse_hex_color(value: &str) -> Option<slint::Color> {
    let digits = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let green = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some(slint::Color::from_rgb_u8(red, green, blue))
}

pub(crate) fn validate_output_highlight_rule(
    pattern: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> std::result::Result<(), String> {
    if pattern.is_empty() {
        return Err(t("请输入关键词或正则表达式", "Enter a keyword or regular expression").into());
    }
    if pattern.chars().count() > 512 {
        return Err(t("规则不能超过 512 个字符", "Rules cannot exceed 512 characters").into());
    }
    if is_regex {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|error| format!("{}: {error}", t("无效的正则表达式", "Invalid regular expression")))?;
    }
    Ok(())
}

/// Build the filtered history-view model for the dropdown: case-insensitive
/// substring matches of `query`, in the same order as the full history (#101).
pub(crate) fn history_view_model(store: &ConfigStore, query: &str) -> ModelRc<SharedString> {
    let q = query.trim().to_lowercase();
    let rows: Vec<SharedString> = store
        .command_history()
        .iter()
        .filter(|c| q.is_empty() || c.to_lowercase().contains(&q))
        .map(|s| s.clone().into())
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}
