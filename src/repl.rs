use crate::nodes::KnaixContext;
use anyhow::{Context, Result};
use colored::*;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use termimad::MadSkin;

pub async fn run(ctx: &KnaixContext, node_id: &str) -> Result<()> {
    let mut rl = DefaultEditor::new().context("Failed to initialize readline")?;
    let skin = MadSkin::default_dark();

    println!(
        "\n{} Knaix AI Session: {} (Type '/exit' to end or enter your message)",
        "●".green(),
        node_id.cyan().bold()
    );

    let mut message_count = 0;
    let mut history_buffer: Vec<String> = Vec::new();

    loop {
        let prompt = format!("knaix [{}]> ", node_id);
        let readline = rl.readline(&prompt);

        match readline {
            Ok(line) => {
                let input: String = line.trim().to_string();
                if input.is_empty() {
                    continue;
                }

                let mut final_input = input.clone();

                let mut is_memory_intent = false;
                let mut fact_to_remember = "";

                if let Some(fact) = input.strip_prefix("/remember ") {
                    is_memory_intent = true;
                    fact_to_remember = fact;
                } else if input.to_lowercase().starts_with("remember ") {
                    is_memory_intent = true;
                    let lower = input.to_lowercase();
                    if lower.starts_with("remember that ") {
                        fact_to_remember = &input[14..];
                    } else if lower.starts_with("remember ") {
                        fact_to_remember = &input[9..];
                    }
                }

                if is_memory_intent {
                    println!(
                        "{} Recognizing intent to memorize: {}",
                        "Intent recognized:".magenta(),
                        fact_to_remember.cyan()
                    );

                    if let Err(e) =
                        crate::nodes::memorize(ctx, node_id, fact_to_remember, true).await
                    {
                        println!("{} Failed to save memory: {}", "Error:".red(), e);
                    } else {
                        println!(
                            "{} Explicit memory securely stored and available across sessions.",
                            "✓".green()
                        );
                    }

                    final_input = format!("I have securely stored the following fact in your durable memory: {}. Please confirm.", fact_to_remember);
                } else if input.starts_with('/') {
                    let parts: Vec<&str> = input.splitn(2, ' ').collect();
                    let cmd = parts[0];
                    let args = if parts.len() > 1 { parts[1] } else { "" };

                    match cmd {
                        "/exit" | "/quit" => break,
                        "/help" => {
                            println!("\n{}", "Available REPL Commands:".bold().underline());
                            println!("  {:<14} End the current session", "/exit, /quit".cyan());
                            println!("  {:<14} Show this help message", "/help".cyan());
                            println!(
                                "  {:<14} Ask the AI to explain a concept in detail",
                                "/explain <...>\t".cyan()
                            );
                            println!(
                                "  {:<14} Ask the AI to summarize a topic or findings",
                                "/summarize <...>\t".cyan()
                            );
                            println!(
                                "  {:<14} Explicitly save a fact to your durable memory",
                                "/remember <...>\t".cyan()
                            );
                            println!(
                                "  {:<14} Ask the AI what it remembers about you\n",
                                "/memory\t\t".cyan()
                            );
                            continue;
                        }
                        "/explain" => {
                            if args.is_empty() {
                                println!("{} Usage: /explain <topic>", "Error:".red());
                                continue;
                            }
                            final_input = format!("Please explain this in detail: {}", args);
                        }
                        "/summarize" => {
                            if args.is_empty() {
                                println!("{} Usage: /summarize <text or topic>", "Error:".red());
                                continue;
                            }
                            final_input = format!(
                                "Please provide a concise summary of the key findings for: {}",
                                args
                            );
                        }
                        "/memory" => {
                            final_input = "Please review your durable memory and summarize what you know and remember about me and my environment.".to_string();
                        }
                        _ => {
                            final_input = input.clone();
                        }
                    }
                }

                rl.add_history_entry(input.as_str()).ok();
                message_count += 1;
                history_buffer.push(input.clone());

                if history_buffer.len() >= 5 {
                    println!("{} Session growing. Background worker compressing context into ephemeral log...", "Memory:".magenta());
                    let history_text = history_buffer.join("\n");
                    let summary_prompt = format!("Concisely summarize the technical facts or context from these messages:\n{}", history_text);

                    let ctx_clone = ctx.clone();
                    let node_id_clone = node_id.to_string();

                    tokio::spawn(async move {
                        if let Ok(Some(summary)) = crate::nodes::chat_silent(
                            ctx_clone.config.clone(),
                            node_id_clone.clone(),
                            summary_prompt,
                        )
                        .await
                        {
                            let _ =
                                crate::nodes::memorize(&ctx_clone, &node_id_clone, &summary, false)
                                    .await;
                        }
                    });
                    history_buffer.clear();
                }

                match crate::nodes::chat(ctx, node_id, &final_input, false).await {
                    Ok(Some(answer)) => {
                        println!();
                        skin.print_text(&answer.text);
                        // Show sources here too: an ungrounded claim should be
                        // as visible in a session as in a one-shot command.
                        crate::nodes::print_citations(&answer.citations);
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
        "\n{} Session closed ({} messages sent). Stay sovereign.\n",
        "✓".green(),
        message_count
    );

    Ok(())
}
