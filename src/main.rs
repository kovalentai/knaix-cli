mod brand;
mod config;
mod exit;
mod local;
mod login;
mod mcp;
mod model_server;
mod nodes;
mod project;
mod repl;
mod selftest;
mod shell;
mod stdin_arg;
mod update;
mod upload_filter;

use anyhow::{Context, Result};
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

    /// Suppress progress and commentary; results and errors still print
    #[clap(short = 'q', long, global = true)]
    pub quiet: bool,

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

    /// Write a .knaix.toml so this repo remembers its node and what to ingest
    Init {
        /// The node commands in this repo should address
        #[clap(short = 'n', long = "node-id")]
        node_id: Option<String>,

        /// Only ingest files matching these glob patterns (repeatable)
        #[clap(long)]
        include: Vec<String>,

        /// Skip files matching these glob patterns (repeatable)
        #[clap(long)]
        exclude: Vec<String>,

        /// Overwrite an existing .knaix.toml
        #[clap(long)]
        force: bool,
    },

    /// Ask a node one question and print the grounded answer
    Chat {
        /// The node to ask (falls back to the default)
        #[clap(short = 'n', long = "node-id")]
        node_id: Option<String>,

        /// The question to ask, or '-' to read it from standard input
        message: String,

        /// Answer in one or two sentences (local node only)
        #[clap(long, conflicts_with = "detailed")]
        brief: bool,

        /// Answer thoroughly, with all relevant detail (local node only)
        #[clap(long)]
        detailed: bool,
    },

    /// Ingest a file or directory into a node's knowledge base
    Upload {
        /// The node to ingest into (falls back to the default)
        #[clap(short = 'n', long = "node-id")]
        node_id: Option<String>,

        /// Path to the file or directory to ingest, or '-' for standard input
        file_path: String,

        /// Name to file piped input under (only with '-')
        #[clap(long, default_value = "stdin.md")]
        name: String,

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

    /// Print the shell integration that paints the knaix wordmark as you type
    ShellInit {
        /// Shell to generate for
        #[clap(value_enum)]
        shell: shell::InitShell,

        /// Add it to your shell profile, after showing you what will change
        #[clap(long, conflicts_with = "uninstall")]
        install: bool,

        /// Remove a previously installed integration
        #[clap(long)]
        uninstall: bool,
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

    /// Print the MCP client config that points an editor at a node
    Mcp {
        /// The node to configure for (falls back to the default)
        node_id: Option<String>,

        /// The node to configure for, as a flag for symmetry with chat and upload
        #[clap(short = 'n', long = "node-id", conflicts_with = "node_id")]
        node: Option<String>,
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

    /// Pick the model that answers, and start the node if it is not running
    Setup,

    /// Empty the store and start fresh, keeping your model choice
    Reset {
        /// Skip the confirmation prompt
        #[clap(long)]
        yes: bool,
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

    /// Connect a running local node to your account and stream its metrics and logs
    Connect {
        /// Keep relaying in the background after the command returns
        #[clap(long)]
        daemon: bool,

        /// Internal: run the detached relay loop (used by --daemon)
        #[clap(long, hide = true)]
        worker: bool,
    },

    /// Stop relaying and mark the local node offline in your account
    Disconnect,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::from(exit::Code::Ok.as_u8()),
        Err(e) => {
            // anyhow used to render this via Termination's Debug formatting.
            // Taking the exit code back means rendering it here, so the causes
            // are printed in the house style rather than anyhow's numbered
            // "Caused by:" block.
            eprintln!("{} {}", "Error:".red(), e);
            for cause in e.chain().skip(1) {
                eprintln!("  {} {}", "caused by:".dimmed(), cause);
            }
            std::process::ExitCode::from(exit::code_of(&e).as_u8())
        }
    }
}

/// Which node a command addresses: an explicit flag, then the project file,
/// then whatever `resolve_target` falls back to.
///
/// The flag wins because it was typed for this one command. The project file
/// beats the machine's saved default because it is the more specific statement:
/// the repo says which node its documents belong to.
fn project_node(flag: Option<String>, project: Option<&project::Project>) -> Option<String> {
    flag.or_else(|| project.and_then(|p| p.node.clone()))
}

/// Globs replace rather than merge. Adding to a project's list would leave no
/// way to narrow it for one command, and narrowing is the reason to pass them.
fn project_globs(flags: Vec<String>, from_project: Option<&Vec<String>>) -> Vec<String> {
    if !flags.is_empty() {
        return flags;
    }
    from_project.cloned().unwrap_or_default()
}

async fn run() -> Result<()> {
    let update_task = tokio::spawn(async {
        update::check_for_update_async().await;
    });

    let cli = Cli::parse();

    if cli.version {
        println!("{} {}", brand::wordmark(), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let _ = cmd.print_help();
            // Matches what clap exits with for a bad argument.
            std::process::exit(exit::Code::Usage.as_u8() as i32);
        }
    };

    let ctx = KnaixContext::with_quiet(cli.output.clone(), cli.quiet);

    // Read once. A file that cannot be parsed stops the command rather than
    // being skipped, or the command runs under settings the file does not ask
    // for while the file looks correct.
    //
    // Except for `init`, which is how a broken file gets replaced. Refusing to
    // run it leaves the one command that would fix the problem unreachable, and
    // the only way out is to delete the file by hand.
    let project = if matches!(command, Commands::Init { .. }) {
        None
    } else {
        project::current()?
    };

    match command {
        Commands::Login => {
            login::login().await?;
            // If a local node is running, connect it so it shows up right away.
            local::connect_snapshot().await;
        }
        Commands::Logout => {
            // Best-effort disconnect while the token is still on disk.
            let _ = local::disconnect().await;
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
        Commands::Chat {
            node_id,
            message,
            brief,
            detailed,
        } => {
            let verbosity = if brief {
                nodes::Verbosity::Brief
            } else if detailed {
                nodes::Verbosity::Detailed
            } else {
                nodes::Verbosity::Normal
            };
            let message = if stdin_arg::is_stdin(&message) {
                stdin_arg::read_text("the question")?
            } else {
                message
            };
            let node_id = project_node(node_id, project.as_ref());
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                // Verbosity shapes the local node's system prompt; a hosted node
                // is prompted by the control plane, so say the flag had no effect
                // rather than dropping it silently.
                if verbosity != nodes::Verbosity::Normal && !target.is_local() {
                    println!(
                        "{} {} apply to the local node; a hosted node is prompted by the control plane.",
                        "Note:".blue(),
                        "--brief/--detailed".cyan()
                    );
                }
                if ctx.output_format == "json" {
                    if let Some(answer) =
                        nodes::chat(&ctx, &target, &message, false, &[], verbosity).await?
                    {
                        nodes::print_answer_json(&answer)?;
                    }
                } else if let Some(answer) =
                    nodes::chat(&ctx, &target, &message, true, &[], verbosity).await?
                {
                    nodes::print_answer_footer(&target, &answer);
                }
            }
        }
        Commands::Upload {
            node_id,
            file_path,
            name,
            include,
            exclude,
            all,
            dry_run,
        } => {
            let node_id = project_node(node_id, project.as_ref());
            let opts = nodes::UploadOptions {
                include: project_globs(include, project.as_ref().map(|p| &p.upload.include)),
                exclude: project_globs(exclude, project.as_ref().map(|p| &p.upload.exclude)),
                all,
            };

            if stdin_arg::is_stdin(&file_path) {
                // The name is checked before anything is read, so a bad one
                // fails on the argument rather than after consuming the pipe.
                let checked = stdin_arg::checked_name(&name)?.to_string();
                if dry_run {
                    println!("  {} {}", "would ingest".dimmed(), checked);
                } else {
                    // Staged to a real file so piped content goes through the
                    // same upload path as a named one, rather than a second
                    // code path that would drift from it.
                    let bytes = stdin_arg::read_bytes("the document")?;
                    let staged = stdin_arg::TempFile::write(&checked, &bytes)?;
                    if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                        nodes::upload_single_file(&ctx, &target, staged.path(), &checked).await?;
                    }
                }
            } else {
                // Planning is entirely local: which files qualify is decided by
                // the directory and the filters. Doing it first means --dry-run
                // needs no node, and a bad path is reported as a bad path
                // rather than as whatever the node happened to say.
                let plan = nodes::plan_upload(&file_path, &opts)?;
                if dry_run {
                    nodes::report_plan(&plan, &file_path);
                } else if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                    nodes::upload(&ctx, &target, &file_path, plan).await?;
                }
            }
        }
        Commands::Init {
            node_id,
            include,
            exclude,
            force,
        } => {
            // Written where the command was run, not at a discovered root: the
            // point is to mark this directory as the project.
            let path = std::env::current_dir()
                .context("Could not read the current directory")?
                .join(project::FILE_NAME);

            // Fall back to the saved default so the common case needs no flag,
            // and the file records the node rather than leaving it implicit.
            let node = node_id.or_else(|| ctx.config.default_node_id.clone());
            let settings = project::Project {
                node,
                upload: project::Upload { include, exclude },
            };
            project::write(&path, &settings, force)?;

            // The file is what init produces. Saying so is for the person
            // watching, so it goes when they ask for quiet.
            ctx.info(&format!(
                "{} Wrote {}",
                "✓".green(),
                path.display().to_string().bold()
            ));
            match &settings.node {
                Some(node) => ctx.info(&format!("  Commands run here address {}.", node.cyan())),
                None => ctx.info(&format!(
                    "  No node recorded. Set one with {}, or edit the file.",
                    brand::cmd("init --node-id <NODE>").as_str()
                )),
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
            let node_id = project_node(node.or(node_id), project.as_ref());
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                nodes::get_metrics_for(&ctx, &target).await?;
            }
        }
        Commands::Logs {
            node_id,
            node,
            lines,
        } => {
            let node_id = project_node(node.or(node_id), project.as_ref());
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                nodes::get_logs_for(&ctx, &target, lines).await?;
            }
        }
        Commands::Repl { node_id, node } => {
            let node_id = project_node(node.or(node_id), project.as_ref());
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
            LocalAction::Reset { yes } => local::reset(yes).await?,
            LocalAction::Down { purge } => local::down(purge)?,
            LocalAction::Status => local::status(ctx.output_format == "json").await?,
            LocalAction::Logs { lines } => local::logs(lines)?,
            LocalAction::Connect { daemon, worker } => local::connect(daemon, worker).await?,
            LocalAction::Disconnect => local::disconnect().await?,
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
            let node_id = project_node(node.or(node_id), project.as_ref());
            if let Some(target) = nodes::resolve_target(&ctx, node_id.clone()).await? {
                selftest::run(&ctx, &target, keep, quick, sweep).await?;
            }
        }
        Commands::ShellInit {
            shell: init_shell,
            install,
            uninstall,
        } => {
            if uninstall {
                shell::uninstall(init_shell)?;
            } else if install {
                shell::install(init_shell)?;
            } else {
                // Straight to stdout so `eval "$(knaix shell-init zsh)"` works;
                // anything else here would be evaluated as shell code.
                print!("{}", shell::init_script(init_shell)?);
            }
            return Ok(());
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
        Commands::Mcp { node_id, node } => {
            mcp::run(&ctx, node_id.or(node)).await?;
        }

        Commands::Memory {
            node_id,
            node,
            file,
        } => {
            let node_id = project_node(node.or(node_id), project.as_ref());
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
