//! `knaix bench` -- how fast this node is, in the terms it is actually used in.
//!
//! `selftest` answers "does this node answer correctly". This answers "how long
//! does it take", which is the other half of the question anyone comparing a
//! local node against a hosted one, or one machine against another, is really
//! asking.
//!
//! Three numbers, because they fail for different reasons and a single
//! end-to-end figure hides which one moved:
//!
//! - **reach**: a bare round trip to the node's health endpoint. The floor
//!   everything else sits on, and the part that is purely network.
//! - **ingest**: one generated document, parsed, chunked, embedded and written.
//!   This is the write side of the vector store, and the number that grows when
//!   embedding is slow or being done somewhere far away.
//! - **answer**: a question, split into time-to-first-token and total. Retrieval,
//!   reranking and prompt assembly all happen before that first token, so the
//!   split is what separates a slow knowledge base from a slow model.
//!
//! Like `selftest`, everything it uploads is removed before it returns.

use crate::exit::{Code, WithCode};
use crate::nodes::{KnaixContext, Target, Verbosity};
use crate::selftest::{delete_document, ingest_text, percentile};
use anyhow::{anyhow, Context, Result};
use colored::*;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use std::time::{Duration, Instant};

/// Marks the document this command uploads, matching selftest's convention so a
/// stray one is recognisable as something the CLI generated rather than a real
/// document someone ingested.
const DOC_PREFIX: &str = "knaix-bench-";

/// Runs per phase when none is given. Enough for a p50 and a rough p95 without
/// spending a coffee break, or a rate-limit budget, on a diagnostic.
pub const DEFAULT_RUNS: usize = 5;

/// The ceiling on runs. Past this the command stops being a quick measurement
/// and starts being a load test against someone's node, which is not what a
/// diagnostic should make easy to do by accident.
const MAX_RUNS: usize = 100;

/// The document every run ingests and asks about.
///
/// Fixed, so two runs measure the same work, and self-contained, so a benchmark
/// needs no network beyond the node under test. Long enough to chunk into more
/// than one passage: a single-chunk document would measure an embedding call
/// rather than an ingest.
const DOC_BODY: &str = "\
# Meridian Freight Handbook (generated)

## Section 1 -- Dispatch windows

Every consignment leaving the Halverton depot is assigned a dispatch window of
ninety minutes. A consignment that misses its window is re-slotted to the next
available window on the same day, and the dispatcher records the reason under
code DW-4. Three DW-4 records against one carrier in a rolling week triggers a
review by the depot supervisor.

## Section 2 -- Cold chain

Refrigerated consignments are held between two and six degrees Celsius from the
moment the seal is applied until the seal is broken at the destination. The
temperature is logged every fifteen minutes by the trailer unit. A gap of more
than one hour in the log invalidates the cold chain for that consignment, and
the receiving depot must quarantine it pending an inspection.

## Section 3 -- Damage claims

A damage claim must be raised within seventy-two hours of delivery. Claims
raised later are assessed only where the consignee can show that the damage was
concealed at the point of delivery. Photographs taken at the delivery point are
required in every case; a claim without them is returned unassessed.

## Section 4 -- Driver hours

A driver may not exceed nine hours at the wheel in a single shift, extendable to
ten hours twice in any week. A break of at least forty-five minutes is taken
after four and a half hours of driving, and may be split into one break of at
least fifteen minutes followed by one of at least thirty.
";

/// The question each answer run asks. One question, asked repeatedly, so the
/// spread across runs is the node's variance rather than the difference between
/// an easy question and a hard one.
const QUESTION: &str = "What invalidates the cold chain for a refrigerated consignment?";

/// Milliseconds at each percentile for one phase.
#[derive(Serialize)]
pub struct Timing {
    pub runs: usize,
    pub p50: u128,
    pub p95: u128,
    pub min: u128,
    pub max: u128,
}

impl Timing {
    /// Summarize raw samples. Sorts a copy: the caller's order is the order the
    /// runs happened in, which is worth keeping for the JSON.
    fn of(samples: &[u128]) -> Self {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        Timing {
            runs: sorted.len(),
            p50: percentile(&sorted, 0.50),
            p95: percentile(&sorted, 0.95),
            min: sorted.first().copied().unwrap_or(0),
            max: sorted.last().copied().unwrap_or(0),
        }
    }
}

#[derive(Serialize)]
pub struct BenchReport {
    pub node: String,
    pub model: Option<String>,
    /// False when the deterministic mock answered. Retrieval and ingest are
    /// still real, but the answer timings measure the mock rather than a model,
    /// so a number from a mock run must never be compared with a real one.
    pub answer_timing_meaningful: bool,
    pub reach_ms: Timing,
    /// Absent when `--no-ingest` skipped the write side.
    pub ingest_ms: Option<u128>,
    pub answer_total_ms: Timing,
    /// Absent when the node did not stream, so there was no first token to time.
    pub answer_first_token_ms: Option<Timing>,
    /// Raw samples, in the order the runs happened, so a warm-up effect is
    /// visible rather than averaged away.
    pub samples: Samples,
}

#[derive(Serialize)]
pub struct Samples {
    pub reach_ms: Vec<u128>,
    pub answer_total_ms: Vec<u128>,
    pub answer_first_token_ms: Vec<u128>,
}

pub async fn run(
    ctx: &KnaixContext,
    target: &Target,
    runs: usize,
    no_ingest: bool,
    keep: bool,
) -> Result<()> {
    if runs == 0 || runs > MAX_RUNS {
        return Err(anyhow!(
            "--runs must be between 1 and {MAX_RUNS}; {runs} was asked for"
        ))
        .coded(Code::Usage);
    }

    let node = target.label();
    let human = ctx.output_format != "json" && !ctx.quiet;

    if human {
        println!(
            "\n{}",
            format!("Benchmark on node {node}").bold().underline()
        );
        println!(
            "  {runs} run(s) per phase. One generated document is ingested and removed again.\n"
        );
    }

    let pb = spinner(human);

    // Reach first: if the node cannot be reached at all, the failure should say
    // that rather than surfacing as a confusing ingest error.
    pb.set_message("Measuring reachability...");
    let reach = match measure_reach(ctx, target, runs).await {
        Ok(samples) => samples,
        Err(e) => {
            pb.finish_and_clear();
            return Err(e);
        }
    };

    let mut document_id: Option<String> = None;
    let ingest_ms = if no_ingest {
        None
    } else {
        pb.set_message("Ingesting the benchmark document...");
        match measure_ingest(ctx, target).await {
            Ok((ms, id)) => {
                document_id = id;
                Some(ms)
            }
            Err(e) => {
                pb.finish_and_clear();
                return Err(e);
            }
        }
    };

    let asked = match measure_answers(ctx, target, runs, &pb, human).await {
        Ok(asked) => asked,
        Err(e) => {
            pb.finish_and_clear();
            cleanup(ctx, target, document_id.as_deref(), keep, human).await;
            return Err(e);
        }
    };
    pb.finish_and_clear();

    cleanup(ctx, target, document_id.as_deref(), keep, human).await;

    let report = BenchReport {
        node,
        answer_timing_meaningful: !is_mock(asked.model.as_deref()),
        model: asked.model,
        reach_ms: Timing::of(&reach),
        ingest_ms,
        answer_total_ms: Timing::of(&asked.total_ms),
        answer_first_token_ms: if asked.first_token_ms.is_empty() {
            None
        } else {
            Some(Timing::of(&asked.first_token_ms))
        },
        samples: Samples {
            reach_ms: reach,
            answer_total_ms: asked.total_ms,
            answer_first_token_ms: asked.first_token_ms,
        },
    };

    if human {
        print_report(&report);
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }

    Ok(())
}

/// A node with no real model behind it answers from a fixed template, so its
/// answer timings measure the template rather than generation.
fn is_mock(model: Option<&str>) -> bool {
    matches!(model, None | Some("mock"))
}

fn spinner(human: bool) -> ProgressBar {
    if !human {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

/// Round trips to whichever health endpoint stands in front of this node.
///
/// The two are not the same measurement and the report says so: a local node is
/// one hop away on loopback, while a hosted node's health is what the control
/// plane can see of it, so the hosted number includes the control plane's own
/// round trip to the node.
async fn measure_reach(ctx: &KnaixContext, target: &Target, runs: usize) -> Result<Vec<u128>> {
    let mut samples = Vec::with_capacity(runs);
    for i in 0..runs {
        let started = Instant::now();
        let ok = match target {
            Target::Local { base, .. } => ctx
                .client
                .get(format!("{base}/health"))
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false),
            Target::Remote { uuid } => {
                let token = ctx.get_token()?;
                ctx.client
                    .get(format!("{}/api/nodes/{}/health", ctx.config.api_url, uuid))
                    .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
                    .timeout(Duration::from_secs(30))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            }
        };
        if !ok {
            return Err(anyhow!(
                "The node stopped answering its health check on run {} of {}. Nothing was ingested.",
                i + 1,
                runs
            ))
            .coded(Code::Unavailable);
        }
        samples.push(started.elapsed().as_millis());
    }
    Ok(samples)
}

/// Time one document all the way through parse, chunk, embed and write.
///
/// One document rather than several: the phase is already the slowest one on a
/// node embedding locally, and repeating it would multiply the wait for a
/// number whose variance is not what anyone is here to see.
async fn measure_ingest(ctx: &KnaixContext, target: &Target) -> Result<(u128, Option<String>)> {
    let name = format!("{DOC_PREFIX}{}.md", run_id());
    let started = Instant::now();
    let id = ingest_text(ctx, target, &name, DOC_BODY)
        .await
        .context("The benchmark document could not be ingested")?;
    Ok((started.elapsed().as_millis(), id))
}

struct Asked {
    total_ms: Vec<u128>,
    first_token_ms: Vec<u128>,
    model: Option<String>,
}

async fn measure_answers(
    ctx: &KnaixContext,
    target: &Target,
    runs: usize,
    pb: &ProgressBar,
    human: bool,
) -> Result<Asked> {
    let mut total_ms = Vec::with_capacity(runs);
    let mut first_token_ms = Vec::new();
    let mut model: Option<String> = None;

    for i in 0..runs {
        if human {
            pb.set_message(format!("Asking {} of {}...", i + 1, runs));
        }
        let started = Instant::now();
        // No history and default verbosity, so every run asks for the same work.
        let answer = crate::nodes::chat(ctx, target, QUESTION, false, &[], Verbosity::Normal)
            .await
            .map_err(|e| {
                if e.to_string().contains("429") {
                    anyhow!(
                        "Rate limited part-way through the benchmark. A run spends one request per \
                         answer; wait for the window to reset, or lower --runs."
                    )
                } else {
                    e
                }
            })?
            .ok_or_else(|| anyhow!("The node returned no answer on run {}", i + 1))?;

        total_ms.push(started.elapsed().as_millis());
        if let Some(ms) = answer.first_token_ms {
            first_token_ms.push(ms);
        }
        if model.is_none() {
            model = answer.model.clone();
        }
    }

    Ok(Asked {
        total_ms,
        first_token_ms,
        model,
    })
}

/// A short, unique tag per run, so two benchmarks running at once cannot delete
/// each other's document.
fn run_id() -> String {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).expect("failed to read OS randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Remove what this run created. Reports rather than throws: the measurement is
/// already taken, and a user must never be left unaware that a generated
/// document is still on their node.
async fn cleanup(
    ctx: &KnaixContext,
    target: &Target,
    document_id: Option<&str>,
    keep: bool,
    human: bool,
) {
    let Some(id) = document_id else {
        return;
    };
    if keep {
        if human {
            println!(
                "{} Keeping the benchmark document on the node (--keep). It is named '{}*'.",
                "Info:".blue(),
                DOC_PREFIX
            );
        }
        return;
    }
    if delete_document(ctx, target, id).await.is_err() {
        eprintln!(
            "{} Could not remove the benchmark document from the node. It is named '{}*'.",
            "Warning:".yellow(),
            DOC_PREFIX
        );
    }
}

fn print_report(report: &BenchReport) {
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.set_header(vec!["Phase", "p50", "p95", "min", "max"]);

    let row = |name: &str, t: &Timing| {
        vec![
            name.to_string(),
            format!("{}ms", t.p50),
            format!("{}ms", t.p95),
            format!("{}ms", t.min),
            format!("{}ms", t.max),
        ]
    };

    table.add_row(row("Reach", &report.reach_ms));
    if let Some(ms) = report.ingest_ms {
        // One sample, so percentile columns would be four copies of one number.
        table.add_row(vec![
            "Ingest".to_string(),
            format!("{ms}ms"),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
        ]);
    }
    if let Some(t) = &report.answer_first_token_ms {
        table.add_row(row("Answer, first token", t));
    }
    table.add_row(row("Answer, total", &report.answer_total_ms));
    println!("{table}");

    println!(
        "\n  {} {}",
        "Answered by:".dimmed(),
        report.model.as_deref().unwrap_or("unreported")
    );
    if !report.answer_timing_meaningful {
        println!(
            "  {} The deterministic mock answered, so the answer timings measure the mock, not a\n      model. Retrieval and ingest are real. Point the node at a model with {}.",
            "Note:".blue(),
            crate::brand::cmd("local setup")
        );
    }
    if let Some(t) = &report.answer_first_token_ms {
        // The one comparison worth spelling out: it is the split between the
        // knowledge base and the model, and it is not obvious from the rows.
        let generation = report.answer_total_ms.p50.saturating_sub(t.p50);
        println!(
            "  {} {}ms to the first token (retrieval, reranking, prompt), then {}ms generating.",
            "Split:".dimmed(),
            t.p50,
            generation
        );
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_come_from_the_sorted_samples() {
        // Deliberately out of order: the runs happen in time order, and the
        // percentiles must not depend on which run was slow.
        let t = Timing::of(&[90, 10, 50, 30, 70]);
        assert_eq!(t.runs, 5);
        assert_eq!(t.min, 10);
        assert_eq!(t.max, 90);
        assert_eq!(t.p50, 50);
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        let t = Timing::of(&[42]);
        assert_eq!((t.p50, t.p95, t.min, t.max), (42, 42, 42, 42));
    }

    /// The mock answers from a template. Comparing a mock run's timings with a
    /// real one's would be meaningless, so the report has to say which it was.
    #[test]
    fn the_mock_is_recognised_however_it_is_reported() {
        assert!(is_mock(None));
        assert!(is_mock(Some("mock")));
        assert!(!is_mock(Some("llama3.1:8b")));
    }

    #[test]
    fn run_ids_are_unique_per_run() {
        assert_ne!(run_id(), run_id());
    }
}
