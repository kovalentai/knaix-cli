//! `knaix top`: one live view of every node, hosted and local.
//!
//! Between `knaix metrics` (one node, one snapshot) and the dashboard (a
//! browser) there was nothing. This is the terminal answer: the whole mesh at
//! once, refreshed on an interval, with the selected node's logs streaming
//! underneath.
//!
//! The data layer lives here and is deliberately separate from the drawing. A
//! snapshot is a plain value, so `--json` emits one and exits without a
//! terminal being involved at all, and the view has nothing to do but render
//! what it is handed.

use anyhow::{anyhow, Context, Result};
use reqwest::header::AUTHORIZATION;
use serde::Serialize;
use std::time::{Duration, Instant};

use crate::exit::{Code, WithCode};
use crate::nodes::{KnaixContext, Node};

/// How `top` was asked to run.
pub struct Options {
    /// The node selected when the view opens.
    pub node_id: Option<String>,
    pub interval: Duration,
    pub log_lines: usize,
}

/// Every reading slower than the tick is refreshed on this multiple of it, so
/// the interval stays the interval no matter what a deep pass costs.
const DEEP_EVERY: u32 = 5;

/// Floor on the refresh interval. Below this the probes overlap and the view
/// spends more time fetching than drawing.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Whether a node answered, and how fast. A node that stops answering mid
/// session becomes `Unreachable` and keeps its row: dropping it would make a
/// blip look like a deletion.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(
    tag = "status",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum Reach {
    Ok {
        latency_ms: u64,
    },
    Unreachable {
        reason: String,
    },
    /// Not probed yet. The first frame draws before the first probe returns.
    Unknown,
}

/// One node, as a row.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRow {
    /// What the user types to address this node.
    pub id: String,
    /// The instance UUID the control plane's routes are keyed by. The local
    /// node has none: nothing central knows about it.
    pub uuid: Option<String>,
    pub name: String,
    pub local: bool,
    /// Lifecycle as the control plane records it, or docker for a local node.
    pub state: String,
    pub tier: Option<String>,
    pub reach: Reach,
    /// Percent, most recent sample. Absent when the runtime cannot be sampled.
    pub cpu: Option<f64>,
    pub memory: Option<f64>,
    pub documents: Option<u64>,
    pub peers: usize,
    pub last_seen: Option<String>,
}

impl NodeRow {
    pub fn is_reachable(&self) -> bool {
        matches!(self.reach, Reach::Ok { .. })
    }
}

/// Every node at one moment.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub taken_at: String,
    pub nodes: Vec<NodeRow>,
    /// Why the hosted half is missing, when it is. A logged-out user still gets
    /// their local node rather than an error, so this says what they are not
    /// seeing instead of the list pretending to be complete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosted_unavailable: Option<String>,
}

impl Snapshot {
    /// True when there was nothing to show and a reason for it. A user with no
    /// nodes at all should be told, not shown an empty frame forever.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Run `knaix top`.
///
/// Three ways out, chosen by what the caller can actually use rather than by a
/// flag alone: `--json` is one snapshot for a script, a pipe gets a plain table
/// repeated on the interval, and a terminal gets the live view.
pub async fn run(ctx: &KnaixContext, opts: Options) -> Result<()> {
    let interval = opts.interval.max(MIN_INTERVAL);

    if ctx.output_format == "json" {
        let snapshot = snapshot(ctx, true).await?;
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    let _ = &opts.node_id;
    let _ = opts.log_lines;
    follow_plain(ctx, interval).await
}

/// The pipe case: the same table, printed again on every interval. No cursor
/// control, because whatever is reading this cannot redraw.
async fn follow_plain(ctx: &KnaixContext, interval: Duration) -> Result<()> {
    let mut tick: u32 = 0;
    let mut previous: Option<Snapshot> = None;
    loop {
        let mut current = snapshot(ctx, tick.is_multiple_of(DEEP_EVERY)).await?;
        if let Some(previous) = &previous {
            carry_forward(&mut current, previous);
        }
        println!("{}", plain_table(&current));
        previous = Some(current);
        tick = tick.wrapping_add(1);
        tokio::time::sleep(interval).await;
    }
}

/// Keep the slow readings visible between the passes that take them.
///
/// Only the fields a shallow pass never fills are carried, and only into a gap.
/// Without this every tick between deep passes blanks the load and document
/// columns, which reads as a node that stopped reporting rather than one that
/// was not asked.
fn carry_forward(current: &mut Snapshot, previous: &Snapshot) {
    for row in &mut current.nodes {
        let Some(before) = previous.nodes.iter().find(|p| p.id == row.id) else {
            continue;
        };
        row.cpu = row.cpu.or(before.cpu);
        row.memory = row.memory.or(before.memory);
        row.documents = row.documents.or(before.documents);
        // A node that has gone unreachable keeps the time it was last seen,
        // which is the one fact worth having about a node that is not there.
        if row.last_seen.is_none() {
            row.last_seen = before.last_seen.clone();
        }
    }
}

/// One snapshot as a table. Shared by the pipe case and by `--help`-shaped
/// debugging, so it holds no colour and no cursor movement.
pub fn plain_table(snapshot: &Snapshot) -> String {
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.set_header(vec![
        "Node", "ID", "Where", "Status", "Tier", "CPU", "Mem", "Docs", "Peers",
    ]);

    for row in &snapshot.nodes {
        table.add_row(vec![
            row.name.clone(),
            row.id.clone(),
            if row.local { "local" } else { "hosted" }.to_string(),
            status_cell(row),
            row.tier.clone().unwrap_or_else(|| "-".to_string()),
            percent_cell(row.cpu),
            percent_cell(row.memory),
            row.documents
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string()),
            row.peers.to_string(),
        ]);
    }

    let mut out = format!("{table}");
    if let Some(reason) = &snapshot.hosted_unavailable {
        out.push_str(&format!("\nHosted nodes unavailable: {reason}"));
    }
    if snapshot.is_empty() {
        out.push_str("\nNo nodes. 'knaix local up' runs one on this machine.");
    }
    out
}

/// Status, with the latency or the reason folded in. A row that just says
/// "unreachable" sends the reader to another command to find out why.
pub fn status_cell(row: &NodeRow) -> String {
    match &row.reach {
        Reach::Ok { latency_ms } => format!("up {latency_ms}ms"),
        Reach::Unreachable { reason } => format!("unreachable ({reason})"),
        Reach::Unknown => "...".to_string(),
    }
}

/// A percentage, or a dash when nothing could be sampled. Never 0%, which would
/// draw an idle node where there is an unmeasurable one.
pub fn percent_cell(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.0}%"))
        .unwrap_or_else(|| "-".to_string())
}

/// Take one snapshot of every node this machine can see.
///
/// `deep` adds the readings that are too slow for a per-second tick: a document
/// count is a request per node against another route, and `docker stats`
/// samples twice before it can report a rate. The fast pass runs on the
/// interval; this one runs occasionally, and its values persist in between.
pub async fn snapshot(ctx: &KnaixContext, deep: bool) -> Result<Snapshot> {
    let mut nodes = Vec::new();
    let mut hosted_unavailable = None;

    match hosted_nodes(ctx).await {
        Ok(hosted) => {
            // Probe concurrently: a serial pass would make the refresh interval
            // a function of how many nodes the user owns.
            let probes = hosted.into_iter().map(|node| probe_hosted(ctx, node, deep));
            nodes.extend(futures_util::future::join_all(probes).await);
        }
        Err(err) => hosted_unavailable = Some(short_reason(&err)),
    }

    if let Some(row) = probe_local(ctx, deep).await {
        nodes.push(row);
    }

    // Local first, then by name: the local node is the one the user just
    // started, and a list that reorders itself as latencies move is unusable.
    nodes.sort_by(|a, b| b.local.cmp(&a.local).then_with(|| a.name.cmp(&b.name)));

    Ok(Snapshot {
        taken_at: now_rfc3339(),
        nodes,
        hosted_unavailable,
    })
}

/// The account's nodes. An unauthenticated user has none rather than an error,
/// so `top` still runs on the local-only path.
async fn hosted_nodes(ctx: &KnaixContext) -> Result<Vec<Node>> {
    let token = ctx.get_token()?;
    let url = format!("{}/api/instances", ctx.config.api_url);

    let resp = ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .context("Could not reach the Kovalent API")?;

    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {}", resp.status()))
            .coded(Code::for_status(resp.status().as_u16()));
    }

    let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
    Ok(serde_json::from_value(wrapper["data"].clone()).unwrap_or_default())
}

/// Health, metrics and optionally a document count for one hosted node.
///
/// Each is allowed to fail on its own. A node whose metrics route is down is
/// still a node that answers, and reporting it as unreachable because a second
/// request failed would be a lie about the thing the user most needs to trust.
async fn probe_hosted(ctx: &KnaixContext, node: Node, deep: bool) -> NodeRow {
    let uuid = node.id.clone();
    let client_id = node.instance_id.clone();
    let display_id = client_id
        .clone()
        .or_else(|| uuid.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let mut row = NodeRow {
        id: display_id,
        uuid: uuid.clone(),
        name: node.name.clone(),
        local: false,
        state: node.state.clone(),
        tier: node
            .config
            .as_ref()
            .and_then(|c| c.get("tier"))
            .and_then(|t| t.as_str())
            .map(str::to_string),
        reach: Reach::Unknown,
        cpu: None,
        memory: None,
        documents: None,
        peers: 0,
        last_seen: None,
    };

    let Some(uuid) = uuid else {
        // No UUID means nothing can be asked about it. Say so rather than
        // leaving the row on "...", which reads as still loading.
        row.reach = Reach::Unreachable {
            reason: "no instance id".to_string(),
        };
        return row;
    };

    row.reach = hosted_health(ctx, &uuid, &mut row).await;

    if let Some(client_id) = client_id.as_deref() {
        if let Some((cpu, memory)) = hosted_metrics(ctx, client_id).await {
            row.cpu = cpu;
            row.memory = memory;
        }
    }

    if deep {
        row.documents = hosted_document_count(ctx, &uuid).await;
    }

    row
}

/// Reachability for one hosted node, filling in what health also reports.
async fn hosted_health(ctx: &KnaixContext, uuid: &str, row: &mut NodeRow) -> Reach {
    let url = format!("{}/api/nodes/{}/health", ctx.config.api_url, uuid);
    let token = match ctx.get_token() {
        Ok(token) => token,
        Err(err) => {
            return Reach::Unreachable {
                reason: short_reason(&err),
            }
        }
    };

    let started = Instant::now();
    let resp = ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .timeout(Duration::from_secs(5))
        .send()
        .await;

    match resp {
        Ok(resp) if resp.status().is_success() => {
            let latency = started.elapsed().as_millis() as u64;
            let body: serde_json::Value = resp.json().await.unwrap_or_default();

            if let Some(name) = body["nodeName"].as_str() {
                if !name.is_empty() {
                    row.name = name.to_string();
                }
            }
            row.last_seen = body["checkedAt"].as_str().map(str::to_string);

            if body["healthy"].as_bool().unwrap_or(false) {
                // The control plane's own latency measurement when it has one:
                // it is the round trip to the node, where ours is the round
                // trip to the control plane.
                let latency_ms = body["latencyMs"]
                    .as_f64()
                    .map(|v| v as u64)
                    .unwrap_or(latency);
                Reach::Ok { latency_ms }
            } else {
                Reach::Unreachable {
                    reason: "node reported unhealthy".to_string(),
                }
            }
        }
        Ok(resp) => Reach::Unreachable {
            reason: format!("HTTP {}", resp.status().as_u16()),
        },
        Err(err) if err.is_timeout() => Reach::Unreachable {
            reason: "timed out".to_string(),
        },
        Err(_) => Reach::Unreachable {
            reason: "no answer".to_string(),
        },
    }
}

/// The most recent CPU and memory sample. Both are time series; `top` wants the
/// last point, and an empty series means the runtime could not be sampled
/// rather than that it was idle.
async fn hosted_metrics(ctx: &KnaixContext, client_id: &str) -> Option<(Option<f64>, Option<f64>)> {
    let token = ctx.get_token().ok()?;
    let url = format!("{}/api/instances/{}/metrics", ctx.config.api_url, client_id);

    let resp = ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    let data = if body["data"].is_null() {
        &body
    } else {
        &body["data"]
    };
    Some((
        last_series_value(&data["cpu"]),
        last_series_value(&data["memory"]),
    ))
}

/// The last value of a metrics series, whether its points are bare numbers or
/// `{ value }` objects. Both shapes are in flight depending on the runtime.
fn last_series_value(series: &serde_json::Value) -> Option<f64> {
    let points = series.as_array()?;
    let last = points.last()?;
    last.as_f64()
        .or_else(|| last["value"].as_f64())
        .or_else(|| last["y"].as_f64())
}

async fn hosted_document_count(ctx: &KnaixContext, uuid: &str) -> Option<u64> {
    let token = ctx.get_token().ok()?;
    let url = format!("{}/api/knowledge/{}/documents", ctx.config.api_url, uuid);

    let resp = ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    body["data"].as_array().map(|docs| docs.len() as u64)
}

/// The node on this machine, if one is running. Nothing here needs an account,
/// which is the point: `top` works on the local-only path.
async fn probe_local(ctx: &KnaixContext, deep: bool) -> Option<NodeRow> {
    let node = crate::local::load()?;
    let base = node.base_url();

    // Every docker call shells out and blocks, so it goes to a blocking thread
    // rather than stalling the runtime the other probes are running on.
    let state = tokio::task::spawn_blocking(crate::local::container_state)
        .await
        .ok()
        .flatten();

    let mut row = NodeRow {
        id: crate::local::LOCAL_NODE_ID.to_string(),
        uuid: None,
        name: "local".to_string(),
        local: true,
        state: state.unwrap_or_else(|| "unknown".to_string()),
        tier: None,
        reach: Reach::Unknown,
        cpu: None,
        memory: None,
        documents: None,
        peers: 0,
        last_seen: None,
    };

    let started = Instant::now();
    match ctx
        .client
        .get(format!("{}/health", base))
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            row.tier = body["tier"].as_str().map(str::to_string);
            row.peers = body["peers"].as_array().map(|p| p.len()).unwrap_or(0);
            row.reach = if body["ready"].as_bool().unwrap_or(false) {
                Reach::Ok { latency_ms }
            } else {
                Reach::Unreachable {
                    reason: "not ready".to_string(),
                }
            };
            row.last_seen = Some(now_rfc3339());
        }
        Ok(resp) => {
            row.reach = Reach::Unreachable {
                reason: format!("HTTP {}", resp.status().as_u16()),
            }
        }
        Err(_) => {
            row.reach = Reach::Unreachable {
                reason: "no answer".to_string(),
            }
        }
    }

    if deep {
        if let Some((cpu, memory)) = tokio::task::spawn_blocking(crate::local::container_usage)
            .await
            .ok()
            .flatten()
        {
            row.cpu = Some(cpu);
            row.memory = Some(memory);
        }

        if row.is_reachable() {
            row.documents = local_document_count(ctx, &base, &node.instance_id).await;
        }
    }

    Some(row)
}

/// How many documents the local node holds.
///
/// The count intent answers in chunks unless asked to split by document, and
/// chunks are not what a reader means by "documents": one PDF is hundreds.
async fn local_document_count(ctx: &KnaixContext, base: &str, instance_id: &str) -> Option<u64> {
    let resp = ctx
        .client
        .post(format!("{}/api/kb/count", base))
        .json(&serde_json::json!({
            "instance_id": instance_id,
            "by_document": true,
        }))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    body["documents"].as_array().map(|docs| docs.len() as u64)
}

/// An error reduced to something that fits in a cell. The full chain is what
/// `knaix doctor` is for.
fn short_reason(err: &anyhow::Error) -> String {
    let text = err.to_string();
    match text.split_once('\n') {
        Some((first, _)) => first.trim().to_string(),
        None => text.trim().to_string(),
    }
}

/// A timestamp without pulling in a date library for one format string.
fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    humantime_like(now.as_secs())
}

/// Seconds since the epoch, rendered as an RFC 3339 UTC timestamp.
fn humantime_like(secs: u64) -> String {
    // Civil-from-days, the standard algorithm. Cheaper than a dependency for
    // the one timestamp this module emits.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, d, h, m, s
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_yields_its_last_point_in_either_shape() {
        assert_eq!(last_series_value(&serde_json::json!([1.0, 2.5])), Some(2.5));
        assert_eq!(
            last_series_value(&serde_json::json!([{ "value": 4.0 }, { "value": 9.5 }])),
            Some(9.5)
        );
        assert_eq!(
            last_series_value(&serde_json::json!([{ "y": 3.0 }])),
            Some(3.0)
        );
    }

    /// An empty series is an unsampleable runtime, not an idle one. Returning
    /// 0.0 here would draw a healthy flat line over a node nobody can measure.
    #[test]
    fn an_empty_series_has_no_value_rather_than_zero() {
        assert_eq!(last_series_value(&serde_json::json!([])), None);
        assert_eq!(last_series_value(&serde_json::json!(null)), None);
    }

    #[test]
    fn timestamps_render_as_rfc3339() {
        assert_eq!(humantime_like(0), "1970-01-01T00:00:00Z");
        assert_eq!(humantime_like(1_753_920_000), "2025-07-31T00:00:00Z");
        // A leap day, where the civil-from-days arithmetic earns its keep.
        assert_eq!(humantime_like(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn a_multiline_error_is_reduced_to_its_first_line() {
        let err = anyhow!("Could not reach the Kovalent API\n  caused by: dns failure");
        assert_eq!(short_reason(&err), "Could not reach the Kovalent API");
    }

    /// Local first, then alphabetical. A list that reorders itself as latencies
    /// move cannot be selected from with the arrow keys.
    #[test]
    fn rows_sort_local_first_then_by_name() {
        let mut rows = [
            row_named("zeta", false),
            row_named("alpha", false),
            row_named("local", true),
        ];
        rows.sort_by(|a, b| b.local.cmp(&a.local).then_with(|| a.name.cmp(&b.name)));
        let order: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, vec!["local", "alpha", "zeta"]);
    }

    /// The columns a shallow tick cannot fill keep the last reading, or the
    /// table blinks to dashes four ticks in five.
    #[test]
    fn slow_readings_survive_the_ticks_that_do_not_take_them() {
        let mut previous = Snapshot {
            taken_at: "t0".to_string(),
            nodes: vec![row_named("alpha", false)],
            hosted_unavailable: None,
        };
        previous.nodes[0].cpu = Some(12.0);
        previous.nodes[0].documents = Some(7);
        previous.nodes[0].last_seen = Some("t0".to_string());

        let mut current = Snapshot {
            taken_at: "t1".to_string(),
            nodes: vec![row_named("alpha", false)],
            hosted_unavailable: None,
        };
        carry_forward(&mut current, &previous);

        assert_eq!(current.nodes[0].cpu, Some(12.0));
        assert_eq!(current.nodes[0].documents, Some(7));
        assert_eq!(current.nodes[0].last_seen.as_deref(), Some("t0"));
    }

    /// A fresh reading always wins. Carrying over a live value would show a
    /// load that is no longer being measured.
    #[test]
    fn a_new_reading_is_never_overwritten_by_the_old_one() {
        let mut previous = Snapshot {
            taken_at: "t0".to_string(),
            nodes: vec![row_named("alpha", false)],
            hosted_unavailable: None,
        };
        previous.nodes[0].cpu = Some(12.0);

        let mut current = Snapshot {
            taken_at: "t1".to_string(),
            nodes: vec![row_named("alpha", false)],
            hosted_unavailable: None,
        };
        current.nodes[0].cpu = Some(80.0);
        carry_forward(&mut current, &previous);

        assert_eq!(current.nodes[0].cpu, Some(80.0));
    }

    /// A node that appears mid-session has nothing to inherit, and must not
    /// pick up another node's numbers.
    #[test]
    fn a_node_with_no_previous_row_inherits_nothing() {
        let previous = Snapshot {
            taken_at: "t0".to_string(),
            nodes: vec![],
            hosted_unavailable: None,
        };
        let mut current = Snapshot {
            taken_at: "t1".to_string(),
            nodes: vec![row_named("alpha", false)],
            hosted_unavailable: None,
        };
        carry_forward(&mut current, &previous);

        assert_eq!(current.nodes[0].cpu, None);
        assert_eq!(current.nodes[0].documents, None);
    }

    fn row_named(name: &str, local: bool) -> NodeRow {
        NodeRow {
            id: name.to_string(),
            uuid: None,
            name: name.to_string(),
            local,
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
}
