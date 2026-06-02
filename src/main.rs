mod client;
mod config;
mod tools;

use std::path::PathBuf;

use clap::Parser;
use rmcp::ServiceExt;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[derive(Parser)]
#[command(name = "proxmox-mcp", about = "Read-only MCP server for Proxmox VE")]
struct Args {
    /// Path to the configuration file (default: ~/.proxmox_mcp.json)
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // JSON logging to stderr; level controlled by RUST_LOG (default: info).
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().json().with_writer(std::io::stderr))
        .init();

    let args = Args::parse();

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
