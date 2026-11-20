//! `herdr.seen` on an hc terminal is the last turn someone looked at, kept
//! by every app that shows the program: herdr.app shows a finished program
//! as done until then. Looking at a tab here (a client shows it and its
//! terminal has focus) moves `herdr.seen` on for its programs, so both apps
//! stop showing them as done.
use super::*;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct SeenTurns {
    /// The turn last written for each hc address, so one look writes once
    /// while the label change is on its way back.
    written: HashMap<String, u64>,
    /// The tabs clients show with their terminal focused, by public id, as
    /// the server tracks them; `None` where this app draws its own one view
    /// (its active tab, when the terminal has focus).
    viewed: Option<std::collections::HashSet<String>>,
}

/// The `herdr.seen` to write: the program's turn, when it is past the seen one
/// and was not just written.
fn to_write(turn: u64, seen: u64, written: Option<u64>) -> Option<u64> {
    (turn > seen && written != Some(turn)).then_some(turn)
}

impl App {
    /// The tabs clients now show with their terminal focused (the server's
    /// own record of who looks at what).
    pub(crate) fn set_viewed_tabs(&mut self, tabs: std::collections::HashSet<String>) {
        self.seen_turns.viewed = Some(tabs);
    }

    /// Moves `herdr.seen` on for the programs of the tabs being looked at.
    pub(crate) fn mark_hc_turns_seen(&mut self) {
        let tabs: Vec<(usize, usize)> = match &self.seen_turns.viewed {
            Some(viewed) => viewed
                .iter()
                .filter_map(|id| self.parse_tab_id(id))
                .collect(),
            // Someone looking: the terminal this app draws in has focus.
            None if self.state.outer_terminal_focus == Some(true) => self
                .state
                .active
                .and_then(|ws| {
                    let tab = self.state.workspaces.get(ws)?.active_tab_index();
                    Some((ws, tab))
                })
                .into_iter()
                .collect(),
            None => return,
        };
        let due: Vec<(String, u64)> = tabs
            .into_iter()
            .filter_map(|(ws, tab)| self.state.workspaces.get(ws)?.tabs.get(tab))
            .flat_map(|tab| tab.panes.values())
            .filter_map(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .filter(|terminal| terminal.hc_authority)
            .filter_map(|terminal| {
                let (turn, seen) = terminal.hc_turns()?;
                let address = terminal.hc_snapshot.as_ref()?.binding.address.clone();
                let turn = to_write(turn, seen, self.seen_turns.written.get(&address).copied())?;
                Some((address, turn))
            })
            .collect();
        for (address, turn) in due {
            self.seen_turns.written.insert(address.clone(), turn);
            let mut labels = serde_json::Map::new();
            labels.insert(
                crate::terminal::SEEN_LABEL.into(),
                serde_json::Value::from(turn.to_string()),
            );
            self.terminal_factory.label_hc_terminal(&address, labels);
        }
    }
}

#[cfg(test)]
impl App {
    pub(crate) fn seen_turns_written_for_test(&self, address: &str) -> Option<u64> {
        self.seen_turns.written.get(address).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_look_writes_the_turn_once_and_only_past_the_seen_one() {
        assert_eq!(to_write(5, 3, None), Some(5));
        assert_eq!(to_write(5, 3, Some(5)), None, "already on its way");
        assert_eq!(to_write(6, 5, Some(5)), Some(6), "a later turn");
        assert_eq!(to_write(5, 5, None), None, "nothing new");
        assert_eq!(to_write(0, 0, None), None);
    }
}
