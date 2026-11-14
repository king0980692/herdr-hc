//! Agent sessions on hc terminals: recorded when an agent reports its
//! session, forgotten when it ends in its own shell, and resumed in the fresh
//! shell hc puts in place of a lost program (see `crate::hc::agent_session`).
use super::*;
use crate::hc::agent_session::{self as recorded, Decision};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

/// How long after recording or resuming a session an agent may take to come
/// to the front of its shell.
const SETTLING: Duration = Duration::from_secs(10);

pub(crate) struct HcAgentSessions {
    pub(crate) resume_enabled: bool,
    /// When this viewer last recorded or resumed a session, by hc address.
    touched: HashMap<String, Instant>,
    /// (address, pid) this viewer has tried to resume: once each.
    attempted: HashSet<(String, u64)>,
}

impl HcAgentSessions {
    pub(crate) fn new(resume_enabled: bool) -> Self {
        Self {
            resume_enabled,
            touched: HashMap::new(),
            attempted: HashSet::new(),
        }
    }

    fn settling(&self, address: &str) -> bool {
        self.touched
            .get(address)
            .is_some_and(|at| at.elapsed() < SETTLING)
    }
}

impl App {
    /// Records an agent's reported session on its hc terminal. `None` when
    /// the pane is not one this viewer can label (no live hc terminal).
    pub(crate) fn record_hc_agent_session(
        &mut self,
        ws_idx: usize,
        pane: crate::layout::PaneId,
        source: &str,
        agent: &str,
        session_ref: &crate::agent_resume::AgentSessionRef,
    ) -> Option<()> {
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)?
            .pane_state(pane)?
            .attached_terminal_id
            .clone();
        let snapshot = self
            .state
            .terminals
            .get(&terminal_id)?
            .hc_snapshot
            .as_ref()?;
        let facts = &snapshot.terminal.facts;
        let pid = recorded::live_pid(facts)?;
        // Reported while the program itself is in front: the agent is it.
        let root = recorded::root_in_front(facts);
        let address = snapshot.binding.address.clone();
        let labels = recorded::labels_for(source, agent, session_ref, pid, root);
        self.hc_agent_sessions
            .touched
            .insert(address.clone(), Instant::now());
        self.terminal_factory.label_hc_terminal(&address, labels);
        Some(())
    }

    /// What a terminal's latest facts call for. Cheap unless the terminal
    /// records a session.
    pub(super) fn follow_hc_agent_session(&mut self, snapshot: &RuntimeSnapshot) {
        let facts = &snapshot.terminal.facts;
        if !recorded::records_session(facts) {
            return;
        }
        let address = &snapshot.binding.address;
        let sessions = &self.hc_agent_sessions;
        let seen = recorded::Seen {
            enabled: sessions.resume_enabled,
            settling: sessions.settling(address),
            resumed_here: recorded::live_pid(facts)
                .is_some_and(|pid| sessions.attempted.contains(&(address.clone(), pid))),
        };
        match recorded::decide(facts, seen) {
            Decision::Nothing => {}
            Decision::Clear => {
                self.terminal_factory
                    .label_hc_terminal(address, recorded::cleared());
            }
            Decision::Resume { pid, plan } => {
                if !self
                    .hc_agent_sessions
                    .attempted
                    .insert((address.clone(), pid))
                {
                    return;
                }
                let Some(command) = super::super::agent_resume::shell_command_from_argv(&plan.argv)
                else {
                    return;
                };
                tracing::info!(%address, agent = %plan.agent, "resuming an agent session in its replacement shell");
                self.hc_agent_sessions
                    .touched
                    .insert(address.clone(), Instant::now());
                self.terminal_factory.resume_hc_agent(address, pid, command);
            }
        }
    }
}
