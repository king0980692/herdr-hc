//! An agent's session, kept on its hc terminal so the agent can be resumed
//! after the program was lost (a reboot, a crash): hc replaces a kept program
//! with a fresh shell at the same address, keeping its labels but not its pid.
//!
//! Labels are this app's own (`herdr-native.agent.*`), never herdr.app's. They
//! name the agent and its session, not a command: the command is rebuilt from
//! them with the same rules as any restore.

use crate::agent_resume::{AgentResumePlan, AgentSessionRef, AgentSessionRefKind};
use serde_json::{Map, Value};

pub(crate) const SOURCE_LABEL: &str = "herdr-native.agent.source";
pub(crate) const AGENT_LABEL: &str = "herdr-native.agent";
pub(crate) const SESSION_LABEL: &str = "herdr-native.agent.session";
/// The terminal's pid when the session was reported (or last resumed): a
/// different pid means hc replaced the program since.
pub(crate) const PID_LABEL: &str = "herdr-native.agent.pid";
/// Set when the agent is the terminal's own program (not run from its shell):
/// it in front is the agent running, not the agent having ended.
pub(crate) const ROOT_LABEL: &str = "herdr-native.agent.root";
/// The document whose versioned writes decide which viewer resumes.
pub(crate) const CLAIMS_DOCUMENT: &str = "herdr-native-resume";

const LABELS: [&str; 5] = [
    SOURCE_LABEL,
    AGENT_LABEL,
    SESSION_LABEL,
    PID_LABEL,
    ROOT_LABEL,
];

/// The labels recording `session_ref` for a terminal now running as `pid`;
/// `root` when the agent is that program itself.
pub(crate) fn labels_for(
    source: &str,
    agent: &str,
    session_ref: &AgentSessionRef,
    pid: u64,
    root: bool,
) -> Map<String, Value> {
    let kind = match session_ref.kind {
        AgentSessionRefKind::Id => "id",
        AgentSessionRefKind::Path => "path",
    };
    let mut labels = Map::new();
    labels.insert(SOURCE_LABEL.into(), source.into());
    labels.insert(AGENT_LABEL.into(), agent.into());
    labels.insert(
        SESSION_LABEL.into(),
        format!("{kind}:{}", session_ref.value).into(),
    );
    labels.insert(PID_LABEL.into(), pid.to_string().into());
    labels.insert(ROOT_LABEL.into(), if root { "1" } else { "" }.into());
    labels
}

/// The labels that remove a recorded session (`key=""` removes a label).
pub(crate) fn cleared() -> Map<String, Value> {
    LABELS
        .iter()
        .map(|key| ((*key).to_owned(), Value::from("")))
        .collect()
}

/// Whether a terminal's labels record a session at all (cheap: one lookup).
pub(crate) fn records_session(facts: &Map<String, Value>) -> bool {
    label(facts, SESSION_LABEL).is_some()
}

/// Whether the terminal's own program is the one in front.
pub(crate) fn root_in_front(facts: &Map<String, Value>) -> bool {
    live_pid(facts).is_some_and(|pid| {
        facts
            .get("foreground")
            .and_then(|foreground| foreground.get("pid"))
            .and_then(Value::as_u64)
            == Some(pid)
    })
}

/// The terminal's pid now, while it is live.
pub(crate) fn live_pid(facts: &Map<String, Value>) -> Option<u64> {
    (facts.get("state").and_then(Value::as_str) == Some("live"))
        .then(|| facts.get("pid").and_then(Value::as_u64))
        .flatten()
}

fn label<'a>(facts: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    facts
        .get("labels")?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
}

/// The plan a terminal's labels record, rebuilt and checked as any restore's.
fn recorded_plan(facts: &Map<String, Value>) -> Option<AgentResumePlan> {
    let source = label(facts, SOURCE_LABEL)?;
    let agent = label(facts, AGENT_LABEL)?;
    let session_ref = match label(facts, SESSION_LABEL)?.split_once(':')? {
        ("id", value) => AgentSessionRef::id(value)?,
        ("path", value) => AgentSessionRef::path(value)?,
        _ => return None,
    };
    let plan = crate::agent_resume::plan(source, agent, &session_ref)?;
    crate::agent_resume::validate_resume_argv(&plan.argv).ok()?;
    Some(plan)
}

#[derive(Debug, PartialEq)]
pub(crate) enum Decision {
    Nothing,
    /// The agent ended in its own shell: its session is no longer one to resume.
    Clear,
    /// The program was replaced (a fresh shell now runs as `pid`): resume.
    Resume {
        pid: u64,
        plan: AgentResumePlan,
    },
}

/// What this viewer knows beyond the terminal's facts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Seen {
    /// `[session] resume_agents_on_restore`.
    pub(crate) enabled: bool,
    /// This viewer recorded or resumed the session moments ago: the agent may
    /// not be in front yet.
    pub(crate) settling: bool,
    /// This viewer already typed the resume into the shell running now.
    pub(crate) resumed_here: bool,
}

/// What a terminal's facts call for.
pub(crate) fn decide(facts: &Map<String, Value>, seen: Seen) -> Decision {
    if !records_session(facts) {
        return Decision::Nothing;
    }
    let Some(pid) = live_pid(facts) else {
        return Decision::Nothing;
    };
    // Something the terminal's program started is in front (an agent, a tool).
    if !root_in_front(facts) {
        return Decision::Nothing;
    }
    let recorded_pid = label(facts, PID_LABEL).and_then(|pid| pid.parse::<u64>().ok());
    if recorded_pid == Some(pid) {
        // The same program as when reported: an agent run from its shell has
        // ended; an agent that is the program itself is what is in front.
        return if label(facts, ROOT_LABEL).is_some() || seen.settling {
            Decision::Nothing
        } else {
            Decision::Clear
        };
    }
    // Replaced since. Resumed here and the shell is back in front: it did not
    // take (or ended without reporting again); typing it again would not help.
    if seen.resumed_here {
        return if seen.settling {
            Decision::Nothing
        } else {
            Decision::Clear
        };
    }
    match recorded_plan(facts) {
        Some(plan) if seen.enabled => Decision::Resume { pid, plan },
        // Not one this build resumes (or resuming is off): it stays recorded.
        _ => Decision::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn facts(pid: u64, foreground: u64, labels: Map<String, Value>) -> Map<String, Value> {
        json!({"state": "live", "pid": pid, "foreground": {"pid": foreground}, "labels": labels})
            .as_object()
            .cloned()
            .unwrap()
    }

    const ON: Seen = Seen {
        enabled: true,
        settling: false,
        resumed_here: false,
    };

    fn claude(pid: u64) -> Map<String, Value> {
        labels_for(
            "herdr:claude",
            "claude",
            &AgentSessionRef::id("8df67940-1111-2222-3333-444455556666").unwrap(),
            pid,
            false,
        )
    }

    #[test]
    fn a_replaced_program_with_a_session_is_resumed_in_its_new_shell() {
        let decision = decide(&facts(200, 200, claude(100)), ON);
        let Decision::Resume { pid, plan } = decision else {
            panic!("{decision:?}");
        };
        assert_eq!(pid, 200);
        assert_eq!(
            plan.argv,
            ["claude", "--resume", "8df67940-1111-2222-3333-444455556666"]
        );
    }

    #[test]
    fn an_agent_that_ended_in_its_own_shell_is_forgotten() {
        assert_eq!(decide(&facts(100, 100, claude(100)), ON), Decision::Clear);
    }

    #[test]
    fn nothing_while_a_program_runs_settles_or_is_off() {
        // The agent (or anything) still in front.
        assert_eq!(decide(&facts(100, 150, claude(100)), ON), Decision::Nothing);
        assert_eq!(decide(&facts(200, 250, claude(100)), ON), Decision::Nothing);
        // Just recorded or resumed: the agent may not be in front yet.
        assert_eq!(
            decide(
                &facts(100, 100, claude(100)),
                Seen {
                    settling: true,
                    ..ON
                }
            ),
            Decision::Nothing
        );
        // Resuming is off: the session stays recorded, nothing is typed.
        assert_eq!(
            decide(
                &facts(200, 200, claude(100)),
                Seen {
                    enabled: false,
                    ..ON
                }
            ),
            Decision::Nothing
        );
        // No session recorded, or the program is not live.
        assert_eq!(decide(&facts(200, 200, Map::new()), ON), Decision::Nothing);
        let mut finished = facts(200, 200, claude(100));
        finished.insert("state".into(), "finished".into());
        assert_eq!(decide(&finished, ON), Decision::Nothing);
    }

    #[test]
    fn labels_never_become_an_arbitrary_command() {
        // A session value stays one argument (restore quotes each), and one
        // that could end the quoting is refused.
        let mut labels = claude(100);
        labels.insert(SESSION_LABEL.into(), "id:x; rm -rf ~".into());
        let Decision::Resume { plan, .. } = decide(&facts(200, 200, labels), ON) else {
            panic!("a plain session id");
        };
        assert_eq!(plan.argv, ["claude", "--resume", "x; rm -rf ~"]);
        let mut labels = claude(100);
        labels.insert(SESSION_LABEL.into(), "id:x' ; rm -rf ~ '".into());
        assert_eq!(decide(&facts(200, 200, labels), ON), Decision::Nothing);
        let mut labels = claude(100);
        labels.insert(SOURCE_LABEL.into(), "someone-else".into());
        assert_eq!(decide(&facts(200, 200, labels), ON), Decision::Nothing);
        let mut labels = claude(100);
        labels.insert(SESSION_LABEL.into(), "other:abc".into());
        assert_eq!(decide(&facts(200, 200, labels), ON), Decision::Nothing);
    }

    #[test]
    fn an_agent_that_is_the_program_itself_is_not_taken_for_ended() {
        let mut labels = claude(100);
        labels.insert(ROOT_LABEL.into(), "1".into());
        assert_eq!(
            decide(&facts(100, 100, labels.clone()), ON),
            Decision::Nothing
        );
        // Lost and replaced by a shell: resumed there.
        assert!(matches!(
            decide(&facts(200, 200, labels), ON),
            Decision::Resume { pid: 200, .. }
        ));
    }

    #[test]
    fn a_resume_that_did_not_take_is_forgotten_not_typed_again() {
        let resumed = Seen {
            resumed_here: true,
            ..ON
        };
        // Still starting: wait.
        assert_eq!(
            decide(
                &facts(200, 200, claude(100)),
                Seen {
                    settling: true,
                    ..resumed
                }
            ),
            Decision::Nothing
        );
        // The shell back in front after that: forgotten.
        assert_eq!(
            decide(&facts(200, 200, claude(100)), resumed),
            Decision::Clear
        );
        // A crash right after a report is still resumed.
        assert!(matches!(
            decide(
                &facts(200, 200, claude(100)),
                Seen {
                    settling: true,
                    ..ON
                }
            ),
            Decision::Resume { pid: 200, .. }
        ));
    }

    #[test]
    fn clearing_removes_every_label_it_set() {
        let set = claude(1);
        let cleared = cleared();
        assert_eq!(
            set.keys().collect::<Vec<_>>(),
            cleared.keys().collect::<Vec<_>>()
        );
        assert!(cleared.values().all(|value| value == ""));
    }
}
