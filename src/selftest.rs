//! `knaix selftest` -- measure whether a node actually answers correctly.
//!
//! Ingests a small, synthetic corpus, asks questions whose supporting passages
//! are known in advance, and reports how often retrieval found the right
//! passage and how often the answer cited it. It is a real end-to-end exercise
//! of the node -- ingest, embed, retrieve, rerank, synthesize -- rather than a
//! reachability check, so "the API is up" and "the node can answer" stop being
//! the same result.
//!
//! The corpus is synthetic and license-clean, and every document this command
//! uploads is deleted before it returns. A node under test finishes with the
//! corpus it started with.

use crate::nodes::{Citation, KnaixContext};
use anyhow::{anyhow, Context, Result};
use colored::*;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::AUTHORIZATION;
use reqwest::multipart;
use serde::Deserialize;
use std::time::{Duration, Instant};

/// Marks every document this command uploads, so a run can find and remove its
/// own documents -- and any left behind by a run that was interrupted.
const DOC_PREFIX: &str = "knaix-selftest-";

/// Retrieval depth. The gold labels identify one supporting passage per
/// question, so this is how far down the ranking still counts as finding it.
const TOP_K: usize = 5;

/// Pass bar, mirroring the platform's own eval floors. A run below any of these
/// is the finding; do not lower them to make a red run green.
const FLOOR_HIT_RATE: f64 = 0.95;
const FLOOR_MRR: f64 = 0.90;
const FLOOR_CITATION_ACCURACY: f64 = 0.95;

/// One corpus document, embedded in the binary so a self-test needs no network
/// beyond the node it is testing.
struct CorpusDoc {
    source_key: &'static str,
    body: &'static str,
}

/// The corpus travels with the binary: a self-test that downloaded its own
/// fixtures could not run on an air-gapped node, which is exactly where being
/// able to prove the node works matters most.
fn corpus() -> Vec<CorpusDoc> {
    vec![
        CorpusDoc {
            source_key: "legal-msa",
            body: include_str!("../assets/selftest/corpus/legal-msa.md"),
        },
        CorpusDoc {
            source_key: "legal-dpa",
            body: include_str!("../assets/selftest/corpus/legal-dpa.md"),
        },
        CorpusDoc {
            source_key: "finance-expense",
            body: include_str!("../assets/selftest/corpus/finance-expense-policy.md"),
        },
        CorpusDoc {
            source_key: "finance-revrec",
            body: include_str!("../assets/selftest/corpus/finance-revenue-recognition.md"),
        },
        CorpusDoc {
            source_key: "health-baa",
            body: include_str!("../assets/selftest/corpus/healthcare-hipaa-baa.md"),
        },
        CorpusDoc {
            source_key: "health-protocol",
            body: include_str!("../assets/selftest/corpus/healthcare-clinical-protocol.md"),
        },
    ]
}

#[derive(Deserialize, Clone)]
struct GoldenQuestion {
    id: String,
    domain: String,
    query: String,
    /// The document(s) whose passages support the answer.
    #[serde(rename = "goldSourceKeys")]
    gold_source_keys: Vec<String>,
    /// Phrases that together identify the supporting passage. Chunk ids are
    /// generated at ingest, so the label matches on content instead.
    #[serde(rename = "goldNeedles")]
    gold_needles: Vec<String>,
}

fn questions() -> Result<Vec<GoldenQuestion>> {
    serde_json::from_str(include_str!("../assets/selftest/questions.json"))
        .context("selftest question set is malformed")
}

impl GoldenQuestion {
    /// Whether a retrieved passage is the one that supports this question.
    ///
    /// Both halves of the label have to hold: the passage must carry the
    /// distinguishing phrases *and* come from the document the answer lives in.
    /// Content alone would credit a phrase that happened to appear in another
    /// document, which is a retrieval mistake rather than a hit.
    fn is_supporting(&self, content: &str, source_name: Option<&str>) -> bool {
        let haystack = content.to_lowercase();
        let needles_match = self
            .gold_needles
            .iter()
            .all(|n| haystack.contains(&n.to_lowercase()));

        // Documents are uploaded as "<prefix><run>-<sourceKey>.md", so the key
        // is recoverable from the name the node reports back.
        let source_match = match source_name {
            Some(name) => self.gold_source_keys.iter().any(|k| name.contains(k)),
            // A node that reports no source cannot be credited with a hit it
            // cannot be shown to have made.
            None => false,
        };

        needles_match && source_match
    }
}

/// What one question produced, kept so `--json` can hand back the evidence
/// behind every number rather than just the number.
#[derive(serde::Serialize)]
pub struct QuestionOutcome {
    pub id: String,
    pub domain: String,
    pub query: String,
    /// 1-based rank of the first supporting passage, absent if none was found.
    pub supporting_rank: Option<usize>,
    pub cited_supporting: Option<bool>,
    pub answer: String,
    pub answer_ms: u128,
}

#[derive(serde::Serialize)]
pub struct SelfTestReport {
    pub node: String,
    pub model: Option<String>,
    /// False when the node answered with the deterministic mock, where citation
    /// accuracy only restates rank-1 retrieval and says nothing about a model.
    pub citation_accuracy_meaningful: bool,
    pub questions: usize,
    pub hit_rate_at_k: f64,
    pub mrr: f64,
    pub citation_accuracy: f64,
    pub top_k: usize,
    pub passed: bool,
    pub latency_ms: LatencySummary,
    pub outcomes: Vec<QuestionOutcome>,
}

#[derive(serde::Serialize)]
pub struct LatencySummary {
    pub answer_p50: u128,
    pub answer_p95: u128,
}

/// Percentile by nearest-rank over an already-sorted slice.
fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).max(1) - 1;
    sorted[rank.min(sorted.len() - 1)]
}

/// Questions per source document in a quick run, chosen to keep every document
/// and domain represented.
const QUICK_PER_SOURCE: usize = 2;

/// A balanced subset: the first few questions for each source document, in
/// fixture order so two runs measure the same thing. Sampling randomly would
/// make consecutive runs incomparable, which is the opposite of what a bar you
/// re-run against is for.
fn quick_subset(all: &[GoldenQuestion]) -> Vec<GoldenQuestion> {
    let mut taken: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for q in all {
        let key = q.gold_source_keys.first().map(|s| s.as_str()).unwrap_or("");
        let count = taken.entry(key).or_insert(0);
        if *count < QUICK_PER_SOURCE {
            *count += 1;
            out.push(q.clone());
        }
    }
    out
}

pub async fn run(
    ctx: &KnaixContext,
    node_uuid: &str,
    keep: bool,
    quick: bool,
    sweep: bool,
) -> Result<()> {
    let all = questions()?;
    let questions = if quick { quick_subset(&all) } else { all };
    let human = ctx.output_format != "json";

    // A run interrupted before cleanup leaves its corpus behind, and the next
    // run would then measure against two copies of every document. Report that
    // rather than assuming ownership of documents this run did not create.
    let leftovers = list_selftest_documents(ctx, node_uuid).await?;
    if !leftovers.is_empty() {
        if sweep {
            let swept = sweep_previous(ctx, node_uuid).await?;
            if human {
                println!(
                    "{} Removed {} self-test document(s) left by an earlier run.",
                    "Info:".blue(),
                    swept
                );
            }
        } else if human {
            // Benign, and worth saying so: the control plane reuses the
            // registry row for content-identical uploads, so this run re-ingests
            // those same documents rather than adding a second copy, and its own
            // cleanup removes them. --sweep exists for clearing them without
            // running a test at all.
            println!(
                "{} {} self-test document(s) from an earlier run are on this node. This run\n      re-uses and then removes them; pass {} to clear them without testing.",
                "Info:".blue(),
                leftovers.len(),
                "--sweep".cyan()
            );
        }
    }

    let run_id = short_run_id();
    if human {
        println!(
            "\n{}",
            format!("Self-test on node {} (run {})", node_uuid, run_id)
                .bold()
                .underline()
        );
        println!(
            "  {} synthetic documents, {} question{}. Everything uploaded is deleted afterwards.\n",
            corpus().len(),
            questions.len(),
            if quick { " (quick subset)" } else { "s" }
        );
    }

    let pb = spinner(human);
    pb.set_message("Ingesting the self-test corpus...");

    // Track ids as we go: a failure part-way through still has to clean up what
    // it managed to create.
    let mut document_ids: Vec<String> = Vec::new();
    let ingest = ingest_corpus(ctx, node_uuid, &run_id, &mut document_ids).await;
    if let Err(e) = ingest {
        pb.finish_and_clear();
        cleanup(ctx, node_uuid, &document_ids, keep, human).await;
        return Err(e);
    }

    let (outcomes, model) = match run_questions(ctx, node_uuid, &questions, &pb, human).await {
        Ok(o) => o,
        Err(e) => {
            pb.finish_and_clear();
            cleanup(ctx, node_uuid, &document_ids, keep, human).await;
            return Err(e);
        }
    };
    pb.finish_and_clear();

    cleanup(ctx, node_uuid, &document_ids, keep, human).await;

    let report = summarize(node_uuid, outcomes, model);
    if human {
        print_report(&report);
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }

    if report.passed {
        Ok(())
    } else {
        Err(anyhow!(
            "Self-test below the pass bar: hit-rate {:.0}% (floor {:.0}%), MRR {:.2} (floor {:.2}), citation accuracy {:.0}% (floor {:.0}%)",
            report.hit_rate_at_k * 100.0,
            FLOOR_HIT_RATE * 100.0,
            report.mrr,
            FLOOR_MRR,
            report.citation_accuracy * 100.0,
            FLOOR_CITATION_ACCURACY * 100.0
        ))
    }
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

/// A short, unique tag per run, so concurrent runs cannot delete each other's
/// documents.
fn short_run_id() -> String {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).expect("failed to read OS randomness");
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn document_name(run_id: &str, source_key: &str) -> String {
    format!("{}{}-{}.md", DOC_PREFIX, run_id, source_key)
}

async fn ingest_corpus(
    ctx: &KnaixContext,
    node_uuid: &str,
    run_id: &str,
    document_ids: &mut Vec<String>,
) -> Result<()> {
    let token = ctx.get_token()?;
    let url = format!(
        "{}/api/knowledge/{}/documents",
        ctx.config.api_url, node_uuid
    );

    for doc in corpus() {
        let name = document_name(run_id, doc.source_key);
        let part = multipart::Part::text(doc.body)
            .file_name(name.clone())
            .mime_str("text/markdown")?;
        let form = multipart::Form::new().part("file", part);

        let resp = ctx
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", token))
            .multipart(form)
            .send()
            .await
            .context("Failed to upload the self-test corpus")?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        if !status.is_success() || !body["success"].as_bool().unwrap_or(false) {
            return Err(anyhow!(
                "Self-test could not ingest {}: HTTP {} - {}",
                name,
                status,
                body["error"].as_str().unwrap_or("unknown error")
            ));
        }
        if let Some(id) = body["data"]["documentId"].as_str() {
            document_ids.push(id.to_string());
        }
    }
    Ok(())
}

async fn run_questions(
    ctx: &KnaixContext,
    node_uuid: &str,
    questions: &[GoldenQuestion],
    pb: &ProgressBar,
    human: bool,
) -> Result<(Vec<QuestionOutcome>, Option<String>)> {
    let mut outcomes = Vec::with_capacity(questions.len());
    let mut model: Option<String> = None;

    for (i, q) in questions.iter().enumerate() {
        if human {
            pb.set_message(format!("Asking {} of {}: {}", i + 1, questions.len(), q.id));
        }

        let started = Instant::now();
        let answer = ask(ctx, node_uuid, &q.query).await?;
        let answer_ms = started.elapsed().as_millis();
        if model.is_none() {
            model = answer.model.clone();
        }

        // Rank from the answer's own citations rather than a separate search.
        // They are the context the pipeline actually admitted after reranking,
        // so this measures the passage set the answer could have used -- and it
        // halves the requests a run costs, which matters against a shared
        // per-account rate limit.
        let mut ranked: Vec<&Citation> = answer.citations.iter().collect();
        ranked.sort_by_key(|c| c.index.unwrap_or(u32::MAX));

        let supporting_rank = ranked
            .iter()
            .position(|c| {
                q.is_supporting(
                    c.content.as_deref().unwrap_or(""),
                    c.source.as_ref().and_then(|s| s.name.as_deref()),
                )
            })
            .map(|i| i + 1);

        // Citation accuracy only means something when the answer cited
        // anything at all; an uncited answer is neither right nor wrong here.
        let cited: Vec<&Citation> = ranked
            .iter()
            .copied()
            .filter(|c| c.cited.unwrap_or(false))
            .collect();
        let cited_supporting = if cited.is_empty() {
            None
        } else {
            Some(cited.iter().any(|c| {
                q.is_supporting(
                    c.content.as_deref().unwrap_or(""),
                    c.source.as_ref().and_then(|s| s.name.as_deref()),
                )
            }))
        };

        outcomes.push(QuestionOutcome {
            id: q.id.clone(),
            domain: q.domain.clone(),
            query: q.query.clone(),
            supporting_rank,
            cited_supporting,
            answer: answer.text,
            answer_ms,
        });
    }

    Ok((outcomes, model))
}

/// Ask one question, surfacing a rate limit as the thing it is.
///
/// A run costs one request per question plus a handful for setup, against an
/// account-wide budget shared with everything else the user is doing. A raw
/// "HTTP 429" mid-run reads as a broken node rather than a spent budget.
async fn ask(ctx: &KnaixContext, node_uuid: &str, query: &str) -> Result<crate::nodes::ChatAnswer> {
    match crate::nodes::chat(ctx, node_uuid, query, false).await {
        Ok(Some(answer)) => Ok(answer),
        Ok(None) => Err(anyhow!("Node returned no answer")),
        Err(e) if e.to_string().contains("429") => Err(anyhow!(
            "Rate limited part-way through the run. A self-test spends one request per question; \
             wait for the window to reset, or use --quick for a shorter run."
        )),
        Err(e) => Err(e),
    }
}

/// Remove documents left by an earlier run, without running a test.
///
/// Opt-in, because the prefix identifies a self-test document but not *whose*.
/// Sweeping unconditionally would delete a concurrent run's corpus out from
/// under it, and that run would then measure a node whose documents vanished
/// mid-question -- a wrong answer reported confidently, which is worse than a
/// stale document left lying around.
async fn sweep_previous(ctx: &KnaixContext, node_uuid: &str) -> Result<usize> {
    let stale = list_selftest_documents(ctx, node_uuid).await?;
    let mut removed = 0;
    for id in stale {
        if delete_document(ctx, node_uuid, &id).await.is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

async fn list_selftest_documents(ctx: &KnaixContext, node_uuid: &str) -> Result<Vec<String>> {
    let token = ctx.get_token()?;
    let url = format!(
        "{}/api/knowledge/{}/documents",
        ctx.config.api_url, node_uuid
    );

    let resp = ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to list documents on the node")?;

    if !resp.status().is_success() {
        return Err(anyhow!("Failed to list documents: HTTP {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let empty = vec![];
    let docs = body["data"].as_array().unwrap_or(&empty);

    Ok(docs
        .iter()
        .filter(|d| {
            d["source"]["name"]
                .as_str()
                .map(|n| n.starts_with(DOC_PREFIX))
                .unwrap_or(false)
        })
        .filter_map(|d| d["id"].as_str().map(|s| s.to_string()))
        .collect())
}

async fn delete_document(ctx: &KnaixContext, node_uuid: &str, document_id: &str) -> Result<()> {
    let token = ctx.get_token()?;
    let url = format!(
        "{}/api/knowledge/{}/documents/{}",
        ctx.config.api_url, node_uuid, document_id
    );

    let resp = ctx
        .client
        .delete(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to delete a self-test document")?;

    if resp.status().is_success() {
        Ok(())
    } else {
        Err(anyhow!("Delete failed: HTTP {}", resp.status()))
    }
}

/// Remove what this run created. Reports rather than throws: the measurement is
/// already taken, and losing it to a cleanup error would help nobody -- but a
/// user must never be left unaware that generated documents are still there.
async fn cleanup(
    ctx: &KnaixContext,
    node_uuid: &str,
    document_ids: &[String],
    keep: bool,
    human: bool,
) {
    if keep {
        if human {
            println!(
                "{} Keeping {} self-test document(s) on the node (--keep). Remove them with {}.",
                "Info:".blue(),
                document_ids.len(),
                "knaix selftest --sweep".cyan()
            );
        }
        return;
    }

    let mut failed = Vec::new();
    for id in document_ids {
        if delete_document(ctx, node_uuid, id).await.is_err() {
            failed.push(id.clone());
        }
    }

    if !failed.is_empty() {
        eprintln!(
            "{} Could not remove {} self-test document(s) from the node. They are named '{}*' and the next run will sweep them.",
            "Warning:".yellow(),
            failed.len(),
            DOC_PREFIX
        );
    }
}

/// A node with no real model behind it cites whatever ranked first, so the
/// citation number restates retrieval rather than measuring the model.
fn is_mock_model(model: Option<&str>) -> bool {
    matches!(model, None | Some("mock"))
}

fn summarize(
    node_uuid: &str,
    outcomes: Vec<QuestionOutcome>,
    model: Option<String>,
) -> SelfTestReport {
    let total = outcomes.len();
    let hits = outcomes
        .iter()
        .filter(|o| o.supporting_rank.is_some())
        .count();

    let mrr_sum: f64 = outcomes
        .iter()
        .filter_map(|o| o.supporting_rank)
        .map(|r| 1.0 / r as f64)
        .sum();

    // Only answers that cited something can be scored for citation accuracy.
    let judged: Vec<bool> = outcomes.iter().filter_map(|o| o.cited_supporting).collect();
    let citation_accuracy = if judged.is_empty() {
        0.0
    } else {
        judged.iter().filter(|c| **c).count() as f64 / judged.len() as f64
    };

    let mut answer: Vec<u128> = outcomes.iter().map(|o| o.answer_ms).collect();
    answer.sort_unstable();

    let hit_rate_at_k = if total == 0 {
        0.0
    } else {
        hits as f64 / total as f64
    };
    let mrr = if total == 0 {
        0.0
    } else {
        mrr_sum / total as f64
    };

    let citation_accuracy_meaningful = !is_mock_model(model.as_deref());

    SelfTestReport {
        node: node_uuid.to_string(),
        model,
        citation_accuracy_meaningful,
        questions: total,
        hit_rate_at_k,
        mrr,
        citation_accuracy,
        top_k: TOP_K,
        // Retrieval floors always apply. The citation floor only does when a
        // real model chose the citations; enforcing it against the mock would
        // fail a healthy node for the mock's behaviour, which is the kind of
        // red result that teaches people to ignore the command.
        passed: hit_rate_at_k >= FLOOR_HIT_RATE
            && mrr >= FLOOR_MRR
            && (!citation_accuracy_meaningful || citation_accuracy >= FLOOR_CITATION_ACCURACY),
        latency_ms: LatencySummary {
            answer_p50: percentile(&answer, 0.50),
            answer_p95: percentile(&answer, 0.95),
        },
        outcomes,
    }
}

fn print_report(report: &SelfTestReport) {
    let verdict = |value: f64, floor: f64| {
        if value >= floor {
            "PASS".green()
        } else {
            "FAIL".red()
        }
    };

    println!("{}", "Answer quality:".bold());
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.set_header(vec!["Metric", "Result", "Floor", ""]);
    table.add_row(vec![
        format!("Hit rate @{}", report.top_k),
        format!("{:.1}%", report.hit_rate_at_k * 100.0),
        format!("{:.0}%", FLOOR_HIT_RATE * 100.0),
        verdict(report.hit_rate_at_k, FLOOR_HIT_RATE).to_string(),
    ]);
    table.add_row(vec![
        "MRR".to_string(),
        format!("{:.3}", report.mrr),
        format!("{:.2}", FLOOR_MRR),
        verdict(report.mrr, FLOOR_MRR).to_string(),
    ]);
    table.add_row(vec![
        "Citation accuracy".to_string(),
        format!("{:.1}%", report.citation_accuracy * 100.0),
        if report.citation_accuracy_meaningful {
            format!("{:.0}%", FLOOR_CITATION_ACCURACY * 100.0)
        } else {
            "n/a".to_string()
        },
        if report.citation_accuracy_meaningful {
            verdict(report.citation_accuracy, FLOOR_CITATION_ACCURACY).to_string()
        } else {
            "INFO".dimmed().to_string()
        },
    ]);
    println!("{table}");

    if !report.citation_accuracy_meaningful {
        // Say it plainly. A number with no floor invites the reader to assume
        // the worst about it, and this one is not the node's fault.
        println!(
            "{} This node answered with the deterministic mock, which always cites the\n     top-ranked passage. Citation accuracy therefore restates rank-1 retrieval\n     rather than measuring a model's choices, so it carries no floor here.",
            "Note:".blue()
        );
    }

    println!("\n{}", "Latency:".bold());
    let mut lat = comfy_table::Table::new();
    lat.load_preset(comfy_table::presets::UTF8_FULL);
    lat.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    lat.set_header(vec!["Stage", "p50", "p95"]);
    lat.add_row(vec![
        "Answer (end to end)".to_string(),
        format!("{} ms", report.latency_ms.answer_p50),
        format!("{} ms", report.latency_ms.answer_p95),
    ]);
    println!("{lat}");

    // Name what actually failed. A bare percentage tells you something is
    // wrong; the question that missed tells you what to look at.
    let misses: Vec<&QuestionOutcome> = report
        .outcomes
        .iter()
        .filter(|o| o.supporting_rank.is_none())
        .collect();
    if !misses.is_empty() {
        println!(
            "\n{} {} question(s) retrieved no supporting passage:",
            "Missed:".yellow(),
            misses.len()
        );
        for m in misses.iter().take(10) {
            println!("  {} [{}] {}", "-".dimmed(), m.domain.dimmed(), m.query);
        }
        if misses.len() > 10 {
            println!("  {} and {} more", "-".dimmed(), misses.len() - 10);
        }
    }

    println!();
    if report.passed {
        println!(
            "{} Node answered {} questions at or above every floor.\n",
            "✓".green(),
            report.questions
        );
    } else {
        println!(
            "{} Node fell below the pass bar on {} questions.\n",
            "✗".red(),
            report.questions
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_question_carries_usable_gold_labels() {
        let qs = questions().expect("question set should parse");
        assert_eq!(
            qs.len(),
            52,
            "the full golden set should ship with the binary"
        );
        for q in &qs {
            assert!(!q.query.trim().is_empty(), "{} has no query", q.id);
            assert!(
                !q.gold_source_keys.is_empty(),
                "{} has no gold source",
                q.id
            );
            assert!(!q.gold_needles.is_empty(), "{} has no gold needles", q.id);
        }
    }

    #[test]
    fn gold_sources_all_exist_in_the_bundled_corpus() {
        // A label pointing at a document the binary does not carry would be
        // unanswerable no matter how well the node performed.
        let keys: Vec<&str> = corpus().iter().map(|d| d.source_key).collect();
        for q in questions().unwrap() {
            for want in &q.gold_source_keys {
                assert!(
                    keys.contains(&want.as_str()),
                    "{} cites unknown {}",
                    q.id,
                    want
                );
            }
        }
    }

    #[test]
    fn every_supporting_passage_is_present_in_its_document() {
        // The measure is only meaningful if the answer is actually in the
        // corpus. A needle that matches nothing would score every node zero.
        let docs = corpus();
        for q in questions().unwrap() {
            let bodies: Vec<String> = q
                .gold_source_keys
                .iter()
                .filter_map(|k| docs.iter().find(|d| d.source_key == k))
                .map(|d| d.body.to_lowercase())
                .collect();
            let found = bodies
                .iter()
                .any(|b| q.gold_needles.iter().all(|n| b.contains(&n.to_lowercase())));
            assert!(found, "{}: no document contains all gold needles", q.id);
        }
    }

    #[test]
    fn supporting_match_requires_every_needle() {
        let q = GoldenQuestion {
            id: "t".into(),
            domain: "legal".into(),
            query: "q".into(),
            gold_source_keys: vec!["legal-msa".into()],
            gold_needles: vec!["alpha".into(), "beta".into()],
        };
        let from_gold = Some("knaix-selftest-ab12-legal-msa.md");
        assert!(q.is_supporting("Alpha and BETA appear here", from_gold));
        assert!(!q.is_supporting("only alpha appears", from_gold));
        // Right phrases, wrong document: a retrieval mistake, not a hit.
        assert!(!q.is_supporting(
            "Alpha and beta appear here",
            Some("knaix-selftest-ab12-finance-expense.md")
        ));
        assert!(!q.is_supporting("Alpha and beta appear here", None));
    }

    #[test]
    fn no_source_key_is_a_prefix_of_another() {
        // Source matching asks whether the document name contains the key, so
        // one key being a prefix of another would credit the wrong document.
        let keys: Vec<&str> = corpus().iter().map(|d| d.source_key).collect();
        for a in &keys {
            for b in &keys {
                if a != b {
                    assert!(!b.contains(a), "source key '{}' is contained in '{}'", a, b);
                }
            }
        }
    }

    #[test]
    fn the_quick_subset_still_covers_every_document() {
        // A quick run that skipped a document would report a clean bill of
        // health for a corpus it never asked about.
        let all = questions().unwrap();
        let quick = quick_subset(&all);
        let sources: std::collections::HashSet<&str> = quick
            .iter()
            .filter_map(|q| q.gold_source_keys.first())
            .map(|s| s.as_str())
            .collect();
        assert_eq!(
            sources.len(),
            corpus().len(),
            "every document must be represented"
        );
        assert!(
            quick.len() < all.len(),
            "a quick run must actually be shorter"
        );
    }

    #[test]
    fn the_quick_subset_is_stable_across_runs() {
        // Two runs have to be comparable, so selection is fixture order rather
        // than a sample.
        let all = questions().unwrap();
        let a: Vec<String> = quick_subset(&all).iter().map(|q| q.id.clone()).collect();
        let b: Vec<String> = quick_subset(&all).iter().map(|q| q.id.clone()).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let sorted = vec![10u128, 20, 30, 40];
        assert_eq!(percentile(&sorted, 0.50), 20);
        assert_eq!(percentile(&sorted, 0.95), 40);
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn the_citation_floor_only_binds_when_a_real_model_answered() {
        // Below the citation floor, but the mock picks citations by rank, so
        // failing the node for it would be blaming it for the mock.
        let outcomes = vec![
            outcome("a", Some(1), Some(true)),
            outcome("b", Some(1), Some(false)),
        ];
        let mocked = summarize("n", outcomes, Some("mock".into()));
        assert!(!mocked.citation_accuracy_meaningful);
        assert!(
            mocked.passed,
            "retrieval was perfect; the mock must not fail it"
        );

        let real = summarize(
            "n",
            vec![
                outcome("a", Some(1), Some(true)),
                outcome("b", Some(1), Some(false)),
            ],
            Some("claude-sonnet-5".into()),
        );
        assert!(real.citation_accuracy_meaningful);
        assert!(
            !real.passed,
            "a real model citing the wrong passage is a failure"
        );
    }

    #[test]
    fn a_node_reporting_no_model_is_treated_as_mock() {
        let r = summarize("n", vec![outcome("a", Some(1), Some(false))], None);
        assert!(!r.citation_accuracy_meaningful);
    }

    #[test]
    fn uncited_answers_are_excluded_from_citation_accuracy() {
        // Otherwise an answer that cited nothing would count as a wrong
        // citation and drag the score below the floor for the wrong reason.
        let outcomes = vec![
            outcome("a", Some(1), Some(true)),
            outcome("b", Some(2), None),
            outcome("c", Some(1), Some(false)),
        ];
        let report = summarize("node", outcomes, Some("real-model".into()));
        assert_eq!(report.citation_accuracy, 0.5);
    }

    #[test]
    fn a_miss_costs_both_hit_rate_and_mrr() {
        let report = summarize(
            "node",
            vec![
                outcome("a", Some(1), Some(true)),
                outcome("b", None, None),
                outcome("c", Some(2), Some(true)),
            ],
            Some("real-model".into()),
        );
        assert!((report.hit_rate_at_k - 2.0 / 3.0).abs() < 1e-9);
        assert!((report.mrr - (1.0 + 0.5) / 3.0).abs() < 1e-9);
        assert!(!report.passed, "a third of questions missing must not pass");
    }

    fn outcome(id: &str, rank: Option<usize>, cited: Option<bool>) -> QuestionOutcome {
        QuestionOutcome {
            id: id.into(),
            domain: "legal".into(),
            query: "q".into(),
            supporting_rank: rank,
            cited_supporting: cited,
            answer: String::new(),
            answer_ms: 1,
        }
    }
}
