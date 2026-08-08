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

use crate::exit::{Code, WithCode};
use crate::nodes::{Citation, KnaixContext, Target};
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

/// The bar a node is held to. Do not lower one to make a red run green: the
/// point of a floor is that falling under it is the finding.
///
/// Only one of these three moves with the model. `supporting_rank`, which feeds
/// both hit rate and MRR, is taken over every passage the node returned,
/// without regard to which ones the answer cited -- that is the output of the
/// node's own embedder and reranker, and it is the same whichever model the
/// user brought. Measured directly: the deterministic mock and a real model
/// return an identical passage list, in an identical order, for the same
/// question against the same store. Only the cited flags differ.
///
/// So retrieval floors are shared, and only the citation floor is relaxed for a
/// node answering from a model the user runs themselves. Relaxing the retrieval
/// floors would hide a reranker that orders badly, which is the one thing MRR
/// exists to report.
struct Floors {
    /// Named in the report, so which bar was applied is never a guess.
    class: &'static str,
    hit_rate: f64,
    mrr: f64,
    citation_accuracy: f64,
}

/// The platform's own eval floors, for the models it runs itself.
const FLOORS_HOSTED: Floors = Floors {
    class: "hosted",
    hit_rate: 0.95,
    mrr: 0.90,
    citation_accuracy: 0.95,
};

/// A node answering from a model the user brought and runs themselves.
///
/// Retrieval floors match the hosted ones, because retrieval does not depend on
/// which model answers. Only the citation floor moves.
///
/// PROVISIONAL, and only the citation number. Calibrated against one model
/// (gemma4 on Apple silicon cited the supporting passage 89.8% of the time over
/// the full 52), which is enough to show a small model cites more loosely than
/// a frontier one and not enough to say where the line belongs. Widen the
/// sample before treating it as settled.
const FLOORS_LOCAL: Floors = Floors {
    class: "local",
    hit_rate: 0.95,
    mrr: 0.90,
    citation_accuracy: 0.85,
};

impl Floors {
    /// Which bar applies. A local node answers from whatever model the user
    /// pointed it at; a hosted one answers from the platform's.
    fn for_target(target: &Target) -> &'static Floors {
        if target.is_local() {
            &FLOORS_LOCAL
        } else {
            &FLOORS_HOSTED
        }
    }
}

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
    /// Why the node produced no answer, when it produced none.
    ///
    /// One slow generation used to end the whole run and throw away every
    /// question already asked. A model the user brought is allowed to be slow,
    /// so the question is recorded as unanswered and the run carries on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(serde::Serialize)]
pub struct SelfTestReport {
    pub node: String,
    pub model: Option<String>,
    /// Whether the citation number can be read as a measurement of a model.
    ///
    /// False when the deterministic mock answered, where it only restates
    /// rank-1 retrieval, and false when nothing answered at all, where no model
    /// chose anything. Both report no model, so this must not be read as "the
    /// mock ran".
    pub citation_accuracy_meaningful: bool,
    pub questions: usize,
    /// Which floors were applied, so a result is never read against the wrong
    /// bar.
    pub floor_class: String,
    /// Whether this run scores the node at all.
    ///
    /// False for `--quick`, which asks 12 of 52 and takes the first two per
    /// document rather than sampling. At that size the interval around a ~90%
    /// rate is wider than the gap between passing and failing, so a verdict
    /// from it would be a coin toss wearing a floor's clothes.
    pub scored: bool,
    /// Questions the node never answered. Counted in every rate below, because
    /// a node that cannot answer has not retrieved anything either, and scoring
    /// only what survived would let a node time out its way to a pass.
    pub unanswered: usize,
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
pub(crate) fn percentile(sorted: &[u128], p: f64) -> u128 {
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
    target: &Target,
    keep: bool,
    quick: bool,
    sweep: bool,
) -> Result<()> {
    let node_uuid = &target.label();
    let all = questions()?;
    let questions = if quick { quick_subset(&all) } else { all };
    // Quiet hides the spinner for the same reason JSON does: it is commentary,
    // and the result prints either way.
    let human = ctx.output_format != "json" && !ctx.quiet;

    // A run interrupted before cleanup leaves its corpus behind, and the next
    // run would then measure against two copies of every document. Report that
    // rather than assuming ownership of documents this run did not create.
    let leftovers = list_selftest_documents(ctx, target).await?;
    if !leftovers.is_empty() {
        if sweep {
            let swept = sweep_previous(ctx, target).await?;
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
            "  {} synthetic documents, {} questions{}. Everything uploaded is deleted afterwards.\n",
            corpus().len(),
            questions.len(),
            if quick { " (quick subset)" } else { "" }
        );
    }

    let pb = spinner(human);
    pb.set_message("Ingesting the self-test corpus...");

    // Track ids as we go: a failure part-way through still has to clean up what
    // it managed to create.
    let mut document_ids: Vec<String> = Vec::new();
    let ingest = ingest_corpus(ctx, target, &run_id, &mut document_ids).await;
    if let Err(e) = ingest {
        pb.finish_and_clear();
        cleanup(ctx, target, &document_ids, keep, human).await;
        return Err(e);
    }

    let (outcomes, model) = match run_questions(ctx, target, &questions, &pb, human).await {
        Ok(o) => o,
        Err(e) => {
            pb.finish_and_clear();
            cleanup(ctx, target, &document_ids, keep, human).await;
            return Err(e);
        }
    };
    pb.finish_and_clear();

    cleanup(ctx, target, &document_ids, keep, human).await;

    let floors = Floors::for_target(target);
    let report = summarize(node_uuid, outcomes, model, floors, !quick);
    if human {
        print_report(&report, floors);
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }

    // A run that did not score the node cannot fail it either. The node still
    // has to have answered: that is a fact about the machine, not a judgement
    // about quality, and it is the one thing a quick run does establish.
    if !report.scored && report.unanswered == 0 {
        return Ok(());
    }

    if report.passed {
        Ok(())
    } else if report.unanswered > 0 {
        // The floors are beside the point when the node did not answer: it is
        // not that retrieval was weak, it is that there was nothing to score.
        Err(anyhow!(
            "Self-test incomplete: the node did not answer {} of {} questions. The scores above cover only what it answered.",
            report.unanswered,
            report.questions
        ))
        .coded(Code::Unavailable)
    } else {
        Err(anyhow!(
            "Self-test below the {} pass bar: hit-rate {:.0}% (floor {:.0}%), MRR {:.2} (floor {:.2}), citation accuracy {:.0}% (floor {:.0}%)",
            floors.class,
            report.hit_rate_at_k * 100.0,
            floors.hit_rate * 100.0,
            report.mrr,
            floors.mrr,
            report.citation_accuracy * 100.0,
            floors.citation_accuracy * 100.0
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

/// Ingest one document of generated text, hosted or local, and return the id the
/// node filed it under.
///
/// Shared with `knaix bench`, which needs the same "put a known document on a
/// node and take it away again" move. The id can be absent: a node that accepted
/// the document without reporting one leaves nothing to delete later, and saying
/// so is more honest than inventing an id that will fail to delete.
pub(crate) async fn ingest_text(
    ctx: &KnaixContext,
    target: &Target,
    filename: &str,
    body: &str,
) -> Result<Option<String>> {
    if let Target::Local { base, instance_id } = target {
        // The node parses, chunks and embeds it itself; there is no control
        // plane in the path and no credential to present.
        let resp = ctx
            .client
            .post(format!("{}/api/kb/ingest", base))
            .json(&serde_json::json!({
                "instance_id": instance_id,
                "text": body,
                "filename": filename,
            }))
            .send()
            .await
            .context("Failed to reach the local node")?;

        let status = resp.status();
        let payload: serde_json::Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "Could not ingest {}: HTTP {} - {}",
                filename,
                status,
                payload["message"].as_str().unwrap_or("unknown error")
            ));
        }
        return Ok(payload["documentId"].as_str().map(|s| s.to_string()));
    }

    let node_uuid = target.label();
    let token = ctx.get_token()?;
    let url = format!(
        "{}/api/knowledge/{}/documents",
        ctx.config.api_url, node_uuid
    );

    let part = multipart::Part::text(body.to_string())
        .file_name(filename.to_string())
        .mime_str("text/markdown")?;
    let form = multipart::Form::new().part("file", part);

    let resp = ctx
        .client
        .post(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .multipart(form)
        .send()
        .await
        .context("Failed to upload a generated document")?;

    let status = resp.status();
    let payload: serde_json::Value = resp.json().await.unwrap_or_default();
    if !status.is_success() || !payload["success"].as_bool().unwrap_or(false) {
        return Err(anyhow!(
            "Could not ingest {}: HTTP {} - {}",
            filename,
            status,
            payload["error"].as_str().unwrap_or("unknown error")
        ));
    }
    Ok(payload["data"]["documentId"]
        .as_str()
        .map(|s| s.to_string()))
}

async fn ingest_corpus(
    ctx: &KnaixContext,
    target: &Target,
    run_id: &str,
    document_ids: &mut Vec<String>,
) -> Result<()> {
    for doc in corpus() {
        let name = document_name(run_id, doc.source_key);
        if let Some(id) = ingest_text(ctx, target, &name, doc.body).await? {
            document_ids.push(id);
        }
    }
    Ok(())
}

async fn run_questions(
    ctx: &KnaixContext,
    target: &Target,
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
        let answer = match ask(ctx, target, &q.query).await {
            Ok(a) => a,
            // One question the node could not answer is a result, not the end
            // of the run. Recording it keeps the other fifty-one measurements
            // and names the cause, which a bare abort threw away.
            Err(e) => {
                outcomes.push(QuestionOutcome {
                    id: q.id.to_string(),
                    domain: q.domain.to_string(),
                    query: q.query.to_string(),
                    supporting_rank: None,
                    cited_supporting: None,
                    answer: String::new(),
                    answer_ms: started.elapsed().as_millis(),
                    error: Some(root_cause(&e)),
                });
                continue;
            }
        };
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
            error: None,
        });
    }

    Ok((outcomes, model))
}

/// The innermost thing that went wrong, which is the part worth printing.
///
/// The chain above it restates the request; the end of it says the node timed
/// out, or refused, or was not there.
fn root_cause(e: &anyhow::Error) -> String {
    e.chain().last().map(|c| c.to_string()).unwrap_or_default()
}

/// Ask one question, surfacing a rate limit as the thing it is.
///
/// A run costs one request per question plus a handful for setup, against an
/// account-wide budget shared with everything else the user is doing. A raw
/// "HTTP 429" mid-run reads as a broken node rather than a spent budget.
async fn ask(ctx: &KnaixContext, target: &Target, query: &str) -> Result<crate::nodes::ChatAnswer> {
    // Each self-test question stands alone; no conversation history to carry,
    // and the default verbosity keeps the graded answers comparable.
    match crate::nodes::chat(
        ctx,
        target,
        query,
        crate::nodes::Echo::Silent,
        &[],
        &crate::nodes::AnswerOptions::default(),
        None,
    )
    .await
    {
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
async fn sweep_previous(ctx: &KnaixContext, target: &Target) -> Result<usize> {
    let stale = list_selftest_documents(ctx, target).await?;
    let mut removed = 0;
    for id in stale {
        if delete_document(ctx, target, &id).await.is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

async fn list_selftest_documents(ctx: &KnaixContext, target: &Target) -> Result<Vec<String>> {
    list_documents_with_prefix(ctx, target, DOC_PREFIX).await
}

/// Document ids on a node whose filename starts with `prefix`.
///
/// Shared with `knaix bench`: both commands generate documents under a prefix
/// of their own and both need to find the ones an interrupted run left behind.
///
/// A local node answers this from its own store. It used to return nothing here
/// and the sweep quietly did nothing, which left the only remedy on offer being
/// to empty the whole store over one synthetic document.
pub(crate) async fn list_documents_with_prefix(
    ctx: &KnaixContext,
    target: &Target,
    prefix: &str,
) -> Result<Vec<String>> {
    if let Target::Local { base, instance_id } = target {
        let documents = crate::nodes::local_documents(ctx, base, instance_id).await?;
        // The source name, never the display label: the label falls back to the
        // document id, and these ids are handed to a delete. A document with no
        // recorded name is not one this generated, and the hosted branch below
        // declines it for the same reason.
        return Ok(documents
            .into_iter()
            .filter(|d| {
                d.source
                    .as_deref()
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .map(|d| d.document_id)
            .collect());
    }

    let node_uuid = target.label();
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
                .map(|n| n.starts_with(prefix))
                .unwrap_or(false)
        })
        .filter_map(|d| d["id"].as_str().map(|s| s.to_string()))
        .collect())
}

/// Remove a document by id, hosted or local. Shared with `knaix bench`, which
/// has the same obligation to leave a node with the corpus it started with.
pub(crate) async fn delete_document(
    ctx: &KnaixContext,
    target: &Target,
    document_id: &str,
) -> Result<()> {
    if let Target::Local { base, instance_id } = target {
        let resp = ctx
            .client
            .post(format!("{}/api/kb/delete", base))
            .json(&serde_json::json!({
                "instance_id": instance_id,
                "document_id": document_id,
            }))
            .send()
            .await
            .context("Failed to reach the local node")?;
        return if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("Delete failed: HTTP {}", resp.status()))
        };
    }

    let node_uuid = target.label();
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
    target: &Target,
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
                crate::brand::cmd("selftest --sweep")
            );
        }
        return;
    }

    let mut failed = Vec::new();
    for id in document_ids {
        if delete_document(ctx, target, id).await.is_err() {
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
    floors: &Floors,
    scored: bool,
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

    let unanswered = outcomes.iter().filter(|o| o.error.is_some()).count();

    // Latency describes answers, so a question that produced none contributes
    // no timing. Including the wait before a timeout would report the timeout
    // as the node's p95 and make a slow node look like a fast one that failed.
    let mut answer: Vec<u128> = outcomes
        .iter()
        .filter(|o| o.error.is_none())
        .map(|o| o.answer_ms)
        .collect();
    answer.sort_unstable();

    let hit_rate_at_k = if total == 0 {
        0.0
    } else {
        hits as f64 / total as f64
    };
    // `+ 0.0` is not redundant. Summing no f64 yields -0.0, because that is
    // floating-point addition's identity, and a run where nothing was retrieved
    // then reports its MRR as "-0.000".
    let mrr = if total == 0 {
        0.0
    } else {
        mrr_sum / total as f64 + 0.0
    };

    // Two ways the number cannot be read, and they are not the same thing. The
    // mock picks the top passage every time, so its accuracy restates rank-1
    // retrieval. A run where nothing answered reports no model at all, which
    // looks identical here and is not: that model never got to choose. Both
    // make the number unreadable, so neither may claim it is meaningful.
    let answered_any = unanswered < total;
    let citation_accuracy_meaningful = answered_any && !is_mock_model(model.as_deref());

    SelfTestReport {
        node: node_uuid.to_string(),
        model,
        citation_accuracy_meaningful,
        questions: total,
        floor_class: floors.class.to_string(),
        scored,
        unanswered,
        hit_rate_at_k,
        mrr,
        citation_accuracy,
        top_k: TOP_K,
        // Retrieval floors always apply. The citation floor only does when a
        // real model chose the citations; enforcing it against the mock would
        // fail a healthy node for the mock's behaviour, which is the kind of
        // red result that teaches people to ignore the command.
        // An unanswered question is never a pass. Its cause is the node, not
        // the corpus, and reporting green on a run that could not finish is the
        // one result this command must never give.
        passed: scored
            && unanswered == 0
            && hit_rate_at_k >= floors.hit_rate
            && mrr >= floors.mrr
            && (!citation_accuracy_meaningful || citation_accuracy >= floors.citation_accuracy),
        latency_ms: LatencySummary {
            answer_p50: percentile(&answer, 0.50),
            answer_p95: percentile(&answer, 0.95),
        },
        outcomes,
    }
}

fn print_report(report: &SelfTestReport, floors: &Floors) {
    // An unscored run still prints its numbers; what it must not print is a
    // verdict, because the sample it drew cannot support one.
    let verdict = |value: f64, floor: f64| {
        if !report.scored {
            "--".dimmed()
        } else if value >= floor {
            "PASS".green()
        } else {
            "FAIL".red()
        }
    };

    println!(
        "{} {}",
        "Answer quality:".bold(),
        format!("(against the {} floors)", floors.class).dimmed()
    );
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.set_header(vec!["Metric", "Result", "Floor", ""]);
    table.add_row(vec![
        format!("Hit rate @{}", report.top_k),
        format!("{:.1}%", report.hit_rate_at_k * 100.0),
        format!("{:.0}%", floors.hit_rate * 100.0),
        verdict(report.hit_rate_at_k, floors.hit_rate).to_string(),
    ]);
    table.add_row(vec![
        "MRR".to_string(),
        format!("{:.3}", report.mrr),
        format!("{:.2}", floors.mrr),
        verdict(report.mrr, floors.mrr).to_string(),
    ]);
    table.add_row(vec![
        "Citation accuracy".to_string(),
        format!("{:.1}%", report.citation_accuracy * 100.0),
        if report.citation_accuracy_meaningful {
            format!("{:.0}%", floors.citation_accuracy * 100.0)
        } else {
            "n/a".to_string()
        },
        if report.citation_accuracy_meaningful {
            verdict(report.citation_accuracy, floors.citation_accuracy).to_string()
        } else {
            "INFO".dimmed().to_string()
        },
    ]);
    println!("{table}");

    if !report.scored {
        println!(
            "{} A quick run asks {} of the {} questions and does not score the node.\n     Run {} without {} for a verdict.",
            "Note:".blue(),
            report.questions,
            questions().map(|q| q.len()).unwrap_or(0),
            crate::brand::cmd("selftest"),
            "--quick".cyan()
        );
    }

    // A run where nothing answered reports no model, which is not the same as
    // reporting the mock. Claiming the mock there tells the reader their model
    // was ignored when in fact it never got to speak.
    if !report.citation_accuracy_meaningful && report.unanswered < report.questions {
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
        crate::nodes::format_duration_ms(report.latency_ms.answer_p50),
        crate::nodes::format_duration_ms(report.latency_ms.answer_p95),
    ]);
    println!("{lat}");

    // Name what actually failed. A bare percentage tells you something is
    // wrong; the question that missed tells you what to look at.
    // Separated from the misses below: a question the node never answered says
    // nothing about retrieval, and listing it as one sends the reader to tune a
    // corpus when the model was the thing that gave up.
    let unanswered: Vec<&QuestionOutcome> = report
        .outcomes
        .iter()
        .filter(|o| o.error.is_some())
        .collect();
    if !unanswered.is_empty() {
        println!(
            "\n{} {} question(s) the node never answered:",
            "Unanswered:".red(),
            unanswered.len()
        );
        for u in unanswered.iter().take(5) {
            println!(
                "  {} [{}] {}",
                "-".dimmed(),
                u.domain.dimmed(),
                u.error.as_deref().unwrap_or("no answer")
            );
        }
        if unanswered.len() > 5 {
            println!("  {} and {} more", "-".dimmed(), unanswered.len() - 5);
        }
        println!(
            "  {}",
            "A model that needs longer: knaix local up --generation-timeout <SECONDS>.".dimmed()
        );
    }

    let misses: Vec<&QuestionOutcome> = report
        .outcomes
        .iter()
        .filter(|o| o.error.is_none() && o.supporting_rank.is_none())
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
    if !report.scored && report.unanswered == 0 {
        // Neither pass nor fail. A quick run establishes that the node answers,
        // and saying anything stronger is the thing this change exists to stop.
        println!(
            "{} Node answered all {} questions. Not scored: this was a quick run.\n",
            "✓".green(),
            report.questions
        );
    } else if report.passed {
        println!(
            "{} Node answered {} questions at or above every floor.\n",
            "✓".green(),
            report.questions
        );
    } else if report.unanswered > 0 {
        // Not a quality verdict. Saying it fell below the bar would send the
        // reader to look at retrieval when the node never got that far.
        println!(
            "{} Node did not answer {} of {} questions, so this run does not score it.\n",
            "✗".red(),
            report.unanswered,
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

    /// Every test below predates the per-class floors and was written against
    /// the hosted bar, which is also the bar a regression would show up in.
    fn scored_summary(
        node: &str,
        outcomes: Vec<QuestionOutcome>,
        model: Option<String>,
    ) -> SelfTestReport {
        summarize(node, outcomes, model, &FLOORS_HOSTED, true)
    }

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
        let mocked = scored_summary("n", outcomes, Some("mock".into()));
        assert!(!mocked.citation_accuracy_meaningful);
        assert!(
            mocked.passed,
            "retrieval was perfect; the mock must not fail it"
        );

        let real = scored_summary(
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
        let r = scored_summary("n", vec![outcome("a", Some(1), Some(false))], None);
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
        let report = scored_summary("node", outcomes, Some("real-model".into()));
        assert_eq!(report.citation_accuracy, 0.5);
    }

    #[test]
    fn a_miss_costs_both_hit_rate_and_mrr() {
        let report = scored_summary(
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
            error: None,
        }
    }

    fn unanswered(id: &str, cause: &str) -> QuestionOutcome {
        QuestionOutcome {
            error: Some(cause.into()),
            ..outcome(id, None, None)
        }
    }

    /// A run that could not finish is never green, whatever the questions it
    /// did manage to ask scored.
    #[test]
    fn an_unanswered_question_fails_the_run_and_is_counted() {
        let report = scored_summary(
            "node",
            vec![
                outcome("a", Some(1), Some(true)),
                outcome("b", Some(1), Some(true)),
                unanswered("c", "Local generation timed out"),
            ],
            Some("real-model".into()),
        );
        assert_eq!(report.unanswered, 1);
        assert!(
            !report.passed,
            "a node that did not answer must not report green"
        );
        // Counted in the denominator: scoring only what survived would let a
        // node time out its way to a pass.
        assert!((report.hit_rate_at_k - 2.0 / 3.0).abs() < 1e-9);
    }

    /// Summing no f64 gives -0.0, which reached the table as "-0.000".
    #[test]
    fn a_run_that_retrieved_nothing_reports_positive_zero() {
        let report = scored_summary("node", vec![unanswered("a", "timed out")], None);
        assert!(
            report.mrr.is_sign_positive(),
            "MRR rendered as negative zero: {:?}",
            report.mrr
        );
        assert_eq!(format!("{:.3}", report.mrr), "0.000");
    }

    /// A node that never answered reported no model, which was then read as
    /// the mock and told the user their model had been ignored.
    #[test]
    fn a_fully_unanswered_run_is_not_reported_as_the_mock() {
        let report = scored_summary("node", vec![unanswered("a", "timed out")], None);
        assert_eq!(report.unanswered, report.questions);
        assert!(
            !report.citation_accuracy_meaningful,
            "no model chose anything, so the number cannot be read as one"
        );
    }

    /// Retrieval does not depend on the model, so relaxing its floors for a
    /// local node would hide a reranker that orders badly. Read through
    /// `for_target` rather than off the constants, so this also pins which bar
    /// each kind of node is held to.
    #[test]
    fn only_the_citation_floor_differs_between_classes() {
        let local = Floors::for_target(&Target::Local {
            base: "http://127.0.0.1:8080".into(),
            instance_id: "abc".into(),
        });
        let hosted = Floors::for_target(&Target::Remote { uuid: "u-1".into() });

        assert_eq!(local.class, "local");
        assert_eq!(hosted.class, "hosted");
        assert_eq!(
            local.hit_rate, hosted.hit_rate,
            "retrieval does not depend on the model"
        );
        assert_eq!(
            local.mrr, hosted.mrr,
            "MRR reports the node's reranker, not the user's model"
        );
        assert!(
            local.citation_accuracy < hosted.citation_accuracy,
            "the model's own citing is the only thing that differs"
        );
    }

    /// The wait before a timeout is not a latency measurement.
    #[test]
    fn a_timed_out_question_does_not_enter_the_latency_summary() {
        let mut slow = unanswered("c", "Local generation timed out");
        slow.answer_ms = 600_000;
        let report = scored_summary("node", vec![outcome("a", Some(1), Some(true)), slow], None);
        assert_eq!(report.latency_ms.answer_p50, 1);
        assert_eq!(report.latency_ms.answer_p95, 1);
    }
}
