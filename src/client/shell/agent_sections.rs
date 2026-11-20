//! The agents panel's `status` order: agents in sections (Pinned, Blocked,
//! Done, Working, Idle), each under a header with its count, as herdr.app
//! shows them. Idle starts folded; clicking a header folds or unfolds it.
use std::collections::HashSet;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::*;
use crate::api::schema::AgentStatus;
use crate::protocol::ClientShellAgent;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum AgentSection {
    Pinned,
    Blocked,
    Done,
    Working,
    Idle,
}

impl AgentSection {
    pub(super) fn of(agent: &ClientShellAgent) -> Self {
        if super::agent_pins::pinned_at(agent).is_some() {
            return Self::Pinned;
        }
        match agent.agent_status {
            AgentStatus::Blocked => Self::Blocked,
            AgentStatus::Done => Self::Done,
            AgentStatus::Working => Self::Working,
            AgentStatus::Idle | AgentStatus::Unknown => Self::Idle,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Pinned => "Pinned",
            Self::Blocked => "Blocked",
            Self::Done => "Done",
            Self::Working => "Working",
            Self::Idle => "Idle",
        }
    }

    fn color(self, palette: &Palette) -> ratatui::style::Color {
        match self {
            Self::Pinned | Self::Idle => palette.overlay0,
            Self::Blocked => status_color(AgentStatus::Blocked, palette),
            Self::Done => status_color(AgentStatus::Done, palette),
            Self::Working => status_color(AgentStatus::Working, palette),
        }
    }
}

/// Folded until the user opens them.
pub(super) fn initially_folded() -> HashSet<AgentSection> {
    [AgentSection::Idle].into()
}

/// The `status` order: by section; pinned ones by pin time, the others the
/// latest change first (`recency`, larger is later).
pub(super) fn status_order_key(
    agent: &ClientShellAgent,
    recency: u64,
) -> (AgentSection, u64, std::cmp::Reverse<u64>) {
    let section = AgentSection::of(agent);
    let pinned = super::agent_pins::pinned_at(agent).unwrap_or_default();
    (section, pinned, std::cmp::Reverse(recency))
}

pub(super) enum PanelItem<'a, T> {
    Header {
        section: AgentSection,
        count: usize,
        folded: bool,
        /// A blank line above, apart from the first header.
        spaced: bool,
    },
    Row(&'a T),
}

impl<T> PanelItem<'_, T> {
    pub(super) fn lines(&self, row_lines: impl Fn(&T) -> usize) -> usize {
        match self {
            Self::Header { spaced, .. } => 1 + usize::from(*spaced),
            Self::Row(row) => row_lines(row),
        }
    }
}

/// `rows`, already in `status` order, with a header before each section;
/// a folded section keeps only its header.
pub(super) fn with_headers<'a, T>(
    rows: &'a [T],
    section_of: impl Fn(&T) -> AgentSection,
    folded: &HashSet<AgentSection>,
) -> Vec<PanelItem<'a, T>> {
    let mut items = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        let section = section_of(&rows[index]);
        let end = rows[index..]
            .iter()
            .position(|row| section_of(row) != section)
            .map_or(rows.len(), |offset| index + offset);
        let is_folded = folded.contains(&section);
        items.push(PanelItem::Header {
            section,
            count: end - index,
            folded: is_folded,
            spaced: !items.is_empty(),
        });
        if !is_folded {
            items.extend(rows[index..end].iter().map(PanelItem::Row));
        }
        index = end;
    }
    items
}

/// The leading items that stay put while the rest scrolls: the Pinned
/// header and its rows.
pub(super) fn pinned_items<T>(
    items: &[PanelItem<'_, T>],
    section_of: impl Fn(&T) -> AgentSection,
) -> usize {
    items
        .iter()
        .take_while(|item| match item {
            PanelItem::Header { section, .. } => *section == AgentSection::Pinned,
            PanelItem::Row(row) => section_of(row) == AgentSection::Pinned,
        })
        .count()
}

/// Draws a header on the last line of `rect` and records where it is.
pub(super) fn render_header(
    buffer: &mut Buffer,
    rect: Rect,
    section: AgentSection,
    count: usize,
    folded: bool,
    palette: &Palette,
    hits: &mut ShellHitMap,
) {
    if rect.height == 0 || rect.width == 0 {
        return;
    }
    let line = Rect::new(rect.x, rect.bottom() - 1, rect.width, 1);
    let title = format!(" {} ", section.title());
    let mut x = line.x;
    for (text, style) in [
        (
            title.as_str(),
            Style::default()
                .fg(section.color(palette))
                .add_modifier(Modifier::BOLD),
        ),
        (
            &count.to_string(),
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::DIM),
        ),
    ] {
        for character in text.chars() {
            if x >= line.right() {
                break;
            }
            if let Some(cell) = buffer.cell_mut((x, line.y)) {
                cell.set_char(character).set_style(style);
            }
            x += 1;
        }
    }
    if folded && line.width > 2 {
        if let Some(cell) = buffer.cell_mut((line.right() - 2, line.y)) {
            cell.set_char('›')
                .set_style(Style::default().fg(palette.overlay0));
        }
    }
    hits.agent_sections.push((line, section));
}

impl ClientShellState {
    /// A header click: its section folds or unfolds.
    pub(super) fn toggle_agent_section(&mut self, section: AgentSection) {
        if !self.agent_sections_folded.remove(&section) {
            self.agent_sections_folded.insert(section);
        }
        self.agent_scroll = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_get_a_counted_header_and_folded_ones_keep_only_it() {
        use AgentSection::*;
        let rows = [Pinned, Done, Done, Working, Idle, Idle, Idle];
        let items = with_headers(&rows, |row| *row, &initially_folded());
        let shape = items
            .iter()
            .map(|item| match item {
                PanelItem::Header {
                    section,
                    count,
                    folded,
                    spaced,
                } => format!("{section:?}:{count}:{folded}:{spaced}"),
                PanelItem::Row(row) => format!("{row:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            [
                "Pinned:1:false:false",
                "Pinned",
                "Done:2:false:true",
                "Done",
                "Done",
                "Working:1:false:true",
                "Working",
                "Idle:3:true:true",
            ]
        );
        assert!(with_headers(&[] as &[AgentSection], |row| *row, &HashSet::new()).is_empty());
    }
}
