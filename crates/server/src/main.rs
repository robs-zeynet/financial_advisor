use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use server::claude::ClaudeClient;
use server::config::Config;
use server::session::AppState;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config_path = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| "config.toml".into());
    let config = Config::load(&config_path)?;
    // Refuse anything but loopback before doing anything else (P7).
    let addr = config.bind_addr()?;

    let api_key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty());
    if api_key.is_none() {
        eprintln!("warning: ANTHROPIC_API_KEY is not set; the advisor can't answer until it is.");
    }

    println!("Outbound services enabled:");
    for s in config.outbound_services() {
        println!("  - {s}");
    }
    if !config.web_search.enabled {
        println!("  (web search: off)");
    }

    let app = Arc::new(AppState { claude: ClaudeClient::new(config.claude.clone(), api_key), config });
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    println!("Open http://{addr} — share a single tab or window, not your whole screen.");

    // Ctrl-C exits right away rather than waiting on open calls; frames only
    // ever lived in this process's memory, so they go with it.
    tokio::select! {
        r = axum::serve(listener, server::web::router(app, addr)) => r?,
        _ = tokio::signal::ctrl_c() => println!("Shutting down."),
    }
    Ok(())
}
