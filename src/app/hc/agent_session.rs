//! Agent sessions on hc terminals: recorded when an agent reports its
//! session, forgotten when it ends in its own shell, and resumed in the fresh
//! shell hc puts in place of a lost program (see `crate::hc::agent_session`).
use super::*;
use crate::hc::agent_session::{self as recorded, Decision};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

const SETTLING: Duration = Duration::from_millis(recorded::SETTLING_MS);

pub(crate) struct HcAgentSessions {
    pub(crate) resume_enabled: bool,
    /// When this viewer last recorded or resumed a session, by hc address.
    touched: HashMap<String, Instant>,
    /// (address, pid) this viewer has tried to resume: once each.
    attempted: HashSet<(String, u64)>,
    /// Resumes start one `[session] startup_per_agent_delay_ms` apart, as a
    /// restore's do, so many agents back at once do not start together.
    delay: Duration,
    next_resume: Option<Instant>,
}

impl HcAgentSessions {
    pub(crate) fn new(resume_enabled: bool, delay: Duration) -> Self {
        Self {
            resume_enabled,
            touched: HashMap::new(),
            attempted: HashSet::new(),
            delay,
            next_resume: None,
        }
    }

    /// When the next resume may start: now, or one delay after the last.
    fn schedule(&mut self, now: Instant) -> Instant {
        let at = self.next_resume.filter(|next| *next > now).unwrap_or(now);
        self.next_resume = Some(at + self.delay);
        at
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
        // The terminal's own program is the agent (not a shell it runs in).
        let root = recorded::runs_as_program(facts, agent);
        let address = snapshot.binding.address.clone();
        let labels =
            recorded::labels_for(source, agent, session_ref, pid, root, recorded::now_ms());
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
            now_ms: recorded::now_ms(),
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
                let now = Instant::now();
                let at = self.hc_agent_sessions.schedule(now);
                tracing::info!(%address, agent = %plan.agent, wait_ms = (at - now).as_millis() as u64, "resuming an agent session in its replacement shell");
                // Settling counts from when it is typed, not from now.
                self.hc_agent_sessions.touched.insert(address.clone(), at);
                self.terminal_factory
                    .resume_hc_agent(address, pid, command, at - now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumes_start_one_delay_apart() {
        let mut sessions = HcAgentSessions::new(true, Duration::from_millis(500));
        let start = Instant::now();
        let times: Vec<Duration> = (0..3).map(|_| sessions.schedule(start) - start).collect();
        assert_eq!(
            times,
            [
                Duration::ZERO,
                Duration::from_millis(500),
                Duration::from_millis(1000)
            ]
        );
        // Long after the last: no wait.
        let later = start + Duration::from_secs(10);
        assert_eq!(sessions.schedule(later), later);
        // Scheduled ahead, settling holds until it is typed (and after).
        sessions
            .touched
            .insert("t1".into(), Instant::now() + Duration::from_secs(60));
        assert!(sessions.settling("t1"));
    }
}
