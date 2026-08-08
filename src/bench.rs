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
//! - **reach**: a round trip to the node's health check. The floor everything
//!   else sits on. Against a hosted node this goes through the control plane,
//!   which probes the node itself, so that hop is inside the number.
//! - **ingest**: one generated document, parsed, chunked, embedded and written.
//!   This is the write side of the vector store, and the number that grows when
//!   embedding is slow or being done somewhere far away.
//! - **answer**: a question, split into time-to-first-token and total. Retrieval,
//!   reranking and prompt assembly all happen before that first token, so the
//!   split is what separates a slow knowledge base from a slow model.
//!
//! The document it uploads is part of the corpus while it is there, so a normal
//! run removes it before returning. Two outcomes cannot, and both say so rather
//! than exiting quietly; see `Ingested`.

use crate::exit::{Code, WithCode};
use crate::nodes::{format_duration_ms, KnaixContext, Target};
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
    sweep: bool,
) -> Result<()> {
    if runs == 0 || runs > MAX_RUNS {
        return Err(anyhow!(
            "--runs must be between 1 and {MAX_RUNS}; {runs} was asked for"
        ))
        .coded(Code::Usage);
    }

    let node = target.label();
    let human = ctx.output_format != "json" && !ctx.quiet;

    // A run interrupted before cleanup leaves its document behind, where it
    // becomes part of the corpus and can be retrieved and cited in real
    // answers. Report that before measuring anything, and offer to clear it.
    //
    // On a normal run the scan is a courtesy, and it now costs a request, so a
    // node that cannot answer it must not fail the command here: reachability is
    // measured below and is the check that exists to report exactly that. Under
    // --sweep the listing is the whole job, so there the failure is the answer.
    //
    // Said rather than swallowed. An unreachable node is about to be reported
    // properly, but a node that is up and fails this one route would otherwise
    // take the warning with it, and the run would measure against a corpus with
    // leftovers in it while saying nothing.
    let leftovers = if sweep {
        crate::selftest::list_documents_with_prefix(ctx, target, DOC_PREFIX).await?
    } else {
        match crate::selftest::list_documents_with_prefix(ctx, target, DOC_PREFIX).await {
            Ok(found) => found,
            Err(e) => {
                if human {
                    eprintln!(
                        "{} Could not check for documents left by an earlier run: {}",
                        "Warning:".yellow(),
                        e
                    );
                }
                Vec::new()
            }
        }
    };
    if sweep {
        let removed = sweep_previous(ctx, target, &leftovers).await;
        if human {
            println!(
                "{} Removed {} benchmark document(s) left by an earlier run.",
                "Info:".blue(),
                removed
            );
        }
        if no_ingest || !leftovers.is_empty() {
            return Ok(());
        }
    } else if !leftovers.is_empty() && human {
        println!(
            "{} {} benchmark document(s) from an earlier run are on this node. They are\n      part of the corpus and can be cited in answers; clear them with {}.",
            "Warning:".yellow(),
            leftovers.len(),
            crate::brand::cmd("bench --sweep")
        );
    }

    if human {
        println!(
            "\n{}",
            format!("Benchmark on node {node}").bold().underline()
        );
        // Not "and removed again": two outcomes cannot remove it, and the
        // command says so when they happen. Promising it here would be the
        // same over-claim in the one place the user is actually standing.
        println!("  {runs} run(s) per phase. One generated document is ingested, then removed.\n");
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

    let mut ingested = Ingested::Nothing;
    let ingest_ms = if no_ingest {
        None
    } else {
        pb.set_message("Ingesting the benchmark document...");
        let started = Instant::now();
        let name = document_name();
        // The outcome is recorded before the error is propagated: a request
        // that failed may still have been applied, and the document that
        // leaves behind has to be reported either way.
        match ingest_text(ctx, target, &name, DOC_BODY).await {
            Ok(Some(id)) => {
                ingested = Ingested::Known(id);
                Some(started.elapsed().as_millis())
            }
            Ok(None) => {
                // The node stored it and did not name the row, so there is no
                // id to delete by. Silence here is what orphaned documents.
                ingested = Ingested::Unnamed(name);
                Some(started.elapsed().as_millis())
            }
            Err(e) => {
                pb.finish_and_clear();
                // The node may have applied the write before failing the
                // response, so this is "possibly there", not "not there".
                ingested = Ingested::Uncertain(name);
                cleanup(ctx, target, &ingested, keep, human).await;
                return Err(e).context("The benchmark document could not be ingested");
            }
        }
    };

    let asked = match measure_answers(ctx, target, runs, &pb, human).await {
        Ok(asked) => asked,
        Err(e) => {
            pb.finish_and_clear();
            cleanup(ctx, target, &ingested, keep, human).await;
            return Err(e);
        }
    };
    pb.finish_and_clear();

    cleanup(ctx, target, &ingested, keep, human).await;

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

/// What the ingest phase left on the node, which is not the same question as
/// whether it succeeded.
///
/// The node can accept a document and not name the row, and it can apply a
/// write and then fail the response. Both leave a document behind, and neither
/// leaves an id to delete it by. Modelling only "id or nothing" is what let a
/// successful-looking run walk away from a document it had created.
enum Ingested {
    /// Nothing was sent, or nothing reached the node.
    Nothing,
    /// On the node, under an id that can be deleted.
    Known(String),
    /// On the node, accepted without an id. Nothing to delete it by.
    Unnamed(String),
    /// The request failed, but the node may have applied it first.
    Uncertain(String),
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
        let answer = crate::nodes::chat(
            ctx,
            target,
            QUESTION,
            crate::nodes::Echo::Silent,
            &[],
            &crate::nodes::AnswerOptions::default(),
            None,
        )
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

fn document_name() -> String {
    format!("{DOC_PREFIX}{}.md", run_id())
}

/// Delete documents an earlier run left behind, reporting how many went.
async fn sweep_previous(ctx: &KnaixContext, target: &Target, stale: &[String]) -> usize {
    let mut removed = 0;
    for id in stale {
        if delete_document(ctx, target, id).await.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The sentence to print when a document is on the node and cannot be removed
/// by id. Both node shapes can find it again by name, so both get the sweep.
fn orphan_remedy() -> String {
    format!(
        "It is named '{}*'. Remove it with {}.",
        DOC_PREFIX,
        crate::brand::cmd("bench --sweep")
    )
}

/// Remove what this run created. Reports rather than throws: the measurement is
/// already taken, and a user must never be left unaware that a generated
/// document is still on their node.
///
/// Every arm that leaves something behind says so. The document is a synthetic
/// handbook, so one left on a node joins the corpus and can be retrieved and
/// cited in real answers -- which makes silence here the expensive option.
async fn cleanup(
    ctx: &KnaixContext,
    target: &Target,
    ingested: &Ingested,
    keep: bool,
    human: bool,
) {
    match ingested {
        Ingested::Nothing => {}

        Ingested::Known(id) => {
            if keep {
                if human {
                    println!(
                        "{} Keeping the benchmark document on the node (--keep). {}",
                        "Info:".blue(),
                        orphan_remedy()
                    );
                }
                return;
            }
            if delete_document(ctx, target, id).await.is_err() {
                eprintln!(
                    "{} Could not remove the benchmark document from the node. {}",
                    "Warning:".yellow(),
                    orphan_remedy()
                );
            }
        }

        Ingested::Unnamed(name) => eprintln!(
            "{} The node stored {} without returning an id, so it could not be removed. {}",
            "Warning:".yellow(),
            name,
            orphan_remedy()
        ),

        Ingested::Uncertain(name) => eprintln!(
            "{} The ingest failed, but the node may have stored {} before it did. {}",
            "Warning:".yellow(),
            name,
            orphan_remedy()
        ),
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
            format_duration_ms(t.p50),
            format_duration_ms(t.p95),
            format_duration_ms(t.min),
            format_duration_ms(t.max),
        ]
    };

    table.add_row(row("Reach", &report.reach_ms));
    if let Some(ms) = report.ingest_ms {
        // One sample, so percentile columns would be four copies of one number.
        table.add_row(vec![
            "Ingest".to_string(),
            format_duration_ms(ms),
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
        report
            .model
            .as_deref()
            .map(crate::nodes::display_model)
            .unwrap_or("unreported")
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
            "  {} {} to the first token (retrieval, reranking, prompt), then {} generating.",
            "Split:".dimmed(),
            format_duration_ms(t.p50),
            format_duration_ms(generation)
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

    /// Every state that leaves a document on the node has to be distinguishable
    /// from the state that does not. Collapsing these into "id or nothing" is
    /// what let a run walk away from a document it had created, and the
    /// document is a synthetic handbook that then answers real questions.
    #[test]
    fn every_outcome_that_leaves_a_document_is_its_own_state() {
        let leaves_something = |i: &Ingested| !matches!(i, Ingested::Nothing);

        assert!(!leaves_something(&Ingested::Nothing));
        assert!(leaves_something(&Ingested::Known("doc-1".into())));
        // The two that used to be indistinguishable from Nothing.
        assert!(leaves_something(&Ingested::Unnamed("a.md".into())));
        assert!(leaves_something(&Ingested::Uncertain("a.md".into())));
    }

    /// A hosted node can be searched by filename, so the remedy is the sweep.
    /// The remedy has to be one the sweep can carry out. It used to send a local
    /// node to `local reset`, which empties the store: everything the user
    /// ingested, to remove one synthetic handbook. Both shapes enumerate by name
    /// now, so both get the sweep, and neither is told to start over.
    #[test]
    fn the_remedy_is_the_sweep_and_names_the_prefix() {
        assert!(orphan_remedy().contains("bench --sweep"));
        assert!(orphan_remedy().contains(DOC_PREFIX));
        assert!(!orphan_remedy().contains("local reset"));
    }
}
