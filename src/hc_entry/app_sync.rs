//! A Native viewer's layout and herdr.app's shared document (`herdr-app`),
//! kept in step. Native shows only terminals arranged in rows and columns;
//! everything else in the document (stacks, mods, floating blocks, focus,
//! fields this build does not know) is left exactly as it is.
//!
//! Writing is three-way: only what this viewer changed since the layout it
//! last synced (`base`) is applied to the latest document, so a change
//! another viewer made meanwhile is never undone.

use super::app_layout::{
    children, children_mut, collapse, is_line, kind, place_split, remove_terminal, set_weight,
    shown_layout, terminal, ShownLayout, ShownSpace, ShownTab, SplitPlacement,
};
use crate::persist::{DirectionSnapshot, LayoutSnapshot};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Shares are compared and written to a thousandth.
fn rounded(share: f64) -> f64 {
    (share * 1000.0).round() / 1000.0
}

fn is_row(direction: &DirectionSnapshot) -> bool {
    matches!(direction, DirectionSnapshot::Horizontal)
}

fn single_or_split(row: bool, mut parts: Vec<(f64, ShownLayout)>) -> Option<ShownLayout> {
    match parts.len() {
        0 => None,
        1 => parts.pop().map(|(_, only)| only),
        _ => Some(ShownLayout::Split { row, parts }),
    }
}

fn along(
    node: &LayoutSnapshot,
    row: bool,
    share: f64,
    address: &impl Fn(u32) -> Option<String>,
    out: &mut Vec<(f64, ShownLayout)>,
) {
    match node {
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } if is_row(direction) == row => {
            let ratio = f64::from(*ratio).clamp(0.0, 1.0);
            along(first, row, share * ratio, address, out);
            along(second, row, share * (1.0 - ratio), address, out);
        }
        other => {
            if let Some(layout) = from_binary(other, address) {
                out.push((share, layout));
            }
        }
    }
}

/// Native's two-way splits as herdr.app's rows and columns: a chain of
/// splits along one axis is one row or column, each part with its share.
/// Panes `address` does not know are left out.
pub(crate) fn from_binary(
    layout: &LayoutSnapshot,
    address: &impl Fn(u32) -> Option<String>,
) -> Option<ShownLayout> {
    match layout {
        LayoutSnapshot::Pane(pane) => address(*pane).map(ShownLayout::Program),
        LayoutSnapshot::Split { direction, .. } => {
            let row = is_row(direction);
            let mut parts = Vec::new();
            along(layout, row, 1.0, address, &mut parts);
            single_or_split(row, parts)
        }
    }
}

/// One form for equal arrangements: rows inside rows (columns inside
/// columns) merged, shares as rounded fractions of their row or column.
pub(crate) fn canonical(layout: &ShownLayout) -> ShownLayout {
    match layout {
        ShownLayout::Program(address) => ShownLayout::Program(address.clone()),
        ShownLayout::Split { row, parts } => {
            let total: f64 = parts.iter().map(|(share, _)| *share).sum();
            let mut flat = Vec::new();
            for (share, part) in parts {
                let share = if total > 0.0 { share / total } else { 0.0 };
                match canonical(part) {
                    ShownLayout::Split {
                        row: inner,
                        parts: inner_parts,
                    } if inner == *row => {
                        flat.extend(inner_parts.into_iter().map(|(s, p)| (share * s, p)));
                    }
                    other => flat.push((share, other)),
                }
            }
            ShownLayout::Split {
                row: *row,
                parts: flat
                    .into_iter()
                    .map(|(share, part)| (rounded(share), part))
                    .collect(),
            }
        }
    }
}

fn programs(layout: &ShownLayout, out: &mut Vec<String>) {
    match layout {
        ShownLayout::Program(address) => out.push(address.clone()),
        ShownLayout::Split { parts, .. } => parts.iter().for_each(|(_, p)| programs(p, out)),
    }
}

fn all_programs(spaces: &[ShownSpace]) -> HashSet<String> {
    let mut out = Vec::new();
    for tab in spaces.iter().flat_map(|space| &space.tabs) {
        programs(&tab.layout, &mut out);
    }
    out.into_iter().collect()
}

fn id(node: &Value) -> &str {
    node.get("id").and_then(Value::as_str).unwrap_or_default()
}

fn spaces_mut(document: &mut Value) -> Option<&mut Vec<Value>> {
    if !document.is_object() {
        *document = json!({});
    }
    let object = document.as_object_mut()?;
    if !object.get("spaces").is_some_and(Value::is_array) {
        object.insert("spaces".into(), json!([]));
    }
    object.get_mut("spaces").and_then(Value::as_array_mut)
}

fn tabs_mut(space: &mut Value) -> Option<&mut Vec<Value>> {
    let object = space.as_object_mut()?;
    if !object.get("tabs").is_some_and(Value::is_array) {
        object.insert("tabs".into(), json!([]));
    }
    object.get_mut("tabs").and_then(Value::as_array_mut)
}

/// Stacks and floating blocks keep identity Native cannot see; a tab with
/// them is never rebuilt.
fn rebuildable(node: &Value) -> bool {
    kind(node) != "stack"
        && node
            .get("floating")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        && children(node).is_none_or(|c| c.iter().all(rebuildable))
}

/// What a rebuilt tab reuses from its old tree: blocks by terminal, rows
/// and columns by the terminals they hold, and blocks that show no terminal.
#[derive(Default)]
struct Parts {
    blocks: HashMap<String, Value>,
    lines: HashMap<(String, Vec<String>), Value>,
    others: Vec<Value>,
}

fn held(node: &Value, out: &mut Vec<String>) {
    if let Some(address) = terminal(node) {
        out.push(address.to_owned());
    }
    for child in children(node).into_iter().flatten() {
        held(child, out);
    }
}

fn collect_parts(node: &Value, parts: &mut Parts, root: bool) {
    if let Some(address) = terminal(node) {
        let mut block = node.clone();
        if let Some(object) = block.as_object_mut() {
            object.remove("size");
        }
        parts.blocks.insert(address.to_owned(), block);
        return;
    }
    if kind(node) == "block" {
        let mut block = node.clone();
        if let Some(object) = block.as_object_mut() {
            object.remove("size");
        }
        parts.others.push(block);
        return;
    }
    if is_line(node) && !root {
        let mut terminals = Vec::new();
        held(node, &mut terminals);
        terminals.sort();
        let mut line = node.clone();
        if let Some(object) = line.as_object_mut() {
            object.remove("children");
            object.remove("size");
        }
        parts.lines.insert((kind(node).to_owned(), terminals), line);
    }
    for child in children(node).into_iter().flatten() {
        collect_parts(child, parts, false);
    }
}

fn build(
    layout: &ShownLayout,
    parts: &mut Parts,
    fresh_id: &mut impl FnMut(&str) -> String,
) -> Value {
    match layout {
        ShownLayout::Program(address) => parts.blocks.remove(address).unwrap_or_else(
            || json!({"id": fresh_id("p"), "type": "block", "content": {"terminal": address}}),
        ),
        ShownLayout::Split { row, parts: shares } => {
            let axis = if *row { "row" } else { "column" };
            let mut terminals = Vec::new();
            programs(layout, &mut terminals);
            terminals.sort();
            let mut line = parts
                .lines
                .remove(&(axis.to_owned(), terminals))
                .unwrap_or_else(|| json!({"id": fresh_id("s")}));
            let total: f64 = shares.iter().map(|(share, _)| *share).sum();
            let built: Vec<Value> = shares
                .iter()
                .map(|(share, part)| {
                    let mut child = build(part, parts, fresh_id);
                    let share = if total > 0.0 { share / total } else { 0.0 };
                    set_weight(&mut child, rounded(share));
                    child
                })
                .collect();
            if let Some(object) = line.as_object_mut() {
                object.insert("type".into(), axis.into());
                object.insert("children".into(), Value::Array(built));
            }
            line
        }
    }
}

/// A tab root arranged as `layout`, keeping the old root's own fields, its
/// blocks and lines, and the blocks that show no terminal (after the rest).
fn rebuilt_root(
    old: Option<&Value>,
    tab_id: &str,
    layout: &ShownLayout,
    fresh_id: &mut impl FnMut(&str) -> String,
) -> Value {
    let mut parts = Parts::default();
    if let Some(old) = old {
        collect_parts(old, &mut parts, true);
    }
    let built = build(layout, &mut parts, fresh_id);
    let (axis, mut members) = match &built {
        line if is_line(line) => (
            kind(line).to_owned(),
            children(line).cloned().unwrap_or_default(),
        ),
        block => ("row".to_owned(), vec![block.clone()]),
    };
    members.append(&mut parts.others);
    let mut root = old.and_then(Value::as_object).cloned().unwrap_or_default();
    if is_line(&built) {
        if let Some(fields) = built.as_object() {
            for (key, value) in fields {
                if key != "id" && key != "children" && key != "size" {
                    root.insert(key.clone(), value.clone());
                }
            }
        }
    }
    root.insert("id".into(), tab_id.into());
    root.insert("type".into(), axis.into());
    root.insert("children".into(), Value::Array(members));
    Value::Object(root)
}

/// Where a program new in this tab goes in the document: right after the
/// program before it in its row or column, or `None` when that is not a
/// single program.
fn anchor(layout: &ShownLayout, new: &str) -> Option<(String, bool, f64)> {
    let ShownLayout::Split { row, parts } = layout else {
        return None;
    };
    for (index, (share, part)) in parts.iter().enumerate() {
        if matches!(part, ShownLayout::Program(address) if address == new) {
            let (before, ShownLayout::Program(source)) = parts.get(index.checked_sub(1)?)? else {
                return None;
            };
            let total = before + share;
            let ratio = if total > 0.0 { before / total } else { 0.5 };
            return Some((source.clone(), *row, ratio));
        }
        if let Some(found) = anchor(part, new) {
            return Some(found);
        }
    }
    None
}

fn doc_layout(tab: &Value, shown: &impl Fn(&str) -> bool) -> Option<ShownLayout> {
    tab.get("root")
        .and_then(|root| shown_layout(root, &mut |address: &str| shown(address)))
}

fn new_tab(tab: &ShownTab, fresh_id: &mut impl FnMut(&str) -> String) -> Value {
    json!({
        "id": tab.id,
        "name": tab.name,
        "root": rebuilt_root(None, &tab.id, &tab.layout, fresh_id),
    })
}

/// Applies what this viewer changed from `base` to `native` to herdr.app's
/// `document`; whether it changed. A document another viewer changed
/// meanwhile keeps those changes: removed programs and closed tabs go,
/// new programs are placed beside their neighbor (or at the end of their
/// tab), and a tab is rearranged only while nobody else changed it.
pub(crate) fn push_changes(
    document: &mut Value,
    base: &[ShownSpace],
    native: &[ShownSpace],
    shown: impl Fn(&str) -> bool,
    mut fresh_id: impl FnMut(&str) -> String,
) -> bool {
    let before = document.clone();
    // Whether another viewer rearranged a tab is judged before this one's
    // own removals change it.
    let arranged: HashMap<String, Option<ShownLayout>> = before
        .get("spaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|space| space.get("tabs").and_then(Value::as_array))
        .flatten()
        .map(|tab| {
            (
                id(tab).to_owned(),
                doc_layout(tab, &shown).map(|l| canonical(&l)),
            )
        })
        .collect();
    let base_programs = all_programs(base);
    let native_programs = all_programs(native);
    // A program new to this viewer that the document already places (another
    // viewer added it meanwhile) stays where the document has it.
    let placed: HashSet<String> = before
        .get("spaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|space| space.get("tabs").and_then(Value::as_array))
        .flatten()
        .filter_map(|tab| doc_layout(tab, &shown))
        .flat_map(|layout| {
            let mut out = Vec::new();
            programs(&layout, &mut out);
            out
        })
        .collect();
    let ours = |address: &str| base_programs.contains(address) || !placed.contains(address);
    for address in base_programs.difference(&native_programs) {
        for root in super::app_layout::tab_roots_mut(document) {
            if remove_terminal(root, address) {
                collapse(root);
            }
        }
    }
    let native_spaces: HashSet<&str> = native.iter().map(|space| space.id.as_str()).collect();
    let native_tabs: HashSet<&str> = native
        .iter()
        .flat_map(|space| &space.tabs)
        .map(|tab| tab.id.as_str())
        .collect();
    let base_tabs: HashMap<&str, &ShownTab> = base
        .iter()
        .flat_map(|space| &space.tabs)
        .map(|tab| (tab.id.as_str(), tab))
        .collect();
    let Some(spaces) = spaces_mut(document) else {
        return false;
    };
    // Closed here: spaces and tabs this viewer had and no longer has. One
    // still showing a program this viewer shows was not closed (its id is
    // just not this viewer's): it stays.
    let shows_ours = |node: &Value| {
        let mut held_here = Vec::new();
        held(node, &mut held_here);
        held_here
            .iter()
            .any(|address| native_programs.contains(address))
    };
    spaces.retain(|space| {
        !base.iter().any(|b| b.id == id(space))
            || native_spaces.contains(id(space))
            || space
                .get("tabs")
                .and_then(Value::as_array)
                .is_some_and(|tabs| tabs.iter().filter_map(|t| t.get("root")).any(&shows_ours))
    });
    for space in spaces.iter_mut() {
        if let Some(tabs) = tabs_mut(space) {
            tabs.retain(|tab| {
                !base_tabs.contains_key(id(tab))
                    || native_tabs.contains(id(tab))
                    || tab.get("root").is_some_and(&shows_ours)
            });
        }
    }
    for space in native {
        let base_space = base.iter().find(|b| b.id == space.id);
        let index = match spaces.iter().position(|s| id(s) == space.id) {
            Some(index) => index,
            // Removed elsewhere since this viewer synced: that stands.
            None if base_space.is_some() => continue,
            None => {
                spaces.push(json!({
                    "id": space.id, "name": space.name, "folder": space.folder, "tabs": [],
                }));
                spaces.len() - 1
            }
        };
        let Some(doc_space) = spaces.get_mut(index) else {
            continue;
        };
        if base_space.is_some_and(|b| b.name != space.name) {
            if let Some(object) = doc_space.as_object_mut() {
                object.insert("name".into(), space.name.clone().into());
            }
        }
        let Some(tabs) = tabs_mut(doc_space) else {
            continue;
        };
        for tab in &space.tabs {
            let base_tab = base_tabs.get(tab.id.as_str()).copied();
            let Some(doc_tab) = tabs.iter_mut().find(|t| id(t) == tab.id) else {
                if base_tab.is_none() {
                    if let Some(layout) = only(&tab.layout, &ours) {
                        let tab = ShownTab {
                            layout,
                            ..tab.clone()
                        };
                        tabs.push(new_tab(&tab, &mut fresh_id));
                    }
                }
                continue;
            };
            if base_tab.is_some_and(|b| b.name != tab.name) {
                if let Some(object) = doc_tab.as_object_mut() {
                    object.insert("name".into(), tab.name.clone().into());
                }
            }
            let wanted = canonical(&tab.layout);
            let synced = base_tab.map(|b| canonical(&b.layout));
            if synced.as_ref() == Some(&wanted) {
                continue;
            }
            let untouched = synced.is_some() && arranged.get(&tab.id) == Some(&synced);
            if untouched && doc_tab.get("root").is_some_and(rebuildable) {
                let Some(layout) = only(&tab.layout, &ours) else {
                    continue;
                };
                let root = rebuilt_root(doc_tab.get("root"), &tab.id, &layout, &mut fresh_id);
                if let Some(object) = doc_tab.as_object_mut() {
                    object.insert("root".into(), root);
                }
                continue;
            }
            // Changed elsewhere too: only this viewer's new programs go in.
            let mut added = Vec::new();
            programs(&tab.layout, &mut added);
            for new in added
                .iter()
                .filter(|a| !base_programs.contains(a.as_str()) && ours(a))
            {
                let present_here = doc_layout(doc_tab, &shown).is_some_and(|layout| {
                    let mut present = Vec::new();
                    programs(&layout, &mut present);
                    present.contains(new)
                });
                if present_here {
                    continue;
                }
                let mut single = json!({"spaces": [{"tabs": [doc_tab.clone()]}]});
                let placed = anchor(&tab.layout, new).is_some_and(|(source, row, ratio)| {
                    place_split(&mut single, &source, new, row, ratio, &mut fresh_id)
                        == SplitPlacement::Placed
                });
                if placed {
                    if let Some(tab_value) = single.pointer("/spaces/0/tabs/0") {
                        *doc_tab = tab_value.clone();
                    }
                } else if let Some(members) = doc_tab.get_mut("root").and_then(children_mut) {
                    members.push(json!({
                        "id": fresh_id("p"), "type": "block", "content": {"terminal": new},
                    }));
                }
            }
        }
        // Tabs moved here: the order this viewer gave the ones it synced.
        if let Some(base_space) = base_space {
            let order: Vec<&str> = space.tabs.iter().map(|t| t.id.as_str()).collect();
            let synced: Vec<&str> = base_space
                .tabs
                .iter()
                .map(|t| t.id.as_str())
                .filter(|t| order.contains(t))
                .collect();
            let kept: Vec<&str> = order
                .iter()
                .copied()
                .filter(|t| synced.contains(t))
                .collect();
            if kept != synced {
                let position = |tab: &Value| order.iter().position(|t| *t == id(tab));
                let (mut known, unknown): (Vec<Value>, Vec<Value>) =
                    tabs.drain(..).partition(|tab| position(tab).is_some());
                known.sort_by_key(|tab| position(tab));
                tabs.extend(known);
                tabs.extend(unknown);
            }
        }
    }
    *document != before
}

fn only(layout: &ShownLayout, keep: &impl Fn(&str) -> bool) -> Option<ShownLayout> {
    match layout {
        ShownLayout::Program(address) => keep(address).then(|| layout.clone()),
        ShownLayout::Split { row, parts } => single_or_split(
            *row,
            parts
                .iter()
                .filter_map(|(share, part)| Some((*share, only(part, keep)?)))
                .collect(),
        ),
    }
}

/// The same spaces and tabs holding only the programs `keep` accepts;
/// tabs and spaces left empty are dropped.
pub(crate) fn restricted(spaces: &[ShownSpace], keep: impl Fn(&str) -> bool) -> Vec<ShownSpace> {
    spaces
        .iter()
        .filter_map(|space| {
            let tabs: Vec<ShownTab> = space
                .tabs
                .iter()
                .filter_map(|tab| {
                    Some(ShownTab {
                        layout: only(&tab.layout, &keep)?,
                        ..tab.clone()
                    })
                })
                .collect();
            (!tabs.is_empty()).then(|| ShownSpace {
                tabs,
                ..space.clone()
            })
        })
        .collect()
}

/// What a viewer syncing for the first time shows: the document's layout,
/// then its own programs the document does not have, kept in their tabs.
pub(crate) fn union(document: &[ShownSpace], native: &[ShownSpace]) -> Vec<ShownSpace> {
    let in_document = all_programs(document);
    let mut out = document.to_vec();
    for space in native {
        let tabs: Vec<ShownTab> = space
            .tabs
            .iter()
            .filter_map(|tab| {
                Some(ShownTab {
                    id: tab.id.clone(),
                    name: tab.name.clone(),
                    layout: only(&tab.layout, &|address| !in_document.contains(address))?,
                })
            })
            .collect();
        if tabs.is_empty() {
            continue;
        }
        match out.iter_mut().find(|s| s.id == space.id) {
            Some(existing) => existing.tabs.extend(tabs),
            None => out.push(ShownSpace {
                tabs,
                ..space.clone()
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hc_entry::app_layout::shown_spaces;

    fn ids() -> impl FnMut(&str) -> String {
        let mut next = 0;
        move |prefix: &str| {
            next += 1;
            format!("{prefix}n{next}")
        }
    }

    fn p(address: &str) -> ShownLayout {
        ShownLayout::Program(address.into())
    }

    fn split(row: bool, parts: Vec<(f64, ShownLayout)>) -> ShownLayout {
        ShownLayout::Split { row, parts }
    }

    fn space(id: &str, tabs: Vec<ShownTab>) -> ShownSpace {
        ShownSpace {
            id: id.into(),
            name: "S".into(),
            folder: "/w".into(),
            tabs,
        }
    }

    fn tab(id: &str, layout: ShownLayout) -> ShownTab {
        ShownTab {
            id: id.into(),
            name: "T".into(),
            layout,
        }
    }

    fn block(id: &str, terminal: &str) -> Value {
        json!({"id": id, "type": "block", "content": {"terminal": terminal}})
    }

    /// As herdr.app writes it, with things Native cannot show.
    fn app_document() -> Value {
        json!({"spaces": [{"id": "sp1", "name": "S", "folder": "/w", "active": "tb1", "extra": 1,
            "tabs": [{"id": "tb1", "name": "T", "focused": "p1", "before": ["p2"], "settled": null,
                "root": {"id": "tb1", "type": "row", "background": "#000", "children": [
                    block("p1", "t1"),
                    {"id": "m1", "type": "block", "content": {"mod": "viewer"}},
                ]}}]}],
            "unknown": {"kept": true}})
    }

    fn shown(document: &Value) -> Vec<ShownSpace> {
        shown_spaces(document, |_| true)
    }

    #[test]
    fn binary_splits_along_one_axis_become_one_row() {
        let layout = LayoutSnapshot::Split {
            direction: DirectionSnapshot::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Pane(1)),
            second: Box::new(LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(2)),
                second: Box::new(LayoutSnapshot::Split {
                    direction: DirectionSnapshot::Vertical,
                    ratio: 0.25,
                    first: Box::new(LayoutSnapshot::Pane(3)),
                    second: Box::new(LayoutSnapshot::Pane(9)),
                }),
            }),
        };
        let address = |pane: u32| (pane != 9).then(|| format!("t{pane}"));
        assert_eq!(
            from_binary(&layout, &address),
            Some(split(
                true,
                vec![(0.5, p("t1")), (0.25, p("t2")), (0.25, p("t3"))]
            ))
        );
        // herdr.app's weights compare equal to the same shares.
        let app = split(
            true,
            vec![
                (1.0, p("t1")),
                (2.0, split(true, vec![(1.0, p("t2")), (1.0, p("t3"))])),
            ],
        );
        let native = split(
            true,
            vec![(0.5, p("t1")), (0.333_333, p("t2")), (0.333_333, p("t3"))],
        );
        assert_eq!(
            canonical(&app),
            canonical(&split(
                true,
                vec![(1.0, p("t1")), (1.0, p("t2")), (1.0, p("t3"))]
            ))
        );
        assert_ne!(canonical(&app), canonical(&native));
    }

    #[test]
    fn nothing_changed_here_writes_nothing() {
        let mut document = app_document();
        let synced = shown(&document);
        assert!(!push_changes(
            &mut document,
            &synced,
            &synced,
            |_| true,
            ids()
        ));
        assert_eq!(document, app_document());
    }

    #[test]
    fn a_first_viewer_seeds_an_empty_document() {
        let mut document = Value::Null;
        let native = vec![space(
            "sp1",
            vec![tab(
                "tb1",
                split(false, vec![(0.25, p("t1")), (0.75, p("t2"))]),
            )],
        )];
        assert!(push_changes(&mut document, &[], &native, |_| true, ids()));
        let root = &document["spaces"][0]["tabs"][0]["root"];
        assert_eq!(root["id"], "tb1");
        assert_eq!(root["type"], "column");
        assert_eq!(root["children"][1]["content"]["terminal"], "t2");
        assert_eq!(root["children"][0]["size"]["weight"], 0.25);
        assert_eq!(document["spaces"][0]["folder"], "/w");
        // Read back, it is what was written: the next push writes nothing.
        let synced = shown(&document);
        assert_eq!(synced, native);
        assert!(!push_changes(
            &mut document,
            &synced,
            &native,
            |_| true,
            ids()
        ));
    }

    #[test]
    fn a_split_here_keeps_what_native_cannot_show() {
        let mut document = app_document();
        let base = shown(&document);
        let native = vec![space(
            "sp1",
            vec![tab(
                "tb1",
                split(true, vec![(0.5, p("t1")), (0.5, p("t2"))]),
            )],
        )];
        assert!(push_changes(&mut document, &base, &native, |_| true, ids()));
        let tab = &document["spaces"][0]["tabs"][0];
        assert_eq!(tab["focused"], "p1");
        assert_eq!(tab["before"], json!(["p2"]));
        assert_eq!(tab["root"]["id"], "tb1");
        assert_eq!(tab["root"]["background"], "#000");
        let members = tab["root"]["children"].as_array().unwrap();
        assert_eq!(members[0]["id"], "p1");
        assert_eq!(members[1]["content"]["terminal"], "t2");
        assert!(members.iter().any(|m| m["content"]["mod"] == "viewer"));
        assert_eq!(document["unknown"], json!({"kept": true}));
        assert_eq!(document["spaces"][0]["active"], "tb1");
        assert_eq!(shown(&document)[0].tabs[0].layout, native[0].tabs[0].layout);
    }

    #[test]
    fn a_change_elsewhere_survives_a_split_here() {
        let mut document = app_document();
        let base = shown(&document);
        // Another viewer added t9 to the tab after this one synced.
        document["spaces"][0]["tabs"][0]["root"]["children"]
            .as_array_mut()
            .unwrap()
            .push(block("p9", "t9"));
        let native = vec![space(
            "sp1",
            vec![tab(
                "tb1",
                split(false, vec![(0.5, p("t1")), (0.5, p("t2"))]),
            )],
        )];
        assert!(push_changes(&mut document, &base, &native, |_| true, ids()));
        let mut present = Vec::new();
        programs(&shown(&document)[0].tabs[0].layout, &mut present);
        present.sort();
        assert_eq!(present, ["t1", "t2", "t9"]);
    }

    #[test]
    fn closing_renaming_reshaping_and_moving_here() {
        let mut document = json!({"spaces": [{"id": "sp1", "name": "S", "folder": "/w", "tabs": [
            {"id": "tb1", "name": "T", "root": {"id": "tb1", "type": "row", "children": [
                {"id": "p1", "type": "block", "content": {"terminal": "t1"}, "size": {"weight": 1}},
                {"id": "p2", "type": "block", "content": {"terminal": "t2"}, "size": {"weight": 1}},
                {"id": "p3", "type": "block", "content": {"terminal": "t3"}, "size": {"weight": 1}},
            ]}},
            {"id": "tb2", "name": "T", "root": {"id": "tb2", "type": "row", "children": [block("p4", "t4")]}},
            {"id": "tb3", "name": "T", "root": {"id": "tb3", "type": "row", "children": [block("p5", "t5")]}},
        ]}]});
        let base = shown(&document);
        let mut renamed = tab("tb1", split(true, vec![(0.3, p("t1")), (0.7, p("t2"))]));
        renamed.name = "work".into();
        let mut native_space = space("sp1", vec![tab("tb3", p("t5")), renamed]);
        native_space.name = "Renamed".into();
        assert!(push_changes(
            &mut document,
            &base,
            &[native_space],
            |_| true,
            ids()
        ));
        let space = &document["spaces"][0];
        assert_eq!(space["name"], "Renamed");
        let tabs: Vec<&str> = space["tabs"].as_array().unwrap().iter().map(id).collect();
        assert_eq!(tabs, ["tb3", "tb1"]);
        let tb1 = &space["tabs"][1];
        assert_eq!(tb1["name"], "work");
        let members = tb1["root"]["children"].as_array().unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!((id(&members[0]), id(&members[1])), ("p1", "p2"));
        assert_eq!(members[0]["size"]["weight"], 0.3);
    }

    #[test]
    fn spaces_closed_here_go_and_ones_closed_elsewhere_stay_closed() {
        let mut document = json!({"spaces": [
            {"id": "sp1", "name": "S", "folder": "/w", "tabs": [
                {"id": "tb1", "name": "T", "root": {"id": "tb1", "type": "row", "children": [block("p1", "t1")]}}]},
            {"id": "sp2", "name": "S", "folder": "/w", "tabs": [
                {"id": "tb2", "name": "T", "root": {"id": "tb2", "type": "row", "children": [block("p2", "t2")]}}]},
        ]});
        let base = shown(&document);
        // sp2 closed here; sp1 was closed elsewhere meanwhile.
        document["spaces"].as_array_mut().unwrap().remove(0);
        let native = vec![space("sp1", vec![tab("tb1", p("t1"))])];
        assert!(push_changes(&mut document, &base, &native, |_| true, ids()));
        assert_eq!(document["spaces"], json!([]));
    }

    #[test]
    fn a_first_sync_keeps_the_document_and_adds_what_only_this_viewer_shows() {
        let document = vec![space(
            "sp1",
            vec![tab(
                "tb1",
                split(true, vec![(0.5, p("t1")), (0.5, p("t2"))]),
            )],
        )];
        let native = vec![space(
            "spX",
            vec![
                tab("tbX", split(false, vec![(0.5, p("t1")), (0.5, p("t3"))])),
                tab("tbY", p("t2")),
            ],
        )];
        let merged = union(&document, &native);
        assert_eq!(merged[0], document[0]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[1].id, "spX");
        assert_eq!(merged[1].tabs, vec![tab("tbX", p("t3"))]);
    }

    #[test]
    fn blocks_of_ended_programs_do_not_count_as_a_change_elsewhere() {
        let mut document = app_document();
        document["spaces"][0]["tabs"][0]["root"]["children"]
            .as_array_mut()
            .unwrap()
            .push(block("p8", "t8"));
        let live = |address: &str| address != "t8";
        let base = shown_spaces(&document, |address| live(address));
        let native = vec![space(
            "sp1",
            vec![tab(
                "tb1",
                split(true, vec![(0.4, p("t1")), (0.6, p("t2"))]),
            )],
        )];
        assert!(push_changes(&mut document, &base, &native, live, ids()));
        let members = document["spaces"][0]["tabs"][0]["root"]["children"]
            .as_array()
            .unwrap();
        // Rearranged (nobody else changed it); the ended program's block is gone.
        assert_eq!(members[0]["size"]["weight"], 0.4);
        assert!(!members.iter().any(|m| m["content"]["terminal"] == "t8"));
    }

    #[test]
    fn a_program_another_viewer_placed_meanwhile_is_not_added_twice() {
        let mut document = app_document();
        let base = shown(&document);
        // Both viewers adopt t7, a shell's program; the other one wrote first.
        document["spaces"][0]["tabs"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "tbO", "name": "", "root": {"id": "tbO", "type": "row", "children": [block("pO", "t7")]}}));
        let mut native = base.clone();
        native[0].tabs.push(tab("tbMine", p("t7")));
        assert!(!push_changes(
            &mut document,
            &base,
            &native,
            |_| true,
            ids()
        ));
        let tabs: Vec<&str> = document["spaces"][0]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .map(id)
            .collect();
        assert_eq!(tabs, ["tb1", "tbO"]);
    }

    #[test]
    fn a_space_this_viewer_knows_by_another_id_is_never_closed() {
        let mut document = app_document();
        let base = shown(&document);
        // The same program under ids this viewer made up: not a close.
        let native = vec![space("spMine", vec![tab("tbMine", p("t1"))])];
        push_changes(&mut document, &base, &native, |_| true, ids());
        let spaces: Vec<&str> = document["spaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(id)
            .collect();
        assert!(spaces.contains(&"sp1"));
        assert_eq!(document["spaces"][0]["tabs"][0]["id"], "tb1");
    }
}
