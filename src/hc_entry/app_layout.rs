//! herdr.app's shared layout document (`doc.get`/`doc.set` name `herdr-app`):
//! `spaces[].tabs[].root`, a tree of `row`/`column` containers and `block`s
//! whose `content.terminal` is a core address. herdr.app adopts a labeled
//! terminal missing from the tree by putting it beside the tab's last pane in
//! a row, so herdr-hc writes the splits it makes. These rules mirror
//! herdr-app's `layout.rs` (`split`, `remove`, `collapse`); fields this code
//! does not know are left untouched.

use serde_json::{json, Map, Value};

pub(crate) const DOCUMENT: &str = "herdr-app";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SplitPlacement {
    /// The tree changed and should be written.
    Placed,
    /// The new terminal already sits right beside the source along the axis.
    AlreadyPlaced,
    /// No tab shows the source terminal: nothing to place against.
    SourceMissing,
}

fn kind(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn is_line(node: &Value) -> bool {
    matches!(kind(node), "row" | "column")
}

fn children(node: &Value) -> Option<&Vec<Value>> {
    node.get("children").and_then(Value::as_array)
}

fn children_mut(node: &mut Value) -> Option<&mut Vec<Value>> {
    node.get_mut("children").and_then(Value::as_array_mut)
}

fn terminal(node: &Value) -> Option<&str> {
    (kind(node) == "block")
        .then(|| node.pointer("/content/terminal").and_then(Value::as_str))
        .flatten()
}

/// A share in a row/column: absent means 1, pixels are not a share.
fn weight(node: &Value) -> Option<f64> {
    match node.get("size") {
        None | Some(Value::Null) => Some(1.0),
        Some(size) => size.get("weight").and_then(Value::as_f64),
    }
}

fn set_weight(node: &mut Value, weight: f64) {
    if let Some(object) = node.as_object_mut() {
        object.insert("size".into(), json!({ "weight": weight }));
    }
}

fn contains_terminal(node: &Value, address: &str) -> bool {
    terminal(node) == Some(address)
        || children(node).is_some_and(|c| c.iter().any(|n| contains_terminal(n, address)))
}

fn count_terminal(node: &Value, address: &str) -> usize {
    usize::from(terminal(node) == Some(address))
        + children(node).map_or(0, |c| c.iter().map(|n| count_terminal(n, address)).sum())
}

fn tab_roots_mut(document: &mut Value) -> impl Iterator<Item = &mut Value> {
    document
        .get_mut("spaces")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(|space| space.get_mut("tabs").and_then(Value::as_array_mut))
        .flatten()
        .filter_map(|tab| tab.get_mut("root"))
}

/// Removes every block showing `address`; the share goes to the neighbor it
/// was split from (the one before it, else the next), as herdr.app does.
fn remove_terminal(node: &mut Value, address: &str) -> bool {
    let line = is_line(node);
    let Some(c) = children_mut(node) else {
        return false;
    };
    let mut removed = false;
    while let Some(i) = c.iter().position(|n| terminal(n) == Some(address)) {
        let gone = c.remove(i);
        removed = true;
        if line {
            if let (Some(share), Some(neighbor)) = (weight(&gone), c.get_mut(i.saturating_sub(1))) {
                if let Some(w) = weight(neighbor) {
                    set_weight(neighbor, w + share);
                }
            }
        }
    }
    for n in c.iter_mut() {
        removed |= remove_terminal(n, address);
    }
    removed
}

/// Rows and columns emptied by removals vanish; ones left with one child give
/// way to it, which takes over their size. Stack children keep identity.
fn collapse(node: &mut Value) {
    let stack = kind(node) == "stack";
    let Some(c) = children_mut(node) else {
        return;
    };
    for n in c.iter_mut() {
        collapse(n);
    }
    if stack {
        return;
    }
    c.retain(|n| !(is_line(n) && children(n).is_some_and(Vec::is_empty)));
    for n in c.iter_mut() {
        if is_line(n) && children(n).is_some_and(|c| c.len() == 1) {
            let size = n.get("size").cloned();
            let mut only = children_mut(n).map(|c| c.remove(0)).unwrap_or(Value::Null);
            if let Some(object) = only.as_object_mut() {
                match size {
                    Some(size) => object.insert("size".into(), size),
                    None => object.remove("size"),
                };
            }
            *n = only;
        }
    }
}

/// Puts `new` after the block showing `source`: inside its parent when that
/// already runs along `axis`, else in a new container wrapping the source.
/// `ratio` is the source's share of the space it had.
fn split_beside(
    node: &mut Value,
    source: &str,
    axis: &str,
    new: Value,
    ratio: f64,
    container_id: &str,
) -> Result<(), Value> {
    let along = kind(node) == axis;
    let Some(c) = children_mut(node) else {
        return Err(new);
    };
    if let Some(i) = c.iter().position(|n| terminal(n) == Some(source)) {
        let mut new = new;
        if along {
            let share = weight(&c[i]).unwrap_or(1.0);
            if c[i].get("size").and_then(|s| s.get("px")).is_none() {
                set_weight(&mut c[i], share * ratio);
            }
            set_weight(&mut new, share * (1.0 - ratio));
            c.insert(i + 1, new);
        } else {
            let mut old = c[i].clone();
            let slot = old.as_object_mut().and_then(|o| o.remove("size"));
            set_weight(&mut old, ratio);
            set_weight(&mut new, 1.0 - ratio);
            let mut wrapper = json!({ "id": container_id, "type": axis, "children": [old, new] });
            if let (Some(slot), Some(object)) = (slot, wrapper.as_object_mut()) {
                object.insert("size".into(), slot);
            }
            c[i] = wrapper;
        }
        return Ok(());
    }
    let mut new = new;
    for n in c.iter_mut() {
        match split_beside(n, source, axis, new, ratio, container_id) {
            Ok(()) => return Ok(()),
            Err(back) => new = back,
        }
    }
    Err(new)
}

/// Whether `new` is the next sibling of `source` in a container along `axis`.
fn placed_beside(node: &Value, source: &str, new: &str, axis: &str) -> bool {
    let Some(c) = children(node) else {
        return false;
    };
    (kind(node) == axis
        && c.windows(2)
            .any(|pair| terminal(&pair[0]) == Some(source) && terminal(&pair[1]) == Some(new)))
        || c.iter().any(|n| placed_beside(n, source, new, axis))
}

/// Records in herdr.app's layout that herdr-hc split `source` along a row
/// (`row`) or column and put `new` after it, the source keeping `ratio`.
pub(crate) fn place_split(
    document: &mut Value,
    source: &str,
    new: &str,
    row: bool,
    ratio: f64,
    mut fresh_id: impl FnMut(&str) -> String,
) -> SplitPlacement {
    let axis = if row { "row" } else { "column" };
    let ratio = if ratio.is_finite() && ratio > 0.0 && ratio < 1.0 {
        ratio
    } else {
        0.5
    };
    let total: usize = tab_roots_mut(document)
        .map(|root| count_terminal(root, new))
        .sum();
    let mut source_root = None;
    for (index, root) in tab_roots_mut(document).enumerate() {
        if contains_terminal(root, source) {
            if total == 1 && placed_beside(root, source, new, axis) {
                return SplitPlacement::AlreadyPlaced;
            }
            source_root = Some(index);
            break;
        }
    }
    let Some(index) = source_root else {
        return SplitPlacement::SourceMissing;
    };
    // herdr.app may already have adopted it elsewhere (beside the last pane).
    for root in tab_roots_mut(document) {
        if remove_terminal(root, new) {
            collapse(root);
        }
    }
    let block = json!({
        "id": fresh_id("p"),
        "type": "block",
        "content": { "terminal": new },
    });
    let container = fresh_id("s");
    let Some(root) = tab_roots_mut(document).nth(index) else {
        return SplitPlacement::SourceMissing;
    };
    if terminal(root) == Some(source) {
        // A tab whose root is the pane itself: wrap the root.
        let mut old = root.take();
        let slot = old.as_object_mut().and_then(|o| o.remove("size"));
        set_weight(&mut old, ratio);
        let mut block = block;
        set_weight(&mut block, 1.0 - ratio);
        let mut wrapper = Map::new();
        wrapper.insert("id".into(), container.into());
        wrapper.insert("type".into(), axis.into());
        wrapper.insert("children".into(), json!([old, block]));
        if let Some(slot) = slot {
            wrapper.insert("size".into(), slot);
        }
        *root = Value::Object(wrapper);
        return SplitPlacement::Placed;
    }
    match split_beside(root, source, axis, block, ratio, &container) {
        Ok(()) => SplitPlacement::Placed,
        Err(_) => SplitPlacement::SourceMissing,
    }
}

/// A herdr.app space as herdr-hc can show it on first open.
#[derive(Debug, PartialEq)]
pub(crate) struct ShownSpace {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) tabs: Vec<ShownTab>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ShownTab {
    pub(crate) name: String,
    pub(crate) layout: ShownLayout,
}

/// A tab's panes: programs side by side (`row`) or stacked, each with its share.
#[derive(Debug, PartialEq)]
pub(crate) enum ShownLayout {
    Program(String),
    Split {
        row: bool,
        parts: Vec<(f64, ShownLayout)>,
    },
}

fn text(node: &Value, key: &str) -> String {
    node.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn shown_layout(node: &Value, take: &mut impl FnMut(&str) -> bool) -> Option<ShownLayout> {
    match kind(node) {
        "block" => terminal(node)
            .filter(|address| take(address))
            .map(|address| ShownLayout::Program(address.to_owned())),
        "row" | "column" => {
            let mut parts: Vec<(f64, ShownLayout)> = children(node)?
                .iter()
                .filter_map(|child| {
                    let share = weight(child).filter(|w| w.is_finite() && *w > 0.0);
                    Some((share.unwrap_or(1.0), shown_layout(child, take)?))
                })
                .collect();
            match parts.len() {
                0 => None,
                1 => parts.pop().map(|(_, only)| only),
                _ => Some(ShownLayout::Split {
                    row: kind(node) == "row",
                    parts,
                }),
            }
        }
        // A stack shows one child at a time: herdr-hc shows that one. The
        // others' programs stay untaken and are shown elsewhere.
        "stack" => {
            let c = children(node)?;
            let active = node.get("active").and_then(Value::as_str);
            let shown = active.and_then(|id| {
                c.iter()
                    .find(|n| n.get("id").and_then(Value::as_str) == Some(id))
            });
            shown
                .and_then(|n| shown_layout(n, take))
                .or_else(|| c.iter().find_map(|n| shown_layout(n, take)))
        }
        _ => None,
    }
}

/// The spaces and tabs of herdr.app's document holding programs `take`
/// accepts (it is asked once per block and should accept an address once).
/// Empty tabs and spaces are left out; a malformed document gives none.
pub(crate) fn shown_spaces(
    document: &Value,
    mut take: impl FnMut(&str) -> bool,
) -> Vec<ShownSpace> {
    let Some(spaces) = document.get("spaces").and_then(Value::as_array) else {
        return Vec::new();
    };
    spaces
        .iter()
        .filter_map(|space| {
            let tabs: Vec<ShownTab> = space
                .get("tabs")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|tab| {
                    Some(ShownTab {
                        name: text(tab, "name"),
                        layout: shown_layout(tab.get("root")?, &mut take)?,
                    })
                })
                .collect();
            (!tabs.is_empty()).then(|| ShownSpace {
                id: text(space, "id"),
                name: text(space, "name"),
                tabs,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, terminal: &str) -> Value {
        json!({"id": id, "type": "block", "content": {"terminal": terminal}})
    }

    fn document(root: Value) -> Value {
        json!({"spaces": [{"id": "sp1", "name": "", "folder": "/", "theme": "kept",
            "tabs": [{"id": "tb1", "name": "", "root": root, "pinned": "kept"}]}],
            "unknown": {"kept": true}})
    }

    fn ids() -> impl FnMut(&str) -> String {
        let mut next = 0;
        move |prefix: &str| {
            next += 1;
            format!("{prefix}{next}")
        }
    }

    fn root(doc: &Value) -> &Value {
        &doc["spaces"][0]["tabs"][0]["root"]
    }

    #[test]
    fn a_split_down_of_a_lone_pane_wraps_it_in_a_column() {
        let mut doc = document(json!({"id": "r", "type": "row", "children": [pane("p0", "t3")]}));
        assert_eq!(
            place_split(&mut doc, "t3", "t13", false, 0.5, ids()),
            SplitPlacement::Placed
        );
        let column = &root(&doc)["children"][0];
        assert_eq!(column["type"], "column");
        assert_eq!(column["id"], "s2");
        assert_eq!(column["children"][0]["content"]["terminal"], "t3");
        assert_eq!(column["children"][1]["content"]["terminal"], "t13");
        assert_eq!(column["children"][1]["id"], "p1");
        assert_eq!(column["children"][0]["size"]["weight"], 0.5);
        // Everything this code does not know survives.
        assert_eq!(doc["unknown"]["kept"], true);
        assert_eq!(doc["spaces"][0]["theme"], "kept");
        assert_eq!(doc["spaces"][0]["tabs"][0]["pinned"], "kept");
    }

    #[test]
    fn splitting_again_along_the_same_axis_joins_the_existing_column() {
        let mut doc = document(json!({"id": "r", "type": "row", "children": [pane("p0", "t3")]}));
        place_split(&mut doc, "t3", "t13", false, 0.5, ids());
        place_split(&mut doc, "t13", "t14", false, 0.5, ids());
        let column = &root(&doc)["children"][0];
        let order: Vec<_> = column["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["content"]["terminal"].as_str().unwrap())
            .collect();
        assert_eq!(order, ["t3", "t13", "t14"]);
        assert_eq!(column["children"][1]["size"]["weight"], 0.25);
        assert_eq!(column["children"][2]["size"]["weight"], 0.25);
    }

    #[test]
    fn a_pane_herdr_app_already_adopted_beside_the_last_pane_is_moved() {
        // herdr.app put t13 in a row after t3; herdr-hc split t3 downward.
        let mut doc = document(json!({"id": "r", "type": "row",
            "children": [pane("p0", "t3"), pane("pX", "t13")]}));
        assert_eq!(
            place_split(&mut doc, "t3", "t13", false, 0.5, ids()),
            SplitPlacement::Placed
        );
        assert_eq!(count_terminal(root(&doc), "t13"), 1);
        // herdr.app's root row stays (it never collapses a tab root) and holds the column.
        assert_eq!(root(&doc)["id"], "r");
        let column = &root(&doc)["children"][0];
        assert_eq!(column["type"], "column");
        assert_eq!(column["children"][1]["content"]["terminal"], "t13");
        // Re-running finds it where it belongs and changes nothing.
        let before = doc.clone();
        assert_eq!(
            place_split(&mut doc, "t3", "t13", false, 0.5, ids()),
            SplitPlacement::AlreadyPlaced
        );
        assert_eq!(doc, before);
    }

    #[test]
    fn a_tab_whose_root_is_the_pane_itself_and_a_missing_source() {
        let mut doc = document(pane("p0", "t3"));
        assert_eq!(
            place_split(&mut doc, "t3", "t9", true, 0.6, ids()),
            SplitPlacement::Placed
        );
        assert_eq!(root(&doc)["type"], "row");
        assert_eq!(root(&doc)["children"][0]["size"]["weight"], 0.6);
        let mut other = document(pane("p0", "t3"));
        assert_eq!(
            place_split(&mut other, "t99", "t9", true, 0.5, ids()),
            SplitPlacement::SourceMissing
        );
        assert_eq!(other, document(pane("p0", "t3")));
        let mut empty = json!({});
        assert_eq!(
            place_split(&mut empty, "t3", "t9", true, 0.5, ids()),
            SplitPlacement::SourceMissing
        );
    }

    #[test]
    fn removing_gives_the_share_back_and_collapses_like_herdr_app() {
        let mut node = json!({"id": "r", "type": "row", "children": [
            {"id": "c", "type": "column", "size": {"weight": 2.0},
             "children": [pane("a", "t1"), pane("b", "t2")]},
            pane("d", "t3"),
        ]});
        assert!(remove_terminal(&mut node, "t2"));
        collapse(&mut node);
        assert_eq!(node["children"][0]["content"]["terminal"], "t1");
        assert_eq!(node["children"][0]["size"]["weight"], 2.0);
    }

    fn taking<'a>(live: &'a [&'a str]) -> impl FnMut(&str) -> bool + 'a {
        let mut left: std::collections::HashSet<&str> = live.iter().copied().collect();
        move |address| left.remove(address)
    }

    #[test]
    fn spaces_and_tabs_keep_their_shape_with_only_shown_programs() {
        // As herdr.app writes it: each tab root is a row named after the tab.
        let doc = json!({"spaces": [
            {"id": "sp1", "name": "Test", "folder": "/w", "tabs": [
                {"id": "tb1", "name": "tab 1", "root": {"id": "tb1", "type": "row", "children": [
                    {"id": "p1", "type": "block", "content": {"terminal": "t1"}, "size": {"weight": 3.0}},
                    {"id": "s1", "type": "column", "children": [
                        pane("p2", "t2"),
                        pane("p3", "t9"),
                        {"id": "p4", "type": "block", "content": {"mod": "viewer"}},
                    ]},
                ]}},
                {"id": "tb2", "name": "gone", "root": {"id": "tb2", "type": "row",
                    "children": [pane("p5", "t9")]}},
            ]},
            {"id": "sp2", "name": "empty", "folder": "/", "tabs": []},
            {"id": "sp3", "name": "", "folder": "", "tabs": [
                {"id": "tb3", "name": "", "root": {"id": "tb3", "type": "row", "children": [
                    pane("p6", "t1"),
                    {"id": "p7", "type": "block", "content": {"terminal": "t3"}, "size": {"px": 300}},
                ]}},
            ]},
        ]});
        let spaces = shown_spaces(&doc, taking(&["t1", "t2", "t3"]));
        assert_eq!(
            spaces,
            vec![
                ShownSpace {
                    id: "sp1".into(),
                    name: "Test".into(),
                    tabs: vec![ShownTab {
                        name: "tab 1".into(),
                        layout: ShownLayout::Split {
                            row: true,
                            parts: vec![
                                (3.0, ShownLayout::Program("t1".into())),
                                (1.0, ShownLayout::Program("t2".into())),
                            ],
                        },
                    }],
                },
                // t1 is shown once, so this tab keeps only t3 (pixel sizes count as one share).
                ShownSpace {
                    id: "sp3".into(),
                    name: String::new(),
                    tabs: vec![ShownTab {
                        name: String::new(),
                        layout: ShownLayout::Program("t3".into()),
                    }],
                },
            ]
        );
    }

    #[test]
    fn a_stack_shows_its_active_child_and_leaves_the_rest_untaken() {
        let doc = document(json!({"id": "tb1", "type": "row", "children": [
            {"id": "k", "type": "stack", "active": "p2",
                "children": [pane("p1", "t1"), pane("p2", "t2")]},
        ]}));
        let mut taken = Vec::new();
        let spaces = shown_spaces(&doc, |address| {
            taken.push(address.to_owned());
            true
        });
        assert_eq!(taken, ["t2"]);
        assert_eq!(spaces[0].tabs[0].layout, ShownLayout::Program("t2".into()));
    }

    #[test]
    fn a_document_without_spaces_shows_nothing() {
        for doc in [Value::Null, json!({"spaces": 3}), json!([1])] {
            assert!(shown_spaces(&doc, |_| true).is_empty());
        }
    }
}
