//! The full-screen half of `knaix top`.
//!
//! Everything here draws; nothing here fetches. The parent module hands over
//! snapshots and log lines, which keeps the view free of the question of where
//! a node lives and lets `--json` and the pipe case share the same readings.
//!
//! Fetches run as tasks and answer on channels, so a node that has gone away
//! and is timing out cannot stop the view from redrawing or from taking a
//! keypress. That is the difference between a live view and one that freezes on
//! the node the reader most wants to look away from.

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};
use std::sync::Arc;
use std::time::Duration;

use super::{
    carry_forward, fetch_logs, percent_cell, snapshot, LogTail, NodeRow, Options, Reach, Snapshot,
    DEEP_EVERY,
};
use crate::nodes::KnaixContext;

/// What the view is showing right now.
struct State {
    snapshot: Snapshot,
    /// Selection is held by node id, not by index. Rows are re-sorted on every
    /// snapshot, and an index would quietly move the selection to another node.
    selected: Option<String>,
    logs: LogTail,
    log_error: Option<String>,
    paused: bool,
    /// A line for whatever just happened, cleared by the next thing.
    status: Option<String>,
    quit: bool,
}

impl State {
    fn selected_row(&self) -> Option<&NodeRow> {
        let id = self.selected.as_ref()?;
        self.snapshot.nodes.iter().find(|n| &n.id == id)
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.snapshot.nodes.iter().position(|n| &n.id == id)
    }

    /// Move the selection, clamped at both ends. Wrapping in a list this short
    /// is disorienting: holding an arrow key should stop, not cycle.
    fn move_selection(&mut self, delta: isize) {
        if self.snapshot.nodes.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let last = self.snapshot.nodes.len() as isize - 1;
        let next = (current + delta).clamp(0, last) as usize;
        self.selected = Some(self.snapshot.nodes[next].id.clone());
    }
}

/// What the loop is waiting on.
enum Wake {
    Key(KeyEvent),
    Snapshot(Box<Snapshot>),
    Logs(String, Result<Vec<String>, String>),
    Tick,
}

pub async fn run(ctx: &KnaixContext, opts: Options, interval: Duration) -> Result<()> {
    let ctx = Arc::new(ctx.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Wake>();

    spawn_input_reader(tx.clone());
    spawn_ticker(tx.clone(), interval);

    let mut state = State {
        snapshot: Snapshot {
            taken_at: String::new(),
            nodes: Vec::new(),
            hosted_unavailable: None,
        },
        selected: opts.node_id.clone(),
        logs: LogTail::new(String::new(), opts.log_lines),
        log_error: None,
        paused: false,
        status: Some("Loading...".to_string()),
        quit: false,
    };

    // Installs a panic hook that restores the terminal before the CLI's own
    // hook records the crash, so the message lands on the normal screen.
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(
        &mut terminal,
        &ctx,
        &mut state,
        &tx,
        &mut rx,
        opts.log_lines,
    )
    .await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    ctx: &Arc<KnaixContext>,
    state: &mut State,
    tx: &tokio::sync::mpsc::UnboundedSender<Wake>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Wake>,
    log_lines: usize,
) -> Result<()> {
    let mut tick: u32 = 0;
    let mut snapshot_in_flight = false;
    let mut logs_in_flight = false;

    // Draw before the first fetch returns, so the view appears immediately
    // rather than after a round trip to every node the account owns.
    terminal.draw(|frame| draw(frame, state))?;
    request_snapshot(ctx, tx, true, &mut snapshot_in_flight);

    while let Some(wake) = rx.recv().await {
        match wake {
            Wake::Key(key) => handle_key(key, state, ctx, tx, log_lines, &mut logs_in_flight),
            Wake::Snapshot(fresh) => {
                snapshot_in_flight = false;
                absorb_snapshot(state, *fresh);
                request_logs(ctx, tx, state, log_lines, &mut logs_in_flight);
            }
            Wake::Logs(node, result) => {
                logs_in_flight = false;
                absorb_logs(state, node, result);
            }
            Wake::Tick => {
                tick = tick.wrapping_add(1);
                // Skip a tick whose predecessor has not answered. Stacking
                // fetches on a slow control plane would spend the interval
                // queueing requests instead of showing their results.
                if !snapshot_in_flight {
                    request_snapshot(
                        ctx,
                        tx,
                        tick.is_multiple_of(DEEP_EVERY),
                        &mut snapshot_in_flight,
                    );
                }
            }
        }

        if state.quit {
            break;
        }
        terminal.draw(|frame| draw(frame, state))?;
    }

    Ok(())
}

fn handle_key(
    key: KeyEvent,
    state: &mut State,
    ctx: &Arc<KnaixContext>,
    tx: &tokio::sync::mpsc::UnboundedSender<Wake>,
    log_lines: usize,
    logs_in_flight: &mut bool,
) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => state.quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => state.quit = true,
        KeyCode::Up | KeyCode::Char('k') => {
            state.move_selection(-1);
            state.status = None;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.move_selection(1);
            state.status = None;
        }
        KeyCode::Char('p') => {
            state.paused = !state.paused;
            state.status = Some(
                if state.paused {
                    "Logs paused"
                } else {
                    "Logs resumed"
                }
                .to_string(),
            );
        }
        KeyCode::Char('r') => {
            state.status = Some("Refreshing...".to_string());
            request_logs(ctx, tx, state, log_lines, logs_in_flight);
        }
        _ => {}
    }
}

/// Take a fresh snapshot, keeping what a shallow pass could not measure and
/// holding the selection on the node it was on.
fn absorb_snapshot(state: &mut State, mut fresh: Snapshot) {
    carry_forward(&mut fresh, &state.snapshot);
    state.snapshot = fresh;
    state.status = None;

    let still_there = state
        .selected
        .as_ref()
        .is_some_and(|id| state.snapshot.nodes.iter().any(|n| &n.id == id));
    if !still_there {
        state.selected = state.snapshot.nodes.first().map(|n| n.id.clone());
    }
}

fn absorb_logs(state: &mut State, node: String, result: Result<Vec<String>, String>) {
    // A late answer for a node the reader has already moved on from would
    // append another node's output under this one's heading.
    if state.selected.as_deref() != Some(node.as_str()) {
        return;
    }
    match result {
        Ok(lines) => {
            state.log_error = None;
            state.logs.absorb(&lines);
        }
        Err(err) => state.log_error = Some(err),
    }
}

fn request_snapshot(
    ctx: &Arc<KnaixContext>,
    tx: &tokio::sync::mpsc::UnboundedSender<Wake>,
    deep: bool,
    in_flight: &mut bool,
) {
    let ctx = Arc::clone(ctx);
    let tx = tx.clone();
    *in_flight = true;
    tokio::spawn(async move {
        if let Ok(fresh) = snapshot(&ctx, deep).await {
            let _ = tx.send(Wake::Snapshot(Box::new(fresh)));
        }
    });
}

fn request_logs(
    ctx: &Arc<KnaixContext>,
    tx: &tokio::sync::mpsc::UnboundedSender<Wake>,
    state: &mut State,
    log_lines: usize,
    in_flight: &mut bool,
) {
    if state.paused || *in_flight {
        return;
    }
    let Some(row) = state.selected_row().cloned() else {
        return;
    };

    // Selecting another node starts its pane empty rather than appending its
    // output to the last node's.
    if state.logs.node != row.id {
        state.logs = LogTail::new(row.id.clone(), log_lines);
        state.log_error = None;
    }

    let ctx = Arc::clone(ctx);
    let tx = tx.clone();
    *in_flight = true;
    tokio::spawn(async move {
        let result = fetch_logs(&ctx, &row, log_lines)
            .await
            .map_err(|err| err.to_string());
        let _ = tx.send(Wake::Logs(row.id, result));
    });
}

/// Keys are read on a thread of their own: `event::read` blocks, and blocking
/// the runtime would stop every fetch the view has in flight.
fn spawn_input_reader(tx: tokio::sync::mpsc::UnboundedSender<Wake>) {
    std::thread::spawn(move || loop {
        match event::read() {
            // Windows reports press and release; acting on both would move the
            // selection twice per keystroke.
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                if tx.send(Wake::Key(key)).is_err() {
                    return;
                }
            }
            // A resize is a reason to redraw and nothing more: ratatui measures
            // the new area itself on the next draw.
            Ok(Event::Resize(_, _)) => {
                if tx.send(Wake::Tick).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    });
}

fn spawn_ticker(tx: tokio::sync::mpsc::UnboundedSender<Wake>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if tx.send(Wake::Tick).is_err() {
                return;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw(frame: &mut Frame, state: &State) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Percentage(45),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_header(frame, areas[0], state);
    draw_nodes(frame, areas[1], state);
    draw_bottom(frame, areas[2], state);
    draw_footer(frame, areas[3], state);
}

fn draw_header(frame: &mut Frame, area: Rect, state: &State) {
    let mut spans = vec![
        Span::styled(
            "knaix top",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{} nodes", state.snapshot.nodes.len()),
            Style::default().fg(Color::Gray),
        ),
    ];

    if let Some(reason) = &state.snapshot.hosted_unavailable {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("hosted unavailable: {reason}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(status) = &state.status {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            status.clone(),
            Style::default().fg(Color::Gray),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_nodes(frame: &mut Frame, area: Rect, state: &State) {
    if state.snapshot.nodes.is_empty() {
        let message = if state.snapshot.taken_at.is_empty() {
            "Looking for nodes..."
        } else {
            "No nodes. 'knaix local up' runs one on this machine with no account."
        };
        frame.render_widget(
            Paragraph::new(message).block(Block::default().borders(Borders::ALL).title(" Nodes ")),
            area,
        );
        return;
    }

    let rows: Vec<Row> = state
        .snapshot
        .nodes
        .iter()
        .map(|node| {
            Row::new(vec![
                Cell::from(node.name.clone()),
                Cell::from(if node.local { "local" } else { "hosted" }),
                Cell::from(status_span(node)),
                Cell::from(node.tier.clone().unwrap_or_else(|| "-".to_string())),
                Cell::from(percent_cell(node.cpu)),
                Cell::from(percent_cell(node.memory)),
                Cell::from(
                    node.documents
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                ),
                Cell::from(node.peers.to_string()),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(12),
            Constraint::Length(7),
            Constraint::Length(22),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
        ],
    )
    .header(
        Row::new(vec![
            "Node", "Where", "Status", "Tier", "CPU", "Mem", "Docs", "Peers",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(" Nodes "))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut table_state = TableState::default();
    table_state.select(state.selected_index());
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn draw_bottom(frame: &mut Frame, area: Rect, state: &State) {
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(30), Constraint::Length(32)])
        .split(area);

    draw_logs(frame, panes[0], state);
    draw_detail(frame, panes[1], state);
}

fn draw_logs(frame: &mut Frame, area: Rect, state: &State) {
    let title = match (&state.selected, state.paused) {
        (Some(node), true) => format!(" Logs: {node} (paused) "),
        (Some(node), false) => format!(" Logs: {node} "),
        (None, _) => " Logs ".to_string(),
    };

    let body: Vec<Line> = if let Some(err) = &state.log_error {
        vec![Line::styled(
            err.clone(),
            Style::default().fg(Color::Yellow),
        )]
    } else if state.logs.lines.is_empty() {
        vec![Line::styled(
            "No log lines yet.",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        // The pane shows the end of the log, which is the part that is moving.
        let height = area.height.saturating_sub(2) as usize;
        state
            .logs
            .lines
            .iter()
            .skip(state.logs.lines.len().saturating_sub(height))
            .map(|line| Line::raw(line.clone()))
            .collect()
    };

    frame.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_detail(frame: &mut Frame, area: Rect, state: &State) {
    let Some(node) = state.selected_row() else {
        frame.render_widget(
            Paragraph::new("").block(Block::default().borders(Borders::ALL).title(" Node ")),
            area,
        );
        return;
    };

    let status = super::status_cell(node);
    let mut body = vec![
        field("id", &node.id),
        field("state", &node.state),
        field("status", &status),
        field("last seen", node.last_seen.as_deref().unwrap_or("never")),
    ];

    if let Some(uuid) = &node.uuid {
        body.push(field("instance", uuid));
    }

    body.push(Line::raw(""));
    body.push(Line::styled(
        format!("mesh peers ({})", node.peers),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if node.peers == 0 {
        body.push(Line::styled("  none", Style::default().fg(Color::DarkGray)));
    }

    frame.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(" Node ")),
        area,
    );
}

fn field<'a>(label: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &State) {
    let pause = if state.paused { "resume" } else { "pause" };
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("q quit   up/down select   p {pause} logs   r refresh"),
            Style::default().fg(Color::DarkGray),
        )),
        area,
    );
}

/// The status cell, coloured. Unreachable carries its reason: a row that only
/// says "unreachable" sends the reader to another command to find out why.
fn status_span(node: &NodeRow) -> Span<'static> {
    match &node.reach {
        Reach::Ok { latency_ms } => Span::styled(
            format!("up {latency_ms}ms"),
            Style::default().fg(Color::Green),
        ),
        Reach::Unreachable { reason } => Span::styled(
            format!("unreachable ({reason})"),
            Style::default().fg(Color::Red),
        ),
        Reach::Unknown => Span::styled("...", Style::default().fg(Color::DarkGray)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(nodes: Vec<&str>) -> State {
        State {
            snapshot: Snapshot {
                taken_at: "t".to_string(),
                nodes: nodes.iter().map(|n| node_named(n)).collect(),
                hosted_unavailable: None,
            },
            selected: nodes.first().map(|n| n.to_string()),
            logs: LogTail::new(String::new(), 10),
            log_error: None,
            paused: false,
            status: None,
            quit: false,
        }
    }

    fn node_named(id: &str) -> NodeRow {
        NodeRow {
            id: id.to_string(),
            uuid: None,
            name: id.to_string(),
            local: false,
            state: "running".to_string(),
            tier: None,
            reach: Reach::Unknown,
            cpu: None,
            memory: None,
            documents: None,
            peers: 0,
            last_seen: None,
        }
    }

    /// Holding an arrow key should stop at the end of the list, not cycle back
    /// to the top past the node the reader was heading for.
    #[test]
    fn selection_clamps_at_both_ends() {
        let mut state = state_with(vec!["a", "b", "c"]);
        state.move_selection(-1);
        assert_eq!(state.selected.as_deref(), Some("a"));

        state.move_selection(1);
        state.move_selection(1);
        state.move_selection(1);
        assert_eq!(state.selected.as_deref(), Some("c"));
    }

    /// Rows re-sort on every snapshot. Selection is held by id so it stays on
    /// the node the reader chose rather than on whatever moved into that slot.
    #[test]
    fn selection_follows_the_node_not_the_row_position() {
        let mut state = state_with(vec!["a", "b", "c"]);
        state.move_selection(1);
        assert_eq!(state.selected.as_deref(), Some("b"));

        let mut reordered = Snapshot {
            taken_at: "t2".to_string(),
            nodes: vec![node_named("c"), node_named("b"), node_named("a")],
            hosted_unavailable: None,
        };
        carry_forward(&mut reordered, &state.snapshot);
        absorb_snapshot(&mut state, reordered);

        assert_eq!(state.selected.as_deref(), Some("b"));
        assert_eq!(state.selected_index(), Some(1));
    }

    /// A node that disappears cannot keep the selection, or the detail pane
    /// points at nothing and the log pane never updates again.
    #[test]
    fn a_vanished_node_hands_the_selection_on() {
        let mut state = state_with(vec!["a", "b"]);
        state.move_selection(1);
        assert_eq!(state.selected.as_deref(), Some("b"));

        absorb_snapshot(
            &mut state,
            Snapshot {
                taken_at: "t2".to_string(),
                nodes: vec![node_named("a")],
                hosted_unavailable: None,
            },
        );
        assert_eq!(state.selected.as_deref(), Some("a"));
    }

    /// An empty list must not panic or select a row that is not there.
    #[test]
    fn an_empty_list_has_nothing_to_select() {
        let mut state = state_with(vec![]);
        state.move_selection(1);
        assert_eq!(state.selected, None);
        assert_eq!(state.selected_index(), None);
    }

    // --- drawing ---

    /// Render one frame at a given size and return it as text.
    ///
    /// This drives the real `draw`, so anything that would panic on a terminal
    /// of this size panics here instead of in front of a user.
    fn render(state: &State, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, state)).expect("draw");

        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_frame_shows_the_nodes_and_their_load() {
        let mut state = state_with(vec!["alpha", "beta"]);
        state.snapshot.nodes[0].reach = Reach::Ok { latency_ms: 12 };
        state.snapshot.nodes[0].cpu = Some(37.0);
        state.snapshot.nodes[0].documents = Some(9);
        state.snapshot.nodes[0].tier = Some("Personal".to_string());

        let frame = render(&state, 100, 24);
        assert!(frame.contains("knaix top"), "{frame}");
        assert!(frame.contains("alpha"), "{frame}");
        assert!(frame.contains("beta"), "{frame}");
        assert!(frame.contains("up 12ms"), "{frame}");
        assert!(frame.contains("37%"), "{frame}");
        assert!(frame.contains("Personal"), "{frame}");
        assert!(frame.contains("q quit"), "{frame}");
    }

    /// The reason travels with the status. A row that only says "unreachable"
    /// sends the reader to another command to find out why.
    #[test]
    fn an_unreachable_node_shows_why_on_its_row() {
        let mut state = state_with(vec!["alpha"]);
        state.snapshot.nodes[0].reach = Reach::Unreachable {
            reason: "timed out".to_string(),
        };

        let frame = render(&state, 100, 24);
        assert!(frame.contains("timed out"), "{frame}");
    }

    /// A user with no nodes gets the command that gives them one, not an empty
    /// frame that looks like a hung fetch.
    #[test]
    fn an_empty_mesh_says_how_to_get_a_node() {
        let mut state = state_with(vec![]);
        state.snapshot.taken_at = "t".to_string();

        let frame = render(&state, 100, 24);
        assert!(frame.contains("knaix local up"), "{frame}");
    }

    /// Terminals get resized to absurd sizes mid-session, and a layout that
    /// panics on a narrow one takes the whole command down with it.
    #[test]
    fn drawing_survives_a_terminal_of_any_size() {
        let mut state = state_with(vec!["alpha", "beta"]);
        state.snapshot.nodes[0].reach = Reach::Ok { latency_ms: 3 };
        state.logs = LogTail::new("alpha".to_string(), 50);
        state
            .logs
            .absorb(&["a log line that is considerably wider than a narrow terminal".to_string()]);

        for (width, height) in [(1, 1), (2, 3), (10, 5), (40, 10), (200, 60), (300, 4)] {
            render(&state, width, height);
        }
    }

    /// The pane shows the end of the log, which is the part that is moving.
    #[test]
    fn the_log_pane_shows_the_newest_lines() {
        let mut state = state_with(vec!["alpha"]);
        state.logs = LogTail::new("alpha".to_string(), 500);
        let lines: Vec<String> = (0..200).map(|n| format!("line-{n}")).collect();
        state.logs.absorb(&lines);

        let frame = render(&state, 100, 24);
        assert!(frame.contains("line-199"), "the newest line is missing");
        assert!(!frame.contains("line-0 "), "the oldest line is still shown");
    }

    /// A log answer that arrives after the reader moved on belongs to the node
    /// it was asked of, and appending it here would mix two nodes' output.
    #[test]
    fn a_late_log_answer_for_another_node_is_dropped() {
        let mut state = state_with(vec!["a", "b"]);
        state.logs = LogTail::new("a".to_string(), 10);
        absorb_logs(&mut state, "b".to_string(), Ok(vec!["from b".to_string()]));
        assert!(state.logs.lines.is_empty(), "another node's logs landed");

        absorb_logs(&mut state, "a".to_string(), Ok(vec!["from a".to_string()]));
        assert_eq!(state.logs.lines.len(), 1);
    }
}
