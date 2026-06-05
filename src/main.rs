use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mcp_librarian::{config, discovery, index, lockfile, playbook, probe, server};
use rmcp::ServiceExt;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(name = "mcp-librarian", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run as an MCP server over stdio. This is the mode Claude Code uses.
    Serve,
    /// Print a server's playbook to stdout. Useful for debugging manifests + notes.
    Print {
        /// Server name. Use "librarian" for the librarian's own playbook.
        server: String,
        /// Optional topic for drill-down.
        #[arg(long)]
        topic: Option<String>,
    },
    /// List all discovered MCP servers (cached + freshly discovered).
    List {
        /// Filter by manifest category.
        #[arg(long)]
        category: Option<String>,
    },
    /// Reprobe every probeable MCP server and rebuild the cache.
    Refresh {
        /// Refresh only this server.
        #[arg(long)]
        server: Option<String>,
    },
    /// Stub for future compaction of learned notes. Currently a no-op.
    Compact { server: String },
}

fn init_tracing() {
    // Stderr only — stdout is reserved for MCP wire traffic.
    // Default is quiet: only warn + above for everything, except mcp_librarian itself.
    // Override with RUST_LOG to see rmcp internals.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,mcp_librarian=info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let paths = config::Paths::discover()?;
    paths.ensure_dirs()?;

    match cli.command {
        Command::Serve => run_serve(paths).await,
        Command::Print { server, topic } => run_print(paths, server, topic),
        Command::List { category } => run_list(paths, category),
        Command::Refresh { server } => run_refresh(paths, server).await,
        Command::Compact { server } => {
            println!("TODO: compaction of learned notes for '{server}' not implemented yet.");
            Ok(())
        }
    }
}

async fn run_serve(paths: config::Paths) -> Result<()> {
    tracing::info!("librarian starting on stdio");
    let srv = server::LibrarianServer::new(paths);
    let running = srv
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
        .context("starting rmcp server")?;
    running.waiting().await.context("waiting for shutdown")?;
    Ok(())
}

fn run_print(paths: config::Paths, server: String, topic: Option<String>) -> Result<()> {
    if server.eq_ignore_ascii_case("librarian") {
        println!("{}", playbook::render_self());
        return Ok(());
    }
    let idx = index::Index::load(&paths.cache_file)?;
    let entry = match idx.servers.get(&server) {
        Some(e) => e.clone(),
        None => match discovery::discover()?
            .into_iter()
            .find(|c| c.name == server)
        {
            Some(cfg) => index::entry_from_unprobed(&cfg),
            None => anyhow::bail!("unknown server '{server}'"),
        },
    };
    let manifest = playbook::load_manifest(&paths, &server)?;
    let notes = index::read_notes(&paths, &server)?;
    println!(
        "{}",
        playbook::render_help(&entry, manifest.as_ref(), &notes, topic.as_deref())
    );
    Ok(())
}

fn run_list(paths: config::Paths, category: Option<String>) -> Result<()> {
    let idx = index::Index::load(&paths.cache_file)?;
    let mut pairs = Vec::new();
    if idx.servers.is_empty() {
        for cfg in discovery::discover()? {
            let manifest = playbook::load_manifest(&paths, &cfg.name).ok().flatten();
            pairs.push((index::entry_from_unprobed(&cfg), manifest));
        }
    } else {
        for entry in idx.servers.values() {
            let manifest = playbook::load_manifest(&paths, &entry.name).ok().flatten();
            pairs.push((entry.clone(), manifest));
        }
    }
    println!("{}", playbook::render_list(&pairs, category.as_deref()));
    Ok(())
}

async fn run_refresh(paths: config::Paths, server: Option<String>) -> Result<()> {
    let configs = discovery::discover()?;

    let to_probe: Vec<_> = match &server {
        Some(name) => configs.into_iter().filter(|c| &c.name == name).collect(),
        None => configs,
    };

    if to_probe.is_empty() {
        if let Some(name) = server {
            anyhow::bail!("no config entry for '{name}'");
        }
        println!("(nothing to refresh — no servers configured)");
        return Ok(());
    }

    // Probing spawns child processes and can take seconds per server. Done
    // OUTSIDE the lock so we don't block concurrent MCP-side writes for the
    // duration of the probe sweep. The merge + save step below takes the
    // same lock the MCP refresh path uses; without it a CLI `refresh` racing
    // an MCP write would clobber state.
    let entries = probe::probe_all(&to_probe).await;
    let mut probed = 0usize;
    let mut failed = 0usize;
    let mut remote = 0usize;
    for entry in &entries {
        match entry.probe_status {
            index::ProbeStatus::Ok => probed += 1,
            index::ProbeStatus::NotProbeable => remote += 1,
            _ => failed += 1,
        }
    }

    lockfile::with_write_lock(&paths, || {
        let mut idx = index::Index::load(&paths.cache_file)?;
        let prior = idx.clone();
        for entry in entries {
            // Drift-flag relevant notes against the pre-write index snapshot.
            if let Some(old) = prior.servers.get(&entry.name) {
                let mut drifted_tools = Vec::new();
                for new_tool in &entry.tools {
                    if let Some(old_tool) = old.tools.iter().find(|t| t.name == new_tool.name)
                        && index::Index::arg_shape_drifted(
                            &old_tool.arg_summary,
                            &new_tool.arg_summary,
                        )
                    {
                        drifted_tools.push(new_tool.name.clone());
                    }
                }
                if !drifted_tools.is_empty() {
                    let mut notes = index::read_notes(&paths, &entry.name)?;
                    let mut changed = 0usize;
                    for note in notes.iter_mut() {
                        if let Some(t) = &note.tool
                            && drifted_tools.contains(t)
                            && !note.possibly_stale
                        {
                            note.possibly_stale = true;
                            changed += 1;
                        }
                    }
                    if changed > 0 {
                        index::write_notes(&paths, &entry.name, &notes)?;
                        tracing::info!(
                            server = %entry.name,
                            flagged = changed,
                            "drift flags applied"
                        );
                    }
                }
            }
            idx.servers.insert(entry.name.clone(), entry);
        }
        idx.save(&paths.cache_file)?;
        Ok(())
    })?;

    println!("refreshed: {probed} probed, {failed} failed, {remote} remote (not probed)");
    Ok(())
}
