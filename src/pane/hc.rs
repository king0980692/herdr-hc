//! hc attachment IO for the existing pane parser/renderer. No PTY/process ownership.

use super::*;
use crate::hc::{
    self as api,
    state::{RuntimeSnapshot, TerminalBinding},
    wire,
};
use serde_json::{json, Value};
use tokio::sync::watch;

const INPUT_QUEUE: usize = 32;

enum Command {
    Input(Bytes, bool),
    Resize {
        rows: u16,
        cols: u16,
        cell: (u32, u32),
        responses: Vec<Bytes>,
    },
    Focus(bool),
    Theme(Box<crate::terminal_theme::TerminalTheme>),
    Submission {
        text: Bytes,
        enter: Bytes,
        delay: std::time::Duration,
        deadline: Option<std::time::Instant>,
        reply: std::sync::mpsc::Sender<io::Result<()>>,
    },
}

struct PendingSubmission {
    enter: Bytes,
    due: tokio::time::Instant,
    deadline: Option<std::time::Instant>,
    reply: std::sync::mpsc::Sender<io::Result<()>>,
}

pub(super) struct HcIo {
    client: api::Client,
    commands: mpsc::Sender<Command>,
    updates: watch::Receiver<RuntimeSnapshot>,
    accepting: Arc<AtomicBool>,
    task: tokio::task::AbortHandle,
    executor: tokio::runtime::Handle,
    event_gate: Arc<AtomicBool>,
}

struct ConnectionGuard {
    updates: watch::Sender<RuntimeSnapshot>,
    accepting: Arc<AtomicBool>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        self.updates.send_modify(|state| {
            state.connected = false;
            state.attached = false;
        });
    }
}

impl HcIo {
    fn start(
        client: api::Client,
        initial: RuntimeSnapshot,
        stream: api::Stream,
        events: api::Events,
        projection: api::state::Projection,
        display: Value,
        on_read: impl Fn(&[u8]) -> PtyReadResult + Send + 'static,
        event_gate: Arc<AtomicBool>,
    ) -> Self {
        let (commands, receive) = mpsc::channel(INPUT_QUEUE);
        let (updates, state) = watch::channel(initial);
        let accepting = Arc::new(AtomicBool::new(true));
        let guard = ConnectionGuard {
            updates,
            accepting: accepting.clone(),
        };
        let task_client = client.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = drive(
                stream, events, projection, receive, display, &guard, on_read,
            )
            .await
            {
                guard
                    .updates
                    .send_modify(|state| state.error = Some(error.to_string()));
                tracing::warn!(%error, core = %task_client.hello.core.id, "hc attachment unavailable; program was not killed");
            }
            drop(guard);
        });
        Self {
            client,
            commands,
            updates: state,
            accepting,
            task: task.abort_handle(),
            executor: tokio::runtime::Handle::current(),
            event_gate,
        }
    }

    pub(super) fn executor(&self) -> tokio::runtime::Handle {
        self.executor.clone()
    }

    pub(super) fn snapshot(&self) -> RuntimeSnapshot {
        self.updates.borrow().clone()
    }
    pub(super) fn updates(&self) -> watch::Receiver<RuntimeSnapshot> {
        self.updates.clone()
    }

    pub(super) fn cwd(&self, foreground: bool) -> Option<std::path::PathBuf> {
        let state = self.updates.borrow();
        let facts = &state.terminal.facts;
        let cwd = if foreground {
            facts.get("foreground")?.get("cwd")?.as_str()?
        } else {
            facts.get("cwd")?.as_str()?
        };
        (!cwd.is_empty()).then(|| cwd.into())
    }

    pub(super) fn shutdown(&self) {
        self.accepting.store(false, Ordering::Release);
        self.event_gate.store(false, Ordering::Release);
        self.task.abort(); // Dropping the streams sends CLOSE, never terminal.kill.
    }

    #[cfg(test)]
    pub(super) async fn terminate(&self) -> io::Result<()> {
        let address = self.updates.borrow().binding.address.clone();
        self.client
            .call("terminal.kill", json!({"terminal":address}))
            .await?;
        Ok(())
    }

    pub(super) fn request_terminate(&self) {
        let client = self.client.clone();
        let address = self.updates.borrow().binding.address.clone();
        // Explicit synchronous close keeps its kill RPC on the source executor after detach.
        self.executor.spawn(async move {
            if let Err(error) = client
                .call("terminal.kill", json!({"terminal":address}))
                .await
            {
                tracing::warn!(%error, %address, "explicit hc terminal termination failed");
            }
        });
    }

    pub(super) fn try_send_bytes(
        &self,
        bytes: Bytes,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        if !self.accepting.load(Ordering::Acquire) || bytes.len() > wire::MAX_PAYLOAD {
            return Err(mpsc::error::TrySendError::Closed(bytes));
        }
        self.commands
            .try_send(Command::Input(bytes, false))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(Command::Input(bytes, _)) => {
                    // A stalled source would otherwise take some keys and drop
                    // others, leaving the program a line with holes in it.
                    // Detach instead: every later key is refused until recovery
                    // reattaches, and nothing still queued is delivered late.
                    self.shutdown();
                    tracing::warn!(
                        "hc input queue unavailable; detached rather than dropping part of the input"
                    );
                    mpsc::error::TrySendError::Full(bytes)
                }
                mpsc::error::TrySendError::Closed(Command::Input(bytes, _)) => {
                    mpsc::error::TrySendError::Closed(bytes)
                }
                _ => unreachable!("the queued command is always input"),
            })
    }

    fn control(&self, command: Command) -> bool {
        if self.commands.try_send(command).is_ok() {
            return true;
        }
        // Losing a resize/query reply silently would desynchronize viewer and core.
        self.shutdown();
        tracing::warn!("hc control queue unavailable; detached rather than dropping a control");
        false
    }

    pub(super) fn automatic(&self, response: impl FnOnce() -> Option<Bytes>) {
        if let Some(bytes) = response() {
            self.control(Command::Input(bytes, true));
        }
    }

    pub(super) fn resize(
        &self,
        rows: u16,
        cols: u16,
        width: u32,
        height: u32,
        responses: Vec<Bytes>,
    ) {
        self.control(Command::Resize {
            rows,
            cols,
            cell: (width, height),
            responses,
        });
    }

    pub(super) fn focus(&self, focused: bool) -> bool {
        self.control(Command::Focus(focused))
    }
    pub(super) fn theme(&self, theme: crate::terminal_theme::TerminalTheme) {
        self.control(Command::Theme(Box::new(theme)));
    }

    pub(super) fn submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: std::time::Duration,
        deadline: Option<std::time::Instant>,
    ) -> io::Result<std::sync::mpsc::Receiver<io::Result<()>>> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if std::time::Instant::now().checked_add(delay).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hc submission delay exceeds clock range",
            ));
        }
        if text.len() > wire::MAX_PAYLOAD || enter.len() > wire::MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hc input exceeds 16 MiB",
            ));
        }
        let (reply, receive) = std::sync::mpsc::channel();
        self.commands
            .try_send(Command::Submission {
                text,
                enter,
                delay,
                deadline,
                reply,
            })
            .map_err(|error| io::Error::new(io::ErrorKind::WouldBlock, error.to_string()))?;
        Ok(receive)
    }
}

impl Drop for HcIo {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "hc terminal attachment unavailable",
    )
}

async fn drive(
    mut stream: api::Stream,
    mut events: api::Events,
    mut projection: api::state::Projection,
    mut commands: mpsc::Receiver<Command>,
    mut display: Value,
    guard: &ConnectionGuard,
    on_read: impl Fn(&[u8]) -> PtyReadResult,
) -> io::Result<()> {
    let address = guard.updates.borrow().binding.address.clone();
    let mut output_open = true;
    let mut pending: Option<PendingSubmission> = None;
    loop {
        tokio::select! {
            frame = stream.recv(), if output_open => {
                let frame = frame?;
                match frame.kind {
                    wire::DATA => {
                        let responses = on_read(&frame.payload).terminal_responses;
                        for response in responses { stream.input(response, true).await?; }
                        stream.consumed(frame.payload.len()).await?;
                    }
                    wire::CREDIT => {}
                    wire::CLOSE => {
                        // shortcut: expose loss without respawn; reattach when App restore owns hc bindings.
                        output_open = false;
                        guard.accepting.store(false, Ordering::Release);
                        guard.updates.send_modify(|state| state.attached = false);
                    }
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidData,"unexpected hc viewer frame")),
                }
            }
            notification = events.next() => {
                let change = projection.notification(1, notification?)?;
                if change == api::state::Change::NeedsSnapshot {
                    return Err(io::Error::new(io::ErrorKind::InvalidData,"hc event projection requires a fresh snapshot"));
                }
                if change != api::state::Change::Ignored && change != api::state::Change::Document {
                    if let Some(terminal) = projection.terminals.get(&address) {
                        if terminal.facts.get("state").and_then(Value::as_str) == Some("finished") || !terminal.facts.contains_key("pid") {
                            guard.accepting.store(false, Ordering::Release);
                        }
                        guard.updates.send_modify(|state| {
                            state.terminal = terminal.clone();
                            state.attached = output_open && guard.accepting.load(Ordering::Acquire);
                        });
                    } else {
                        guard.accepting.store(false, Ordering::Release);
                        guard.updates.send_modify(|state| { state.attached = false; state.error = Some("hc terminal was removed".into()); });
                    }
                }
            }
            _ = async {
                if let Some(pending) = &pending { tokio::time::sleep_until(pending.due).await; }
            }, if pending.is_some() => {
                let Some(submission) = pending.take() else { continue; };
                let result = if !guard.accepting.load(Ordering::Acquire) {
                    Err(unavailable())
                } else if submission.deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                    Err(io::Error::new(io::ErrorKind::TimedOut,"hc input submission deadline expired"))
                } else {
                    stream.input(submission.enter, false).await
                };
                let _ = submission.reply.send(result);
            }
            command = commands.recv(), if pending.is_none() => {
                let Some(command) = command else { return Ok(()); };
                match command {
                    Command::Input(bytes, automatic) => {
                        if !guard.accepting.load(Ordering::Acquire) { return Err(unavailable()); }
                        stream.input(bytes, automatic).await?;
                    }
                    Command::Resize { rows, cols, cell, responses } => {
                        display["cell"] = if cell.0 > 0 && cell.1 > 0 { json!([cell.0,cell.1]) } else { Value::Null };
                        stream.display(display.clone()).await?;
                        stream.resize(cols, rows).await?;
                        for response in responses { stream.input(response, true).await?; }
                    }
                    Command::Focus(focused) => stream.focus(focused).await?,
                    Command::Theme(theme) => {
                        let colors = display_facts(*theme);
                        display["fg"] = colors["fg"].clone();
                        display["bg"] = colors["bg"].clone();
                        stream.display(display.clone()).await?;
                    }
                    Command::Submission { text, enter, delay, deadline, reply } => {
                        if !guard.accepting.load(Ordering::Acquire) {
                            let _ = reply.send(Err(unavailable()));
                            continue;
                        }
                        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                            let _ = reply.send(Err(io::Error::new(io::ErrorKind::TimedOut,"hc input submission deadline expired")));
                            continue;
                        }
                        if let Err(error) = stream.input(text, false).await {
                            let _ = reply.send(Err(io::Error::new(error.kind(),error.to_string())));
                            return Err(error);
                        }
                        let Some(enter_at) = std::time::Instant::now().checked_add(delay) else {
                            let _ = reply.send(Err(io::Error::new(io::ErrorKind::InvalidInput,"hc submission delay exceeds clock range")));
                            continue;
                        };
                        // Hold later human commands, not output/events or parser replies.
                        let due = deadline.map_or(enter_at, |deadline| deadline.min(enter_at)).into();
                        pending = Some(PendingSubmission { enter, due, deadline, reply });
                    }
                }
            }
        }
        if pending.is_some() && !guard.accepting.load(Ordering::Acquire) {
            if let Some(submission) = pending.take() {
                let _ = submission.reply.send(Err(unavailable()));
            }
        }
    }
}

fn display_facts(theme: crate::terminal_theme::TerminalTheme) -> Value {
    let color = |color: Option<crate::terminal_theme::RgbColor>| {
        color.map(|rgb| format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b))
    };
    json!({"fg":color(theme.foreground),"bg":color(theme.background),"cursor":null,"cell":null})
}

impl PaneRuntime {
    // Native attachments use the same geometry, parser, and render hooks as PTY construction.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn attach_hc(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        client: api::Client,
        address: &str,
        history: usize,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
        committed: bool,
    ) -> io::Result<Self> {
        let rows = rows.max(MIN_PANE_ROWS);
        let cols = cols.max(MIN_PANE_COLS);
        let (snapshot, subscription) = client.subscribe(json!({"terminal":address})).await?;
        if snapshot.terminals.len() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "hc terminal subscription did not resolve one terminal",
            ));
        }
        let mut projection = api::state::Projection::default();
        projection.begin(client.hello.core.id.clone(), 1);
        let record = snapshot.terminals[0].clone();
        // Attaching a view must not resize an existing program before its viewport is active.
        let rows = record
            .facts
            .get("rows")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value >= MIN_PANE_ROWS)
            .unwrap_or(rows);
        let cols = record
            .facts
            .get("cols")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value >= MIN_PANE_COLS)
            .unwrap_or(cols);
        let binding = TerminalBinding {
            core_id: client.hello.core.id.clone(),
            address: record.address.clone(),
        };
        let requested = TerminalBinding {
            core_id: binding.core_id.clone(),
            address: address.to_owned(),
        };
        if requested.validate().is_ok() && binding.address != requested.address {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "hc returned a different canonical terminal",
            ));
        }
        projection.snapshot(1, snapshot)?;
        let display = display_facts(host_terminal_theme);
        let (_, stream) = client
            .open(
                "terminal.attach",
                json!({"terminal":binding.address,
            "cols":cols,"rows":rows,"history":history,"display":display}),
            )
            .await?;

        let (response_tx, _) = mpsc::channel::<Bytes>(1);
        let mut parser = crate::ghostty::Terminal::new(cols, rows, scrollback_limit_bytes)
            .map_err(|error| io::Error::other(error.to_string()))?;
        if crate::kitty_graphics::is_enabled() {
            parser
                .enable_kitty_graphics()
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        let pane_terminal = GhosttyPaneTerminal::new(parser, response_tx.clone())?;
        pane_terminal.apply_host_terminal_theme(host_terminal_theme);
        let _ = pane_terminal.apply_host_terminal_appearance(host_terminal_appearance);
        let terminal = Arc::new(PaneTerminal::new(pane_terminal));
        let compression = TerminalCompressionTask::spawn(pane_id, terminal.clone());
        let content_seq = Arc::new(AtomicU64::new(0));
        let content_write_lock = Arc::new(Mutex::new(()));
        let event_gate = Arc::new(AtomicBool::new(committed));
        let on_read = {
            let event_gate = event_gate.clone();
            let terminal = terminal.clone();
            let content_seq = content_seq.clone();
            let content_write_lock = content_write_lock.clone();
            let compression_wake = compression.notifier();
            move |bytes: &[u8]| {
                let lock = content_write_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                content_seq.fetch_add(1, Ordering::AcqRel);
                let result = terminal.process_pty_bytes(pane_id, 0, bytes, &response_tx);
                content_seq.fetch_add(1, Ordering::Release);
                drop(lock);
                compression_wake.wake();
                let committed = event_gate.load(Ordering::Acquire);
                if committed && (result.terminal_bells != 0 || !result.clipboard_writes.is_empty())
                {
                    if let Err(error) = events.try_send(AppEvent::HcTerminalEffects {
                        enabled: event_gate.clone(),
                        pane_id,
                        bells: result.terminal_bells,
                        clipboard_writes: result.clipboard_writes,
                    }) {
                        tracing::warn!(%error,"failed to queue hc terminal effects");
                    }
                }
                let title = committed
                    && result.terminal_title_changed
                    && render_dirty.request_terminal_title(pane_id);
                let render =
                    committed && result.request_render && render_dirty.request_pty(pane_id);
                if title || render {
                    render_notify.notify_one();
                }
                if let Some(delay) = result.render_delay.filter(|_| committed) {
                    let notify = render_notify.clone();
                    let dirty = render_dirty.clone();
                    let event_gate = event_gate.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        if event_gate.load(Ordering::Acquire) && dirty.request_pty(pane_id) {
                            notify.notify_one();
                        }
                    });
                }
                PtyReadResult {
                    terminal_responses: result.terminal_responses,
                }
            }
        };
        let initial = RuntimeSnapshot {
            binding,
            terminal: record,
            connected: true,
            attached: true,
            error: None,
        };
        let io = HcIo::start(
            client,
            initial,
            stream,
            subscription,
            projection,
            display,
            on_read,
            event_gate,
        );
        Ok(Self {
            pane_id,
            terminal,
            io: PaneRuntimeIo::Hc(io),
            current_size: Cell::new((rows, cols, 0, 0)),
            child_pid: Arc::new(AtomicU32::new(0)),
            reported_cwd: Arc::new(Mutex::new(None)),
            persistence_cwd: Mutex::new(None),
            cwd_process_exited: Arc::new(AtomicBool::new(false)),
            child_wait_completed: None,
            kitty_keyboard_flags: Arc::new(AtomicU16::new(0)),
            content_seq,
            content_write_lock,
            detection_content_seq: Arc::new(AtomicU64::new(0)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            self_reported_agent_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: Arc::new(Notify::new()),
            pending_release: Arc::new(Mutex::new(None)),
            preserve_processes_on_drop: true,
            compression,
            detect_handle: None,
        })
    }

    pub(crate) fn hc_commit_events(&self, enabled: bool) {
        if let PaneRuntimeIo::Hc(io) = &self.io {
            io.event_gate.store(enabled, Ordering::Release);
        }
    }

    pub(crate) fn hc_executor(&self) -> Option<tokio::runtime::Handle> {
        match &self.io {
            PaneRuntimeIo::Hc(io) => Some(io.executor()),
            _ => None,
        }
    }

    pub(crate) fn hc_snapshot(&self) -> Option<RuntimeSnapshot> {
        match &self.io {
            PaneRuntimeIo::Hc(io) => Some(io.snapshot()),
            _ => None,
        }
    }

    pub(crate) fn hc_updates(&self) -> Option<watch::Receiver<RuntimeSnapshot>> {
        match &self.io {
            PaneRuntimeIo::Hc(io) => Some(io.updates()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) async fn terminate_hc(&self) -> io::Result<()> {
        match &self.io {
            PaneRuntimeIo::Hc(io) => io.terminate().await,
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "not an hc runtime",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hc_command_queue_keeps_input_slots_small() {
        assert!(std::mem::size_of::<super::Command>() <= 256);
    }
}
