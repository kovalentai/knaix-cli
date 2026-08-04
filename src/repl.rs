use crate::brand;
use crate::nodes::KnaixContext;
use anyhow::{Context, Result};
use colored::*;
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::history::DefaultHistory;
use rustyline::{Completer, Editor, Helper, Hinter, Validator};
use std::borrow::Cow::{self, Borrowed, Owned};
use termimad::MadSkin;

/// The prompt as rustyline is given it, with no escapes in it.
///
/// The painted version comes back from the highlighter at render time instead
/// of being passed to `readline()`. Both spellings measure the same today
/// rustyline's width calculation skips escape sequences (`src/tty/mod.rs`,
/// ("ignore ANSI escape sequence"), but `highlight_prompt` is the API built for
/// this, and it keeps the string we measure and the string we print from having
/// to agree by coincidence.
fn plain_prompt(node_id: &str) -> String {
    format!("{} [{}]> ", brand::WORDMARK, node_id)
}

/// Paints the wordmark in the prompt, and again in the line as it is typed.
///
/// The other three traits are what `Helper` requires; none of them do anything
/// here, so they come from the derive rather than being written out.
#[derive(Completer, Helper, Hinter, Validator)]
struct ReplHelper {
    /// The prompt with the wordmark painted, or `None` when colour is off.
    /// `None` makes every method below a pass-through, so there is one place
    /// the decision is made rather than a check in each of them.
    painted_prompt: Option<String>,
    /// The wordmark painted once, reused to repaint it inside the typed line.
    /// Held rather than recomputed because `highlight` runs on every keystroke
    /// and `brand::wordmark()` reads the environment each call.
    painted_wordmark: Option<String>,
}

impl ReplHelper {
    /// The one colour decision, taken from brand rather than a second detection
    /// path. NO_COLOR, a pipe, and KNAIX_COLOR=none all resolve to `None` there.
    fn new(node_id: &str) -> Self {
        Self::at(brand::level(), node_id)
    }

    fn at(lvl: brand::Level, node_id: &str) -> Self {
        if lvl == brand::Level::None {
            return Self {
                painted_prompt: None,
                painted_wordmark: None,
            };
        }
        Self {
            painted_prompt: Some(brand::cmd_at(lvl, &format!("[{}]> ", node_id))),
            painted_wordmark: Some(brand::wordmark_at(lvl)),
        }
    }
}

impl Highlighter for ReplHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        match &self.painted_prompt {
            // `default` is false for rustyline's own prompts, such as the
            // reverse-search one. Painting those would put our node label in
            // front of a prompt that is not ours.
            Some(painted) if default => Borrowed(painted.as_str()),
            _ => Borrowed(prompt),
        }
    }

    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        match &self.painted_wordmark {
            // Only escapes are added, and rustyline measures those as
            // zero-width, so the highlighted line keeps the display width the
            // trait requires of it.
            Some(painted) if line.contains(brand::WORDMARK) => {
                Owned(line.replace(brand::WORDMARK, painted))
            }
            _ => Borrowed(line),
        }
    }

    fn highlight_char(&self, line: &str, _pos: usize, kind: CmdKind) -> bool {
        // Without this, `highlight` is never called: the default is false.
        if self.painted_wordmark.is_none() || kind == CmdKind::ForcedRefresh {
            return false;
        }
        line.contains(brand::WORDMARK)
    }
}

fn print_help() {
    println!("\n{}", "REPL commands:".bold().underline());
    let rows: &[(&str, &str)] = &[
        ("/help", "Show this help"),
        (
            "/remember <fact>",
            "Save a fact to this node's notes and ingest it",
        ),
        ("/memory", "List the notes saved for this node"),
        ("/reset", "Forget the conversation so far and start fresh"),
        (
            "/brief, /normal, /detailed",
            "Set how much detail answers carry (local node)",
        ),
        ("/exit, /quit", "End the session (Ctrl-D works too)"),
    ];
    for (cmd, what) in rows {
        println!("  {:<18} {}", cmd.cyan(), what);
    }
    println!();
}

/// Save a fact and ingest it into the node, reporting exactly what happened.
/// A note that only reached the disk must say so, or "saved" overstates it.
async fn remember(ctx: &KnaixContext, target: &crate::nodes::Target, fact: &str) {
    let path = match crate::nodes::save_note(target, fact).await {
        Ok(p) => p,
        Err(e) => {
            println!("{} Could not save the note: {}", "Error:".red(), e);
            return;
        }
    };
    match crate::nodes::ingest_note(ctx, target, &path).await {
        Ok(()) => println!(
            "{} Noted. Saved to {} and ingested, so later questions can retrieve it.",
            "✓".green(),
            path.display().to_string().dimmed()
        ),
        Err(e) => println!(
            "{} Noted. Saved to {}, but the node did not ingest it ({}); it will not surface in answers.",
            "✓".yellow(),
            path.display().to_string().dimmed(),
            e
        ),
    }
}

pub async fn run(ctx: &KnaixContext, target: &crate::nodes::Target) -> Result<()> {
    let node_id = &target.label();
    let mut rl: Editor<ReplHelper, DefaultHistory> =
        Editor::new().context("Failed to initialize readline")?;
    rl.set_helper(Some(ReplHelper::new(node_id)));
    let skin = MadSkin::default_dark();

    println!(
        "\n{} {} session with {}. {} lists commands; {} ends the session.",
        "●".green(),
        crate::brand::wordmark(),
        node_id.cyan().bold(),
        "/help".cyan(),
        "/exit".cyan()
    );

    let mut message_count = 0;
    // The conversation so far, sent with each question so a follow-up is
    // answered in context. Trimmed to a recent window so a long session does
    // not grow the request without bound.
    let mut history: Vec<crate::nodes::ChatTurn> = Vec::new();
    // How much detail answers carry, adjustable mid-session with /brief etc.
    let mut verbosity = crate::nodes::Verbosity::Normal;

    // The node does not change mid-session, so the prompt is built once.
    let prompt = plain_prompt(node_id);

    loop {
        let readline = rl.readline(&prompt);

        match readline {
            Ok(line) => {
                let input: String = line.trim().to_string();
                if input.is_empty() {
                    continue;
                }
                rl.add_history_entry(input.as_str()).ok();

                if input.starts_with('/') {
                    let (cmd, args) = input.split_once(' ').unwrap_or((input.as_str(), ""));
                    match cmd {
                        "/exit" | "/quit" => break,
                        "/help" => {
                            print_help();
                            continue;
                        }
                        "/remember" => {
                            let fact = args.trim();
                            if fact.is_empty() {
                                println!("{} Usage: /remember <fact>", "Error:".red());
                            } else {
                                remember(ctx, target, fact).await;
                            }
                            continue;
                        }
                        "/memory" => {
                            let key = crate::nodes::memory_key(target);
                            if let Err(e) = crate::nodes::view_memory(ctx, &key, None).await {
                                println!("{} {}", "Error:".red(), e);
                            }
                            continue;
                        }
                        "/reset" => {
                            let had = !history.is_empty();
                            history.clear();
                            if had {
                                println!(
                                    "{} Conversation cleared. The next question starts fresh.",
                                    "✓".green()
                                );
                            } else {
                                println!("{} Nothing to clear yet.", "Info:".blue());
                            }
                            continue;
                        }
                        "/brief" | "/normal" | "/detailed" => {
                            verbosity = match cmd {
                                "/brief" => crate::nodes::Verbosity::Brief,
                                "/detailed" => crate::nodes::Verbosity::Detailed,
                                _ => crate::nodes::Verbosity::Normal,
                            };
                            println!(
                                "{} Answers are now {}.",
                                "✓".green(),
                                cmd.trim_start_matches('/').cyan()
                            );
                            continue;
                        }
                        _ => {
                            println!(
                                "{} Unknown command {}. {} lists commands; a message without a leading '/' is sent to the node.",
                                "Error:".red(),
                                cmd.cyan(),
                                "/help".cyan()
                            );
                            continue;
                        }
                    }
                }

                message_count += 1;

                match crate::nodes::chat(ctx, target, &input, false, &history, verbosity).await {
                    Ok(Some(answer)) => {
                        println!();
                        skin.print_text(&answer.text);
                        // Show sources here too: an ungrounded claim should be
                        // as visible in a session as in a one-shot command.
                        crate::nodes::print_citations(&answer.citations);
                        crate::nodes::print_answer_footer(target, &answer);
                        println!();
                        // Record the exchange only once it succeeded, so a
                        // failed turn does not poison the context of the next.
                        crate::nodes::record_turn(
                            &mut history,
                            &input,
                            &answer.text,
                            crate::nodes::HISTORY_CHAR_BUDGET,
                        );
                    }
                    Ok(None) => {
                        println!("{}", "Warning: Node returned an empty response.".yellow());
                    }
                    Err(e) => {
                        println!("{} {}", "Error:".red(), e);
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("\nSession Interrupted.");
                break;
            }
            Err(ReadlineError::Eof) => {
                println!("\nSession ended.");
                break;
            }
            Err(err) => {
                println!("Readline error: {:?}", err);
                break;
            }
        }
    }

    println!(
        "\n{} Session closed ({} message{} sent).\n",
        "✓".green(),
        message_count,
        if message_count == 1 { "" } else { "s" }
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brand::Level;

    /// Drop SGR sequences, so a painted string can be compared against the
    /// plain one it has to line up with.
    ///
    /// Deliberately a separate implementation from rustyline's: a shared one
    /// would agree with itself by construction, and what these tests need to
    /// know is whether our escapes are the shape rustyline skips.
    fn visible(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            // CSI ... final byte in @-~. Every sequence brand emits is SGR.
            if chars.next() != Some('[') {
                continue;
            }
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        out
    }

    fn prompt_of(helper: &ReplHelper, plain: &str) -> String {
        helper.highlight_prompt(plain, true).into_owned()
    }

    #[test]
    fn visible_strips_what_brand_emits() {
        assert_eq!(visible(&brand::wordmark_at(Level::TrueColor)), "knaix");
        assert_eq!(visible(&brand::wordmark_at(Level::Ansi256)), "knaix");
        assert_eq!(visible(&brand::wordmark_at(Level::Ansi16)), "knaix");
    }

    /// The invariant the whole approach rests on. If the painted prompt is a
    /// different display width from the plain one rustyline measured, the
    /// cursor lands in the wrong column.
    #[test]
    fn painted_prompt_matches_the_plain_one_it_replaces() {
        let plain = plain_prompt("acme-node");
        for lvl in [Level::Ansi16, Level::Ansi256, Level::TrueColor] {
            let helper = ReplHelper::at(lvl, "acme-node");
            let painted = prompt_of(&helper, &plain);
            assert_eq!(visible(&painted), plain, "level {lvl:?}");
        }
    }

    #[test]
    fn colour_off_leaves_the_prompt_exactly_as_it_was() {
        let plain = plain_prompt("acme-node");
        let helper = ReplHelper::at(Level::None, "acme-node");
        let painted = prompt_of(&helper, &plain);
        assert_eq!(painted, plain);
        assert!(!painted.contains('\x1b'), "no escapes when colour is off");
    }

    /// `default` is false for rustyline's own prompts, such as reverse-search.
    #[test]
    fn only_our_own_prompt_is_painted() {
        let helper = ReplHelper::at(Level::TrueColor, "acme-node");
        let theirs = helper.highlight_prompt("(reverse-i-search)`': ", false);
        assert_eq!(theirs, "(reverse-i-search)`': ");
        assert!(!theirs.contains('\x1b'));
    }

    #[test]
    fn the_wordmark_is_painted_in_the_typed_line() {
        let helper = ReplHelper::at(Level::TrueColor, "acme-node");
        let line = "how do I run knaix locally?";
        let painted = helper.highlight(line, 0);
        assert!(painted.contains('\x1b'));
        // Same visible text, so the display width rustyline requires is kept.
        assert_eq!(visible(&painted), line);
    }

    #[test]
    fn a_line_without_the_wordmark_is_not_copied() {
        let helper = ReplHelper::at(Level::TrueColor, "acme-node");
        assert!(matches!(helper.highlight("what changed?", 0), Borrowed(_)));
    }

    #[test]
    fn the_typed_line_is_untouched_when_colour_is_off() {
        let helper = ReplHelper::at(Level::None, "acme-node");
        let painted = helper.highlight("tell me about knaix", 0);
        assert_eq!(painted, "tell me about knaix");
        assert!(matches!(painted, Borrowed(_)));
    }

    /// `highlight` is dead code unless `highlight_char` asks for the repaint:
    /// the trait's default is false.
    #[test]
    fn highlight_char_asks_for_the_repaint_only_when_there_is_one_to_do() {
        let helper = ReplHelper::at(Level::TrueColor, "acme-node");
        assert!(helper.highlight_char("run knaix", 0, CmdKind::Other));
        assert!(!helper.highlight_char("run it", 0, CmdKind::Other));
        // The final redraw goes out unpainted, matching how rustyline's own
        // highlighters drop decoration on a forced refresh.
        assert!(!helper.highlight_char("run knaix", 0, CmdKind::ForcedRefresh));

        let off = ReplHelper::at(Level::None, "acme-node");
        assert!(!off.highlight_char("run knaix", 0, CmdKind::Other));
    }
}
