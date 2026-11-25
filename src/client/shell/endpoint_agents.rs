use super::render::put_text;
use super::*;

pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) {
    let rows = agent_rows(endpoints, active_endpoint_id, config);
    for (index, row) in rows.into_iter().take(area.height as usize).enumerate() {
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        if row.agent.focused {
            buffer.set_style(rect, Style::default().bg(config.palette.active_row_bg));
        }
        let initial = row.machine_label.chars().next().unwrap_or('?');
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width,
            &format!(
                "{initial}{}",
                status_icon(row.agent.status, config.status_indicators)
            ),
            Style::default()
                .fg(if row.stale {
                    config.palette.overlay0
                } else {
                    status_color(row.agent.status, &config.palette)
                })
                .add_modifier(if row.stale {
                    Modifier::DIM
                } else {
                    Modifier::empty()
                }),
        );
        hits.endpoint_agents
            .push((rect, row.endpoint_id, row.agent.pane_id));
    }
}

pub(super) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    folded: &HashSet<super::agent_sections::AgentSection>,
    hits: &mut ShellHitMap,
) {
    if !super::agent_sidebar::render_agent_panel_header(
        buffer,
        area,
        agent_view_label,
        config,
        hits,
    ) {
        return;
    }
    let rows = agent_rows(endpoints, active_endpoint_id, config);
    let render_row =
        |buffer: &mut Buffer, rect: Rect, row: &EndpointAgentRow, hits: &mut ShellHitMap| {
            super::agent_sidebar::render_agent_row(buffer, rect, &row.agent, config);
            if row.stale {
                buffer.set_style(
                    rect,
                    Style::default()
                        .fg(config.palette.overlay0)
                        .add_modifier(Modifier::DIM),
                );
            }
            hits.endpoint_agents
                .push((rect, row.endpoint_id.clone(), row.agent.pane_id.clone()));
        };
    if super::agent_sidebar::sections_shown(agent_view_label, config) {
        let items = super::agent_sections::with_headers(&rows, |row| row.agent.section, folded);
        let pinned = super::agent_sections::pinned_items(&items, |row| row.agent.section);
        super::agent_sidebar::render_agent_list(
            buffer,
            area,
            &items,
            pinned,
            None,
            config,
            agent_scroll,
            hits,
            false,
            |item| item.lines(|row| row.agent.rows.len()),
            |buffer, rect, item, hits| match item {
                super::agent_sections::PanelItem::Header {
                    section,
                    count,
                    folded,
                    ..
                } => super::agent_sections::render_header(
                    buffer,
                    rect,
                    *section,
                    *count,
                    *folded,
                    &config.palette,
                    hits,
                ),
                super::agent_sections::PanelItem::Row(row) => render_row(buffer, rect, row, hits),
            },
        );
        return;
    }
    let pinned = rows
        .iter()
        .take_while(|row| row.agent.section == super::agent_sections::AgentSection::Pinned)
        .count();
    super::agent_sidebar::render_agent_list(
        buffer,
        area,
        &rows,
        pinned,
        agent_view_label.map(|_| " no matching agents"),
        config,
        agent_scroll,
        hits,
        true,
        |row| row.agent.rows.len(),
        render_row,
    );
}

impl ClientShellState {
    pub(super) fn reveal_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        body_height: u16,
    ) {
        if body_height == 0 {
            return;
        }
        let rows = agent_rows(&self.endpoints, &self.active_endpoint_id, &self.config);
        let is_target = |row: &EndpointAgentRow| {
            &row.endpoint_id == endpoint_id && row.agent.pane_id == pane_id
        };
        let label = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .and_then(|endpoint| endpoint.snapshot.as_deref())
            .and_then(|snapshot| snapshot.agent_view_label.as_deref());
        // With sections, the headers take lines too; an agent in a folded
        // section has no line to reveal.
        let sections = super::agent_sidebar::sections_shown(label, &self.config);
        let (target, heights, pinned) = if sections {
            let items = super::agent_sections::with_headers(
                &rows,
                |row| row.agent.section,
                &self.agent_sections_folded,
            );
            let Some(target) = items.iter().position(
                |item| matches!(item, super::agent_sections::PanelItem::Row(row) if is_target(row)),
            ) else {
                return;
            };
            let heights = items
                .iter()
                .map(|item| {
                    item.lines(|row| row.agent.rows.len())
                        .max(1)
                        .min(u16::MAX as usize) as u16
                })
                .collect::<Vec<_>>();
            let pinned = super::agent_sections::pinned_items(&items, |row| row.agent.section);
            (target, heights, pinned)
        } else {
            let Some(target) = rows.iter().position(is_target) else {
                return;
            };
            let heights = rows
                .iter()
                .map(|row| row.agent.rows.len().max(1).min(u16::MAX as usize) as u16)
                .collect::<Vec<_>>();
            let pinned = rows
                .iter()
                .take_while(|row| row.agent.section == super::agent_sections::AgentSection::Pinned)
                .count();
            (target, heights, pinned)
        };
        // Pinned rows stay at the top: always shown, and the others scroll
        // in the lines below them.
        let (pinned, pinned_height) = super::agent_sidebar::pinned_prefix(
            &heights,
            pinned,
            self.config.agents.row_gap,
            // Without sections, a rule sets the pinned ones apart.
            !sections,
            body_height,
        );
        if target < pinned {
            return;
        }
        let target = target - pinned;
        let heights = heights[pinned..].to_vec();
        let body_height = body_height - pinned_height;
        let mut gaps = vec![self.config.agents.row_gap; heights.len()];
        if let Some(last) = gaps.last_mut() {
            *last = 0;
        }
        self.agent_scroll = super::scroll::list_scroll_start_to_reveal(
            &heights,
            &gaps,
            body_height,
            self.agent_scroll,
            target,
        );
    }
}

struct EndpointAgentRow {
    endpoint_id: ClientEndpointId,
    machine_label: String,
    stale: bool,
    agent: super::agent_sidebar::AgentRow,
}

fn agent_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
) -> Vec<EndpointAgentRow> {
    let mut rendered_rows = endpoints
        .iter()
        .filter_map(|endpoint| {
            endpoint.snapshot.as_deref().map(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .filter_map(|agent| {
                        super::agent_sidebar::agent_row(
                            snapshot,
                            &agent.pane_id,
                            config,
                            Some(&endpoint.label),
                        )
                    })
                    .map(|agent| ((endpoint.endpoint_id.clone(), agent.pane_id.clone()), agent))
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect::<HashMap<_, _>>();

    super::aggregate_navigation::aggregate_agent_rows(
        endpoints,
        active_endpoint_id,
        config.agent_panel_sort,
    )
    .into_iter()
    .filter_map(|row| {
        let key = (row.endpoint.endpoint_id.clone(), row.agent.pane_id.clone());
        let mut agent = rendered_rows.remove(&key)?;
        agent.focused &= row.endpoint.endpoint_id == active_endpoint_id;
        Some(EndpointAgentRow {
            endpoint_id: row.endpoint.endpoint_id.clone(),
            machine_label: row.endpoint.label.to_owned(),
            stale: row.endpoint.stale(),
            agent,
        })
    })
    .collect()
}
