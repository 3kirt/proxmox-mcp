mod client;
mod config;
mod tools;

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context;
use clap::Parser;
use rmcp::ServiceExt;
use tracing::info;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[derive(Parser)]
#[command(
    name = "proxmox-mcp",
    about = "Read-only MCP server for Proxmox VE",
    long_about = "Read-only MCP server that exposes Proxmox VE data as tools for Claude and \
        other MCP clients.\n\
        \n\
        Runs over stdio transport — add it to your MCP client config and it will be launched \
        automatically.\n\
        \n\
        DEBUGGING\n\
        \n\
        Pass --debug to log every Proxmox request (method + URL) and full error response \
        bodies. Output is JSON on stderr unless --log-file is given, which is the reliable way \
        to capture a trace when an MCP client spawns the server (its stderr is otherwise hard \
        to reach). RUST_LOG, if set, overrides --debug (e.g. RUST_LOG=proxmox_mcp=trace). The \
        API token is never logged.",
    version
)]
struct Args {
    /// Path to the configuration file (default: ~/.proxmox_mcp.json)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Log every Proxmox request and full error bodies (sets proxmox_mcp=debug
    /// unless RUST_LOG is set). The token is never logged.
    #[arg(long)]
    debug: bool,

    /// Write log output to this file instead of stderr (append + create).
    /// Use this to capture a debug trace when the server is spawned by an MCP client.
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,
}

/// Decide the tracing filter directive. An explicit, non-empty `RUST_LOG`
/// always wins; otherwise `--debug` raises this crate to `debug`, and the
/// default stays `info`.
fn log_directive(rust_log: Option<&str>, debug: bool) -> String {
    match rust_log {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ if debug => "proxmox_mcp=debug".to_string(),
        _ => "info".to_string(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let directive = log_directive(std::env::var("RUST_LOG").ok().as_deref(), args.debug);
    let writer = match &args.log_file {
        Some(path) => {
            let mut opts = OpenOptions::new();
            opts.create(true).append(true);
            // The trace can hold private data (request URLs, error bodies), so
            // restrict it to the owner — matching the world-readable rejection
            // config.rs applies to the credentials file.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let file = opts
                .open(path)
                .with_context(|| format!("opening log file {}", path.display()))?;
            BoxMakeWriter::new(Mutex::new(file))
        }
        None => BoxMakeWriter::new(std::io::stderr),
    };
    tracing_subscriber::registry()
        .with(EnvFilter::new(directive))
        .with(fmt::layer().json().with_writer(writer))
        .init();

    let cfg = config::Config::load(args.config.as_deref())?;
    let conn = cfg.resolve()?;

    info!(
        mode = "stdio",
        proxmox_url = %conn.url,
        insecure = conn.insecure,
        "starting proxmox-mcp"
    );
    let server = tools::ProxmoxMcpServer::new(conn)?;
    server
        .serve(rmcp::transport::io::stdio())
        .await?
        .waiting()
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::log_directive;

    #[test]
    fn rust_log_takes_precedence_over_debug() {
        assert_eq!(
            log_directive(Some("proxmox_mcp=trace"), true),
            "proxmox_mcp=trace"
        );
        assert_eq!(log_directive(Some("warn"), false), "warn");
    }

    #[test]
    fn debug_flag_raises_this_crate_when_no_rust_log() {
        assert_eq!(log_directive(None, true), "proxmox_mcp=debug");
        // Empty/whitespace RUST_LOG is treated as unset.
        assert_eq!(log_directive(Some(""), true), "proxmox_mcp=debug");
        assert_eq!(log_directive(Some("   "), true), "proxmox_mcp=debug");
    }

    #[test]
    fn default_is_info() {
        assert_eq!(log_directive(None, false), "info");
        assert_eq!(log_directive(Some(""), false), "info");
    }
}
