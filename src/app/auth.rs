//! Authentication prompt helpers (host-key, credential, MFA).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use crate::session::{PendingCred, PendingHostKey, PendingMfa};
use crate::ui::AppWindow;

// ---------------------------------------------------------------------------
// Host-key confirmation (#109-5)
// ---------------------------------------------------------------------------

thread_local! {
    /// Prompts awaiting a decision; the front one is shown. Lives on the Slint
    /// event-loop thread (all access is from there).
    static HOSTKEY_QUEUE: RefCell<VecDeque<PendingHostKey>> = RefCell::new(VecDeque::new());
    /// host:port → decision, remembered for this run so a duplicate prompt
    /// (second connection to the same host) is answered without a new dialog.
    static HOSTKEY_DECIDED: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
}

/// Localized title / message / detail / confirm-label for the host-key dialog.
pub(crate) fn hostkey_dialog_text(
    host: &str,
    port: u16,
    key_type: &str,
    fingerprint: &str,
    changed: bool,
) -> (String, String, String, String) {
    let detail = format!("{host}:{port}  ({key_type})\n{fingerprint}");
    if changed {
        (
            crate::i18n::t("⚠ 主机密钥已改变", "⚠ Host key changed").to_string(),
            crate::i18n::t(
                "该主机的密钥与之前记录的不一致,可能存在中间人攻击。仅当你确知服务器密钥已更换时才继续。",
                "This host's key differs from the one stored earlier — this could be a man-in-the-middle attack. Only continue if you know the server's key really changed.",
            )
            .to_string(),
            detail,
            crate::i18n::t("仍然信任", "Trust anyway").to_string(),
        )
    } else {
        (
            crate::i18n::t("未知主机", "Unknown host").to_string(),
            crate::i18n::t(
                "首次连接该主机。请核对下面的密钥指纹,确认无误后再信任并连接。",
                "First time connecting to this host. Verify the key fingerprint below before you trust and connect.",
            )
            .to_string(),
            detail,
            crate::i18n::t("信任并连接", "Trust & connect").to_string(),
        )
    }
}

/// Queue a host-key prompt: answer immediately if already decided this run,
/// merge into an existing pending entry for the same host, otherwise enqueue
/// (and show it now if nothing else is up).
pub(crate) fn enqueue_hostkey_prompt(
    win: &AppWindow,
    host: String,
    port: u16,
    key_type: String,
    fingerprint: String,
    changed: bool,
    responder: crate::ssh::HostKeyResponder,
) {
    let id = format!("{host}:{port}");
    if let Some(ans) = HOSTKEY_DECIDED.with(|d| d.borrow().get(&id).copied()) {
        responder.respond(ans);
        return;
    }
    let show_now = HOSTKEY_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.iter_mut().find(|p| p.host == host && p.port == port) {
            p.responders.push(responder);
            return false;
        }
        let was_empty = q.is_empty();
        let (title, message, detail, confirm_label) =
            hostkey_dialog_text(&host, port, &key_type, &fingerprint, changed);
        q.push_back(PendingHostKey {
            host,
            port,
            changed,
            title,
            message,
            detail,
            confirm_label,
            responders: vec![responder],
        });
        was_empty
    });
    if show_now {
        show_front_hostkey(win);
    }
}

/// Push the front pending prompt's details into the window and open the dialog.
pub(crate) fn show_front_hostkey(win: &AppWindow) {
    HOSTKEY_QUEUE.with(|q| {
        if let Some(p) = q.borrow().front() {
            win.set_hostkey_changed(p.changed);
            win.set_hostkey_title(p.title.clone().into());
            win.set_hostkey_message(p.message.clone().into());
            win.set_hostkey_detail(p.detail.clone().into());
            win.set_hostkey_confirm_label(p.confirm_label.clone().into());
            win.set_hostkey_prompt_open(true);
        }
    });
}

/// Apply the user's decision to the front prompt, then show the next one (or
/// close the dialog if the queue is now empty).
pub(crate) fn resolve_front_hostkey(win: &AppWindow, accept: bool) {
    let has_next = HOSTKEY_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.pop_front() {
            // Only remember an *accept* for this run (so a slightly-later SFTP
            // prompt for the same host is answered without a second dialog). We
            // must NOT cache a reject: a single dismissal — e.g. an accidental
            // backdrop click instead of "Trust & connect" — used to poison the
            // host for the whole session, auto-rejecting every later connect with
            // "Unknown server key" until the app was restarted (#152). A reject now
            // only fails the current attempt; the next connect prompts again.
            if accept {
                HOSTKEY_DECIDED.with(|d| {
                    d.borrow_mut()
                        .insert(format!("{}:{}", p.host, p.port), true);
                });
            }
            for r in &p.responders {
                r.respond(accept);
            }
        }
        !q.is_empty()
    });
    if has_next {
        show_front_hostkey(win);
    } else {
        win.set_hostkey_prompt_open(false);
    }
}

// ---------------------------------------------------------------------------
// Connect-time credential prompt (#110)
// ---------------------------------------------------------------------------

thread_local! {
    static CRED_QUEUE: RefCell<VecDeque<PendingCred>> = RefCell::new(VecDeque::new());
    /// session id → the answer given this run (`None` = cancelled), so a second
    /// connection for the same session is answered without re-prompting.
    static CRED_DECIDED: RefCell<HashMap<String, Option<crate::ssh::CredentialReply>>> =
        RefCell::new(HashMap::new());
}

/// Queue a credential prompt: answer immediately if already decided this run,
/// merge into an existing pending entry for the same session, otherwise enqueue
/// (and show it now if nothing else is up).
pub(crate) fn enqueue_cred_prompt(
    win: &AppWindow,
    session_id: String,
    host: String,
    user: String,
    need_user: bool,
    need_password: bool,
    responder: crate::ssh::CredentialResponder,
) {
    if let Some(reply) = CRED_DECIDED.with(|d| d.borrow().get(&session_id).cloned()) {
        responder.respond(reply);
        return;
    }
    let show_now = CRED_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.iter_mut().find(|p| p.session_id == session_id) {
            p.responders.push(responder);
            return false;
        }
        let was_empty = q.is_empty();
        q.push_back(PendingCred {
            session_id,
            host,
            user,
            need_user,
            need_password,
            responders: vec![responder],
        });
        was_empty
    });
    if show_now {
        show_front_cred(win);
    }
}

/// Populate the credential dialog from the front prompt and open it.
pub(crate) fn show_front_cred(win: &AppWindow) {
    CRED_QUEUE.with(|q| {
        if let Some(p) = q.borrow().front() {
            win.set_cred_host(p.host.clone().into());
            win.set_cred_need_user(p.need_user);
            win.set_cred_need_password(p.need_password);
            win.set_cred_user(p.user.clone().into());
            win.set_cred_password("".into());
            win.set_cred_remember(false);
            win.set_cred_prompt_open(true);
        }
    });
}

/// Apply the user's answer to the front credential prompt (or cancel), persist
/// it when "remember" is checked, then show the next prompt or close.
pub(crate) fn resolve_front_cred(win: &AppWindow, accept: bool) {
    let reply: Option<crate::ssh::CredentialReply> = if accept {
        Some((
            win.get_cred_user().to_string(),
            win.get_cred_password().to_string(),
            win.get_cred_remember(),
        ))
    } else {
        None
    };
    let has_next = CRED_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.pop_front() {
            CRED_DECIDED.with(|d| {
                d.borrow_mut().insert(p.session_id.clone(), reply.clone());
            });
            if let Some((ref u, ref pw, true)) = reply {
                persist_credentials(&p.session_id, u, pw, p.need_user, p.need_password);
            }
            for r in &p.responders {
                r.respond(reply.clone());
            }
        }
        !q.is_empty()
    });
    // Don't leave the typed password lingering in the UI property.
    win.set_cred_password("".into());
    if has_next {
        show_front_cred(win);
    } else {
        win.set_cred_prompt_open(false);
    }
}

/// Persist newly-entered credentials onto the saved session (#110, "remember").
pub(crate) fn persist_credentials(
    session_id: &str,
    user: &str,
    password: &str,
    set_user: bool,
    set_password: bool,
) {
    crate::app::HISTORY_STORE.with(|s| {
        if let Some(store) = s.borrow().as_ref() {
            let mut st = store.borrow_mut();
            if let Some(mut sess) = st.get(session_id).cloned() {
                if set_user && !user.trim().is_empty() {
                    sess.user = user.trim().to_string();
                }
                if set_password {
                    sess.password = crate::config::Secret::new(password.to_string());
                }
                st.upsert(sess);
                let _ = st.save();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// MFA / keyboard-interactive prompt (#86-MFA)
// ---------------------------------------------------------------------------

thread_local! {
    static MFA_QUEUE: RefCell<VecDeque<PendingMfa>> = RefCell::new(VecDeque::new());
}

/// Queue an MFA prompt: a concurrent connection for the same session (the shell
/// and its SFTP channel both hitting the prompt at once) merges into the open
/// dialog so the code is only typed once; otherwise enqueue (and show it now if
/// nothing else is up). We deliberately do NOT cache answers across attempts —
/// a wrong code must re-prompt on reconnect, not be silently replayed.
pub(crate) fn enqueue_mfa_prompt(
    win: &AppWindow,
    session_id: String,
    host: String,
    prompt: String,
    echo: bool,
    responder: crate::ssh::MfaResponder,
) {
    let show_now = MFA_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.iter_mut().find(|p| p.session_id == session_id) {
            p.responders.push(responder);
            return false;
        }
        let was_empty = q.is_empty();
        q.push_back(PendingMfa {
            session_id,
            host,
            prompt,
            echo,
            responders: vec![responder],
        });
        was_empty
    });
    if show_now {
        show_front_mfa(win);
    }
}

/// Populate the MFA dialog from the front prompt and open it.
pub(crate) fn show_front_mfa(win: &AppWindow) {
    MFA_QUEUE.with(|q| {
        if let Some(p) = q.borrow().front() {
            win.set_mfa_host(p.host.clone().into());
            win.set_mfa_prompt(p.prompt.clone().into());
            win.set_mfa_echo(p.echo);
            win.set_mfa_answer("".into());
            win.set_mfa_prompt_open(true);
        }
    });
}

/// Apply the user's answer to the front MFA prompt (or cancel), then show the
/// next prompt or close.
pub(crate) fn resolve_front_mfa(win: &AppWindow, accept: bool) {
    let answer: Option<String> = if accept {
        Some(win.get_mfa_answer().to_string())
    } else {
        None
    };
    let has_next = MFA_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(p) = q.pop_front() {
            for r in &p.responders {
                r.respond(answer.clone());
            }
        }
        !q.is_empty()
    });
    // Don't leave the typed code lingering in the UI property.
    win.set_mfa_answer("".into());
    if has_next {
        show_front_mfa(win);
    } else {
        win.set_mfa_prompt_open(false);
    }
}
