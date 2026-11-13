//! Internal app events delivered via channel.
//!
//! Background tasks (PTY child watchers, future hook listeners, etc.) send
//! events to the main loop through this channel. No polling needed.

use std::time::Instant;

use crate::detect::{Agent, AgentState};
use crate::layout::PaneId;
use crate::workspace::{GitStatusCacheEntry, WorkspaceGitStatus};

#[derive(Debug)]
pub struct ApiWorktreeAddRequest {
    pub id: String,
    pub operation_id: u64,
    pub checkout_key: std::path::PathBuf,
    pub source_workspace_id: Option<String>,
    pub source_existing_membership: Option<crate::workspace::WorktreeSpaceMembership>,
    pub source_checkout_path: std::path::PathBuf,
    pub source_repo_root: std::path::PathBuf,
    pub repo_key: String,
    pub repo_name: String,
    pub label: Option<String>,
    pub focus: bool,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

#[derive(Debug)]
pub struct WorktreeAddResult {
    pub path: std::path::PathBuf,
    pub api_request: Option<ApiWorktreeAddRequest>,
    pub result: Result<(), String>,
}

#[derive(Debug)]
pub struct ApiWorktreeRemoveRequest {
    pub id: String,
    pub operation_id: u64,
    pub checkout_key: std::path::PathBuf,
    pub shutdown_panes: Vec<crate::layout::PaneId>,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

#[derive(Debug)]
pub struct WorktreeRemoveResult {
    pub workspace_id: String,
    pub path: std::path::PathBuf,
    pub workspace: Option<Box<crate::api::schema::WorkspaceInfo>>,
    pub worktree: Option<Box<crate::api::schema::WorktreeInfo>>,
    pub forced: bool,
    pub api_request: Option<ApiWorktreeRemoveRequest>,
    pub result: Result<(), String>,
}

#[derive(Debug)]
pub struct WorktreeReadResult {
    // Keep the slot until completion is consumed, including time queued on the app loop.
    pub(crate) _permit: tokio::sync::OwnedSemaphorePermit,
    pub(crate) client_local: bool,
    pub(crate) request: crate::api::schema::Request,
    pub(crate) source_workspace_id: Option<String>,
    pub(crate) source_cwd: Option<std::path::PathBuf>,
    pub(crate) result: Result<WorktreeReadData, (String, String)>,
    pub(crate) respond_to: std::sync::mpsc::Sender<String>,
}

#[derive(Debug)]
pub(crate) struct WorktreeReadData {
    pub source_checkout_path: std::path::PathBuf,
    pub source_repo_root: std::path::PathBuf,
    pub repo_key: String,
    pub repo_name: String,
    pub entries: Vec<crate::worktree::ExistingWorktree>,
}

/// An event from a background task to the main loop.
#[derive(Debug)]
pub enum AppEvent {
    HcSourceChanged {
        generation: u64,
    },
    HcRecoveryReady,
    /// Live programs that appeared on the source's core after this viewer
    /// opened (another viewer or a shell started them).
    // Started only by production startup; fake-core unit tests never subscribe.
    #[cfg_attr(test, allow(dead_code))]
    HcProgramsAppeared {
        generation: u64,
        core_id: String,
        terminals: Vec<crate::hc::state::Terminal>,
        /// Asked again after a grace period: another viewer had time to
        /// place it in the shared layout.
        deferred: bool,
    },
    /// herdr.app's shared layout document changed on the source's core.
    // Sent only by production startup's subscription; fake-core unit tests never subscribe.
    #[cfg_attr(test, allow(dead_code))]
    HcLayoutChanged {
        generation: u64,
    },
    /// One round of keeping the shared layout in step finished: the document
    /// as it now is, the core's live programs, and the layout this viewer sent.
    HcLayoutSynced {
        generation: u64,
        outcome: Result<
            (
                serde_json::Value,
                Vec<String>,
                Vec<crate::hc_entry::app_layout::ShownSpace>,
            ),
            String,
        >,
    },
    HcTerminalEffects {
        enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        pane_id: crate::layout::PaneId,
        bells: u16,
        clipboard_writes: Vec<Vec<u8>>,
    },
    /// Source-owned facts; the registry token fences replaced/detached attachments.
    HcTerminalUpdated {
        terminal_id: crate::terminal::TerminalId,
        runtime_token: std::sync::Arc<()>,
        snapshot: crate::hc::state::RuntimeSnapshot,
    },
    /// A pane's child process exited.
    PaneDied {
        pane_id: PaneId,
        exit_reason: crate::platform::ChildExitReason,
    },
    /// A worktree-removal runtime could not be restored normally.
    WorktreeRuntimeRestoreFailed {
        pane_id: PaneId,
        operation_id: u64,
    },
    /// Process detection identified an agent before its screen state was confirmed.
    AgentProcessDetected {
        pane_id: PaneId,
        agent: Agent,
        observed_at: Instant,
    },
    /// The current Codex input screen is visible during managed startup.
    CodexPromptObserved {
        pane_id: PaneId,
        ready: bool,
    },
    /// Fallback detector state changed in a pane.
    StateChanged {
        pane_id: PaneId,
        agent: Option<Agent>,
        state: AgentState,
        visible_blocker: bool,
        visible_working: bool,
        process_exited: bool,
        observed_at: Instant,
    },
    /// Hook-authoritative agent state was reported for a pane.
    HookStateReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        seq: Option<u64>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
    },
    /// Agent session identity was reported without state authority.
    AgentSessionReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        seq: Option<u64>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
        session_start_source: Option<String>,
    },
    /// A reporter supplied the command that resumes its own session.
    AgentResumeReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        seq: Option<u64>,
        argv: Vec<String>,
    },
    /// A pane held by a self-reported agent is back at its idle shell.
    ReportedAgentShellReturned {
        pane_id: PaneId,
        observed_at: std::time::Instant,
    },
    /// Display-only agent metadata was reported for a pane.
    HookMetadataReported {
        pane_id: PaneId,
        source: String,
        agent_label: Option<String>,
        applies_to_source: Option<String>,
        title: Option<String>,
        display_agent: Option<String>,
        state_labels: std::collections::HashMap<String, String>,
        clear_title: bool,
        clear_display_agent: bool,
        clear_state_labels: bool,
        seq: Option<u64>,
        ttl: Option<std::time::Duration>,
    },
    /// Hook authority was explicitly cleared for a pane.
    HookAuthorityCleared {
        pane_id: PaneId,
        source: Option<String>,
        seq: Option<u64>,
    },
    /// The current detected agent gracefully released this pane back to the shell.
    HookAgentReleased {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        known_agent: Option<Agent>,
        seq: Option<u64>,
    },
    /// A new version is available through the active installation manager.
    UpdateReady {
        version: String,
        install_command: String,
    },
    /// Remote agent detection manifest update check finished.
    AgentDetectionManifestsUpdated {
        updated: Vec<crate::detect::manifest_update::ManifestUpdateCommit>,
        activated: Vec<crate::detect::Agent>,
        status: crate::detect::manifest_update::ManifestUpdateStatus,
    },
    /// A pane child emitted one or more executable BEL characters.
    /// The host-facing process forwards them to its outer terminal.
    TerminalBell {
        pane_id: PaneId,
        count: u16,
    },
    /// A pane child emitted a valid OSC 52 clipboard write. The main loop
    /// re-emits it through herdr's own clipboard writer.
    ClipboardWrite {
        content: Vec<u8>,
    },
    /// A pane child reported its shell current directory through terminal
    /// metadata such as OSC 7.
    TerminalCwdReported {
        pane_id: PaneId,
        cwd: std::path::PathBuf,
    },
    /// Background git status refresh completed for workspaces.
    GitStatusRefreshed {
        results: Vec<WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, GitStatusCacheEntry)>,
    },
    /// Background validation of a saved membership after session restore.
    RestoredWorktreeSpaceChecked {
        workspace_id: String,
        expected: crate::workspace::WorktreeSpaceMembership,
        valid: bool,
    },
    /// A configured tab bar status command finished.
    TabBarCommandFinished {
        generation: u64,
        segment_index: usize,
        result: Result<Option<String>, String>,
    },
    /// A plugin action or event command finished.
    PluginCommandFinished {
        log_id: String,
        finished_unix_ms: u64,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        error: Option<String>,
    },
    /// Background `git worktree add` completed.
    WorktreeAddFinished(Box<WorktreeAddResult>),
    /// Background `git worktree remove` completed.
    WorktreeRemoveFinished(Box<WorktreeRemoveResult>),
    /// Background worktree discovery completed for an API list/open request.
    WorktreeReadFinished(Box<WorktreeReadResult>),
}

impl AppEvent {
    pub(crate) fn legacy_runtime_pane(&self) -> Option<PaneId> {
        match self {
            Self::PaneDied { pane_id, .. }
            | Self::AgentProcessDetected { pane_id, .. }
            | Self::CodexPromptObserved { pane_id, .. }
            | Self::StateChanged { pane_id, .. }
            | Self::HookStateReported { pane_id, .. }
            | Self::AgentSessionReported { pane_id, .. }
            | Self::AgentResumeReported { pane_id, .. }
            | Self::ReportedAgentShellReturned { pane_id, .. }
            | Self::HookMetadataReported { pane_id, .. }
            | Self::HookAuthorityCleared { pane_id, .. }
            | Self::HookAgentReleased { pane_id, .. }
            | Self::TerminalCwdReported { pane_id, .. } => Some(*pane_id),
            _ => None,
        }
    }
}
