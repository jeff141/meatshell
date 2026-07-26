//! Centralized application context shared across UI callbacks.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use slint::VecModel;
use tokio::runtime::Runtime;

use crate::config::ConfigStore;
use crate::resource::{LocalSnap, NetHist, TabStatuses};
use crate::sftp::{SftpHandles, SftpLastCwd};
use crate::ssh::SessionHandle;
use crate::terminal::{RenderGates, TermBuffers};
use crate::ui::{
    AppWindow, PaneInfo, ProcWindow, SessionInfo, SplitterInfo, SystemInfoWindow, TabInfo,
    TerminalState, TransferInfo,
};

/// All shared application state needed by the UI callback wiring functions.
#[allow(dead_code)]
pub struct AppContext {
    pub store: Rc<RefCell<ConfigStore>>,
    pub runtime: Arc<Runtime>,
    pub handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    pub sftp_handles: SftpHandles,
    pub sftp_last_cwd: SftpLastCwd,
    pub bufs: TermBuffers,
    pub render_gates: RenderGates,
    pub last_term_size: Arc<Mutex<(u32, u32)>>,
    pub main_window: slint::Weak<AppWindow>,
    pub proc_window: slint::Weak<ProcWindow>,
    pub sys_window: slint::Weak<SystemInfoWindow>,
    pub sessions_model: Rc<VecModel<SessionInfo>>,
    pub tabs_model: Rc<VecModel<TabInfo>>,
    pub terminals_model: Rc<VecModel<TerminalState>>,
    pub layout: Rc<RefCell<crate::layout::Layout>>,
    pub content_size: Rc<Cell<(f32, f32)>>,
    pub panes_model: Rc<VecModel<PaneInfo>>,
    pub splitters_model: Rc<VecModel<SplitterInfo>>,
    pub transfers_model: Rc<VecModel<TransferInfo>>,
    pub tab_statuses: TabStatuses,
    pub local_snap: LocalSnap,
    pub local_net_hist: NetHist,
    pub sftp_follow_cd: Arc<AtomicBool>,
}
