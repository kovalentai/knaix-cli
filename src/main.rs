mod config;
mod local;
mod login;
mod model_server;
mod nodes;
mod repl;
mod selftest;
mod update;
mod upload_filter;

use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::*;
use nodes::KnaixContext;

#[derive(Parser)]
#[clap(name = "knaix")]
#[clap(about = "The Kovalent command line: ingest documents into a private AI node and ask questions of them, locally or hosted", long_about = None)]
struct Cli {
    /// Output format (text or json)
    #[clap(short = 'o', long = "output", default_value = "text", global = true)]
    pub output: String,

    /// Print version information
    #[clap(short = 'v', short_alias = 'V', long)]
    pub version: bool,

    #[clap(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Log in to your Kovalent account (opens the browser)
    Login,

    /// Log out, removing the saved session from this machine
    Logout,

    /// List your hosted nodes, or the documents on one node
    #[clap(alias = "ls")]
    List {
        /// A node to list the documents of, instead of listing nodes
        #[clap(name = "NODE_ID")]
        node_id: Option<String>,
    },

    /// Set the default node for later commands ('local' for the local node)
    Use {
        /// The ID of the node to set as default
        node_id: String,
    },

    /// Ask a node one question and print the grounded answer
    Chat {
        /// The node to ask (falls back to the default)
        #[clap(short = 'n', long = "node-id")]
        node_id: Option<String>,

        /// The question to ask
        message: String,
    },

    /// Ingest a file or directory into a node's knowledge base
    Upload {
        /// The node to ingest into (falls back to the default)
        #[clap(short = 'n', long = "node-id")]
        node_id: Option<String>,

        /// Path to the file or directory to ingest
        file_path: String,

        /// Only ingest files matching these glob patterns (repeatable)
        #[clap(long)]
        include: Vec<String>,

        /// Skip files matching these glob patterns (repeatable)
        #[clap(long)]
        exclude: Vec<String>,

        /// Ingest everything, including build directories and unsupported types
        #[clap(long)]
        all: bool,

        /// List what would be ingested without sending anything
        #[clap(long)]
        dry_run: bool,
    },

    /// Show who is logged in, the default node, and the local node's state
    Status,

    /// Show or change the API URL the CLI talks to
    Config {
        /// Set the API URL instead of showing it
        #[clap(long)]
        api_url: Option<String>,
    },

    /// Show a node's health and latency
    Metrics {
        /// The node to check (falls back to the default)
        node_id: Option<String>,

        /// The node to check, as a flag for symmetry with chat and upload
        #[clap(short = 'n', long = "node-id", conflicts_with = "node_id")]
        node: Option<String>,
    },

    /// Show a node's logs
    Logs {
        /// The node to read (falls back to the default)
        node_id: Option<String>,

        /// The node to read, as a flag for symmetry with chat and upload
        #[clap(short = 'n', long = "node-id", conflicts_with = "node_id")]
        node: Option<String>,

        /// Number of lines to retrieve (default: 50)
        #[clap(short, long, default_value = "50")]
        lines: usize,
    },

    /// Start an interactive chat session with a node
    Repl {
        /// The node to chat with (falls back to the default)
        node_id: Option<String>,

        /// The node to chat with, as a flag for symmetry with chat and upload
        #[clap(short = 'n', long = "node-id", conflicts_with = "node_id")]
        node: Option<String>,
    },

    /// Provision a hosted node on your Kovalent account
    Up,

    /// Run the whole stack on this machine, with no account and no network
    Local {
        #[clap(subcommand)]
        action: LocalAction,
    },

    /// Check that a node retrieves and cites correctly, using a bundled corpus
    Selftest {
        /// The node to test (falls back to the default)
        node_id: Option<String>,

        /// The node to test, as a flag for symmetry with chat and upload
        #[clap(short = 'n', long = "node-id", conflicts_with = "node_id")]
        node: Option<String>,

        /// Leave the generated documents on the node instead of removing them
        #[clap(long)]
        keep: bool,

        /// Ask a short, balanced subset instead of the full question set
        #[clap(long)]
        quick: bool,

        /// Remove self-test documents left behind by an earlier interrupted run
        #[clap(long)]
        sweep: bool,
    },

    /// Print a shell completion script (bash, zsh, fish, powershell, elvish)
    Completions {
        /// Shell to generate for
        #[clap(value_enum)]
        shell: clap_complete::Shell,
    },

    /// List or read the notes saved with /remember in the REPL
    Memory {
        /// The node whose notes to show (falls back to the default)
        node_id: Option<String>,

        /// The node whose notes to show, as a flag
        #[clap(short = 'n', long = "node-id", conflicts_with = "node_id")]
        node: Option<String>,

        /// Read a specific notes file instead of listing them
        #[clap(short, long)]
        file: Option<String>,
    },
}

#[derive(Subcommand)]
enum LocalAction {
    /// Start the local node (pulls the image the first time)
    Up {
        /// Host port to publish the node on
        #[clap(long)]
        port: Option<u16>,

        /// Answer with a model served at this URL instead of the mock; any
        /// OpenAI-compatible server (Ollama, LM Studio, vLLM, llama-server)
        #[clap(long, alias = "llama-url", value_name = "URL")]
        model_url: Option<String>,

        /// Model to ask that server for, e.g. an Ollama model name
        #[clap(short = 'm', long, alias = "llama-model", value_name = "NAME")]
        model: Option<String>,

        /// Ignore any remembered model server and use the deterministic mock
        #[clap(long, conflicts_with = "model_url")]
        mock: bool,

        /// Re-pull the image even if it is already present
        #[clap(long)]
        pull: bool,
    },

    /// Pick the model that answers, from the servers on this machine
    Setup,

    /// Stop the local node
    Down {
        /// Also delete its store, discarding everything ingested
        #[clap(long)]
        purge: bool,
    },

    /// Show whether the local node is running and healthy
    Status,

    /// Show the local node's container logs
    Logs {
        /// Number of lines to show
        #[clap(short, long, default_value = "50")]
        lines: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let update_task = tokio::spawn(async {
        update::check_for_update_async().await;
    });

    let cli = Cli::parse();

    if cli.version {
        println!("knaix {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let _ = cmd.print_help();
            std::process::exit(2);
        }
    };

    let ctx = KnaixContext::new(cli.output.clone());

    match command {
        Commands::Login => {
            login::login().await?;
        }
        Commands::Logout => {
            let mut stored = config::load_stored_config();
            if stored.token.is_none() {
                println!("{} No session is stored on this machine.", "Info:".blue());
            } else {
                stored.token = None;
                stored.username = None;
                config::save_config(&stored)?;
                println!(
                    "{} Logged out. The saved session token has been removed.",
                    "✓".green()
                );
            }
            if std::env::var("KNAIX_TOKEN").is_ok() {
                println!(
                    "{} KNAIX_TOKEN is set in this shell and still authenticates requests.",
                    "Note:".blue()
                );
            }
        }
        Commands::List { node_id } => {
            nodes::list_nodes(&ctx, node_id.as_deref()).await?;
        }
        Commands::Use { node_id } => {
            let mut config = config::load_stored_config();
            config.default_node_id = Some(node_id.clone());
            config::save_config(&config)?;
            println!("{} Set default node to {}", "Info:".blue(), node_id.bold());
        }
        Commands::Chat { node_id, message } => {
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                if ctx.output_format == "json" {
                    if let Some(answer) = nodes::chat(&ctx, &target, &message, false, &[]).await? {
                        nodes::print_answer_json(&answer)?;
                    }
                } else if let Some(answer) = nodes::chat(&ctx, &target, &message, true, &[]).await?
                {
                    nodes::print_answer_footer(&target, &answer);
                }
            }
        }
        Commands::Upload {
            node_id,
            file_path,
            include,
            exclude,
            all,
            dry_run,
        } => {
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                let opts = nodes::UploadOptions {
                    include,
                    exclude,
                    all,
                    dry_run,
                };
                nodes::upload(&ctx, &target, &file_path, &opts).await?;
            }
        }
        Commands::Status => {
            let config = config::load_config();
            let local = local::summarize();

            if ctx.output_format == "json" {
                let status_json = serde_json::json!({
                    "username": config.username,
                    "defaultNodeId": config.default_node_id,
                    "apiUrl": config.api_url,
                    "authenticated": config.token.is_some(),
                    "localNode": { "state": local.state, "url": local.url }
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status_json).unwrap_or_default()
                );
                return Ok(());
            }

            println!("\n{}", "Knaix CLI Configuration:".bold().underline());
            let mut table = comfy_table::Table::new();
            table.load_preset(comfy_table::presets::UTF8_FULL);
            table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
            table.set_header(vec!["Setting", "Value"]);

            table.add_row(vec![
                "Username".dimmed().to_string(),
                config
                    .username
                    .as_deref()
                    .unwrap_or("Not logged in")
                    .cyan()
                    .to_string(),
            ]);
            table.add_row(vec![
                "Default Node".dimmed().to_string(),
                config
                    .default_node_id
                    .as_deref()
                    .unwrap_or("None set (use 'knaix use <node-id>')")
                    .blue()
                    .to_string(),
            ]);
            table.add_row(vec![
                "API URL".dimmed().to_string(),
                config.api_url.dimmed().to_string(),
            ]);
            table.add_row(vec![
                "Local Node".dimmed().to_string(),
                match (local.state.as_str(), &local.url) {
                    ("running", Some(url)) => format!("running on {}", url).green().to_string(),
                    ("running", None) => "running".green().to_string(),
                    ("none", _) => "none (start one with 'knaix local up')"
                        .dimmed()
                        .to_string(),
                    (state, _) => state.yellow().to_string(),
                },
            ]);

            // A stored token is all this command can vouch for; whether the
            // control plane still honours it is only knowable by asking it.
            if config.token.is_some() {
                table.add_row(vec![
                    "Auth".dimmed().to_string(),
                    "Session token saved ('knaix list' verifies it)"
                        .green()
                        .to_string(),
                ]);
            } else {
                table.add_row(vec![
                    "Auth".dimmed().to_string(),
                    "Not logged in (run 'knaix login')".red().to_string(),
                ]);
            }
            println!("{table}\n");
        }
        Commands::Config { api_url } => {
            if let Some(url) = api_url {
                let mut stored = config::load_stored_config();
                stored.api_url = url.clone();
                config::save_config(&stored)?;
                println!("{} Updated API URL to {}", "Info:".blue(), url.bold());
            } else {
                // Report the URL requests actually go to, overrides included.
                let config = config::load_config();
                println!("\n{}", "Current Configuration:".bold().underline());
                println!("  API URL: {}", config.api_url.cyan());
            }
        }
        Commands::Metrics { node_id, node } => {
            let node_id = node.or(node_id);
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                nodes::get_metrics_for(&ctx, &target).await?;
            }
        }
        Commands::Logs {
            node_id,
            node,
            lines,
        } => {
            let node_id = node.or(node_id);
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                nodes::get_logs_for(&ctx, &target, lines).await?;
            }
        }
        Commands::Repl { node_id, node } => {
            let node_id = node.or(node_id);
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                repl::run(&ctx, &target).await?;
            }
        }
        Commands::Local { action } => match action {
            LocalAction::Up {
                port,
                model_url,
                model,
                mock,
                pull,
            } => local::up(port, model_url, model, mock, pull).await?,
            LocalAction::Setup => local::setup().await?,
            LocalAction::Down { purge } => local::down(purge)?,
            LocalAction::Status => local::status(ctx.output_format == "json").await?,
            LocalAction::Logs { lines } => local::logs(lines)?,
        },
        Commands::Up => {
            nodes::up(&ctx).await?;
        }
        Commands::Selftest {
            node_id,
            node,
            keep,
            quick,
            sweep,
        } => {
            let node_id = node.or(node_id);
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                selftest::run(&ctx, &target, keep, quick, sweep).await?;
            }
        }
        Commands::Completions { shell } => {
            // Written to stdout so it can be sourced or redirected directly;
            // anything else on stdout here would be sourced as shell code.
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            return Ok(());
        }
        Commands::Memory {
            node_id,
            node,
            file,
        } => {
            let node_id = node.or(node_id);
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                let key = nodes::memory_key(&target);
                nodes::view_memory(&ctx, &key, file.as_deref()).await?;
            }
        }
    }

    let _ = tokio::time::timeout(std::time::Duration::from_millis(50), update_task).await;
    update::print_update_banner();

    Ok(())
}
