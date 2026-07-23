use crate::nodes::KnaixContext;
use anyhow::{Context, Result};
use colored::*;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use termimad::MadSkin;

fn print_help() {
    println!("\n{}", "REPL commands:".bold().underline());
    let rows: &[(&str, &str)] = &[
        ("/help", "Show this help"),
        (
            "/remember <fact>",
            "Save a fact to this node's notes and ingest it",
        ),
        ("/memory", "List the notes saved for this node"),
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
    let mut rl = DefaultEditor::new().context("Failed to initialize readline")?;
    let skin = MadSkin::default_dark();

    println!(
        "\n{} Chatting with {}. {} lists commands; {} ends the session.",
        "●".green(),
        node_id.cyan().bold(),
        "/help".cyan(),
        "/exit".cyan()
    );

    let mut message_count = 0;

    loop {
        let prompt = format!("knaix [{}]> ", node_id);
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

                match crate::nodes::chat(ctx, target, &input, false).await {
                    Ok(Some(answer)) => {
                        println!();
                        skin.print_text(&answer.text);
                        // Show sources here too: an ungrounded claim should be
                        // as visible in a session as in a one-shot command.
                        crate::nodes::print_citations(&answer.citations);
                        crate::nodes::print_answer_footer(target, &answer);
                        println!();
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
