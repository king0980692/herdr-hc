//! Pure hc event projection. No PTYs, screen detection, IO, or Herdr wire revisions.

use serde::Deserialize;
use serde_json::{Map, Value};
use std::{collections::HashMap, io};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Terminal {
    pub address: String,
    // Keep upstream facts losslessly, including status records unknown to this client.
    #[serde(flatten)]
    pub facts: Map<String, Value>,
}

/// Durable runtime identity; neither machine aliases nor PIDs identify a core.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct TerminalBinding {
    pub core_id: String,
    pub address: String,
}

impl TerminalBinding {
    pub fn validate(&self) -> io::Result<()> {
        // Generation 1 counts t1..t9, t10..t19, t1b..t1z; these are not decimal IDs.
        let canonical = self
            .address
            .strip_prefix('t')
            .and_then(|local| {
                let mut symbols = local.bytes();
                let first = symbols
                    .next()?
                    .checked_sub(b'1')
                    .filter(|digit| *digit < 9)?;
                let (mut value, mut before, mut size) = (u64::from(first), 0u64, 9u64);
                for symbol in symbols {
                    let digit = b"0123456789bcdfghjkmnpqrstvwxyz"
                        .iter()
                        .position(|byte| *byte == symbol)? as u64;
                    value = value.checked_mul(30)?.checked_add(digit)?;
                    before = before.checked_add(size)?;
                    size = size.checked_mul(30)?;
                }
                u32::try_from(before.checked_add(value)?.checked_add(1)?).ok()
            })
            .is_some();
        if self.core_id.trim().is_empty() || !canonical {
            return Err(io::Error::new(io::ErrorKind::InvalidData,"invalid hc binding: expected permanent core id and canonical local terminal address"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeSnapshot {
    pub binding: TerminalBinding,
    pub terminal: Terminal,
    pub connected: bool,
    pub attached: bool,
    pub error: Option<String>,
}

impl RuntimeSnapshot {
    /// A program exit is distinct from core shutdown, transport loss, and agent status.
    /// It is final whether or not the terminal is kept: a kept terminal's loss
    /// stays `live` awaiting a new shell, but an exit with a code finishes it
    /// (hc schema, "keeps a lost terminal live"). herdr.app keeps every
    /// terminal it adopts, so its panes must close on exit too.
    pub fn is_program_exit(&self) -> bool {
        self.connected
            && self.terminal.facts.get("state").and_then(Value::as_str) == Some("finished")
            && self.terminal.facts.get("reason").and_then(Value::as_str) == Some("exited")
    }

    pub fn reported_status(&self) -> Option<&Value> {
        if !self.connected
            || !self.attached
            || self.terminal.facts.get("reported") != Some(&Value::Bool(true))
        {
            return None;
        }
        self.terminal
            .facts
            .get("status")
            .filter(|status| status.is_object())
    }
}

#[derive(Debug, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub terminals: Vec<Terminal>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Ignored,
    Terminal(String),
    Snapshot,
    /// Document data is opaque; its owner must refetch it after this notice.
    Document,
    /// A filtered event referred to an unknown record without a replacement.
    NeedsSnapshot,
}

/// Projection of an unfiltered events subscription; presentation filtering stays local.
#[derive(Debug, Default)]
pub struct Projection {
    pub core_id: String,
    pub terminals: HashMap<String, Terminal>,
    epoch: u64,
    seq: Option<u64>,
}

impl Projection {
    /// Called with a NEW local epoch before subscribing/reconnecting. hc seq resets
    /// on restart and is never a Herdr snapshot revision or connection generation.
    pub fn begin(&mut self, core_id: String, epoch: u64) {
        if self.core_id != core_id {
            self.terminals.clear();
        }
        self.core_id = core_id;
        self.epoch = epoch;
        self.seq = None;
    }

    pub fn snapshot(&mut self, epoch: u64, snapshot: Snapshot) -> io::Result<Change> {
        if epoch != self.epoch {
            return Ok(Change::Ignored);
        }
        if self.seq.is_some_and(|seq| snapshot.seq < seq) {
            return Ok(Change::Ignored);
        }
        let mut terminals = HashMap::new();
        for terminal in snapshot.terminals {
            if terminal.address.is_empty()
                || terminals
                    .insert(terminal.address.clone(), terminal)
                    .is_some()
            {
                return Err(invalid("hc snapshot has an empty or duplicate address"));
            }
        }
        self.terminals = terminals;
        self.seq = Some(snapshot.seq);
        Ok(Change::Snapshot)
    }

    pub fn notification(&mut self, epoch: u64, notification: Value) -> io::Result<Change> {
        if epoch != self.epoch {
            return Ok(Change::Ignored);
        }
        match notification.get("method").and_then(Value::as_str) {
            Some("heartbeat") => return Ok(Change::Ignored),
            Some("missed") => {
                let snapshot: Snapshot =
                    serde_json::from_value(notification["params"].clone()).map_err(json_error)?;
                return self.snapshot(epoch, snapshot);
            }
            Some("event") => {}
            _ => return Ok(Change::Ignored),
        }
        let event = &notification["params"];
        let seq = event
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("hc event lacks a seq"))?;
        let Some(previous) = self.seq else {
            return Ok(Change::NeedsSnapshot);
        };
        if seq <= previous {
            return Ok(Change::Ignored);
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("hc event lacks a type"))?;
        let data = &event["data"];
        if kind == "doc" {
            self.seq = Some(seq);
            return Ok(Change::Document);
        }
        let Some(address) = event.get("terminal").and_then(Value::as_str) else {
            // Future non-terminal event kinds are safe to ignore.
            self.seq = Some(seq);
            return Ok(Change::Ignored);
        };
        match kind {
            "created" | "restarted" => {
                let terminal: Terminal =
                    serde_json::from_value(data.clone()).map_err(json_error)?;
                if terminal.address != address || address.is_empty() {
                    return Err(invalid("hc event and record addresses disagree"));
                }
                self.terminals.insert(address.to_owned(), terminal);
            }
            "removed" => {
                self.terminals.remove(address);
            }
            "lost" | "finished" | "status" | "turn" | "title" | "labels" | "size"
            | "foreground" | "keep" => {
                if kind == "labels" && !self.terminals.contains_key(address) {
                    if let Some(record) = data.get("record") {
                        let terminal: Terminal =
                            serde_json::from_value(record.clone()).map_err(json_error)?;
                        if terminal.address != address {
                            return Err(invalid("hc label record address disagrees"));
                        }
                        self.terminals.insert(address.to_owned(), terminal);
                    }
                }
                let Some(terminal) = self.terminals.get_mut(address) else {
                    return Ok(Change::NeedsSnapshot);
                };
                match kind {
                    "foreground" => {
                        if !data.is_null() && !data.is_object() {
                            return Err(invalid("invalid hc foreground"));
                        }
                        terminal.facts.insert("foreground".to_owned(), data.clone());
                    }
                    "lost" | "finished" => {
                        let fields = data
                            .as_object()
                            .ok_or_else(|| invalid("hc lifecycle data must be an object"))?;
                        terminal.facts.extend(fields.clone());
                        terminal.facts.remove("foreground");
                        if kind == "lost" {
                            terminal.facts.remove("pid");
                        }
                        terminal.facts.insert(
                            "state".to_owned(),
                            Value::from(if kind == "lost" { "live" } else { "finished" }),
                        );
                    }
                    "status" => copy_fields(terminal, data, &["status", "records", "reported"])?,
                    // `turn.state` is not a new status or Herdr completion watermark.
                    "turn" => copy_fields(terminal, data, &["turn"])?,
                    "title" => copy_fields(terminal, data, &["title"])?,
                    "labels" => copy_fields(terminal, data, &["labels"])?,
                    "keep" => copy_fields(terminal, data, &["keep"])?,
                    "size" => {
                        for field in ["cols", "rows"] {
                            let dimension = data.get(field).and_then(Value::as_u64);
                            if !dimension.is_some_and(|n| n > 0 && n <= u64::from(u16::MAX)) {
                                return Err(invalid("invalid hc size event"));
                            }
                        }
                        copy_fields(terminal, data, &["cols", "rows"])?;
                    }
                    _ => {}
                }
            }
            _ => {
                self.seq = Some(seq);
                return Ok(Change::Ignored);
            }
        }
        self.seq = Some(seq);
        Ok(Change::Terminal(address.to_owned()))
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn copy_fields(terminal: &mut Terminal, data: &Value, names: &[&str]) -> io::Result<()> {
    if names.iter().any(|name| data.get(*name).is_none()) {
        return Err(invalid("hc event data is missing a required field"));
    }
    for name in names {
        terminal
            .facts
            .insert((*name).to_owned(), data[*name].clone());
    }
    Ok(())
}
