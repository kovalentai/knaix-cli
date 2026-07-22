mod config;
mod local;
mod login;
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
#[clap(about = "Sovereign AI Terminal - Connect to your Kovalent Private Stack", long_about = None)]
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
    /// Authenticate with Kovalent Identity Center (SSO)
    Login,

    /// List your provisioned AI nodes and their status
    #[clap(alias = "ls")]
    List {
        /// Optional: List documents in a specific node's knowledge base
        #[clap(name = "NODE_ID")]
        node_id: Option<String>,
    },

    /// Set a default node context for one-shot commands
    Use {
        /// The ID of the node to set as default
        node_id: String,
    },

    /// Chat with an AI node (one-shot message)
    Chat {
        /// The ID of the node to chat with (falls back to default)
        #[clap(short = 'n', long = "node-id")]
        node_id: Option<String>,

        /// Your message to the AI
        message: String,
    },

    /// Upload a file to a node's knowledge base
    Upload {
        /// The ID of the node (falls back to default)
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

    /// Show CLI configuration and connection status
    Status,

    /// Manage CLI configuration settings (API URL, token)
    Config {
        /// Optional: Override the API URL
        #[clap(long)]
        api_url: Option<String>,
    },

    /// Show real-time performance metrics (latency, health) for a node
    Metrics {
        /// The ID of the node (falls back to default)
        node_id: Option<String>,
    },

    /// View system logs for a node (container logs)
    Logs {
        /// The ID of the node (falls back to default)
        node_id: Option<String>,

        /// Number of lines to retrieve (default: 50)
        #[clap(short, long, default_value = "50")]
        lines: usize,
    },

    /// Launch a continuous, immersive REPL session
    Repl {
        /// The ID of the node to chat with (falls back to default)
        node_id: Option<String>,
    },

    /// Provision a new Sovereign Node instantly
    Up,

    /// Run the whole stack on this machine, with no account and no network
    Local {
        #[clap(subcommand)]
        action: LocalAction,
    },

    /// Check that a node retrieves and cites correctly, using a bundled corpus
    Selftest {
        /// The ID of the node to test (falls back to default)
        node_id: Option<String>,

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

    /// View your Sovereign Agentic Memory for a node
    Memory {
        /// The ID of the node (falls back to default)
        node_id: Option<String>,

        /// Optional: read a specific memory file instead of listing
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

        /// Use a local model served at this URL instead of the mock
        #[clap(long)]
        llama_url: Option<String>,

        /// Ignore any remembered model server and use the deterministic mock
        #[clap(long, conflicts_with = "llama_url")]
        mock: bool,

        /// Re-pull the image even if it is already present
        #[clap(long)]
        pull: bool,
    },

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
            login::login().await;
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
                nodes::chat(&ctx, &target, &message, true).await?;
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

            if ctx.output_format == "json" {
                let status_json = serde_json::json!({
                    "username": config.username,
                    "defaultNodeId": config.default_node_id,
                    "apiUrl": config.api_url,
                    "authenticated": config.token.is_some()
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

            if config.token.is_some() {
                table.add_row(vec![
                    "Auth".dimmed().to_string(),
                    "Connected (Active Session)".green().to_string(),
                ]);
            } else {
                table.add_row(vec![
                    "Auth".dimmed().to_string(),
                    "Missing (Run 'knaix login')".red().to_string(),
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
        Commands::Metrics { node_id } => {
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                nodes::get_metrics_for(&ctx, &target).await?;
            }
        }
        Commands::Logs { node_id, lines } => {
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                nodes::get_logs_for(&ctx, &target, lines).await?;
            }
        }
        Commands::Repl { node_id } => {
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                repl::run(&ctx, &target).await?;
            }
        }
        Commands::Local { action } => match action {
            LocalAction::Up {
                port,
                llama_url,
                mock,
                pull,
            } => local::up(port, llama_url, mock, pull).await?,
            LocalAction::Down { purge } => local::down(purge)?,
            LocalAction::Status => local::status(ctx.output_format == "json").await?,
            LocalAction::Logs { lines } => local::logs(lines)?,
        },
        Commands::Up => {
            nodes::up(&ctx).await?;
        }
        Commands::Selftest {
            node_id,
            keep,
            quick,
            sweep,
        } => {
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
        Commands::Memory { node_id, file } => {
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
