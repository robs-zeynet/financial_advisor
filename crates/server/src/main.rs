use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use server::claude::ClaudeClient;
use server::config::Config;
use server::session::{AppState, Voice};
use server::stt::WhisperStt;
use server::tts::PiperTts;
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

    let voice = load_voice(&config.voice);
    let app = Arc::new(AppState { claude: ClaudeClient::new(config.claude.clone(), api_key), config, voice });
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

/// Loads local speech engines. Missing models disable voice (typed chat still
/// works) rather than failing startup.
fn load_voice(cfg: &server::config::VoiceConfig) -> Voice {
    if !cfg.enabled {
        println!("Voice: off (voice.enabled = false)");
        return Voice::default();
    }
    let stt = match WhisperStt::load(&cfg.whisper_model, cfg.whisper_threads, cfg.whisper_fit_audio_ctx) {
        Ok(s) => Some(Arc::new(s) as Arc<dyn server::stt::SpeechToText>),
        Err(e) => {
            eprintln!("warning: speech-to-text off: {e:#}. Run scripts/fetch-models.sh.");
            None
        }
    };
    let tts = match PiperTts::new(&cfg.piper_bin, &cfg.piper_voice) {
        Ok(t) => Some(Arc::new(t) as Arc<dyn server::tts::TtsEngine>),
        Err(e) => {
            eprintln!("warning: spoken replies off: {e:#}. Run scripts/fetch-models.sh.");
            None
        }
    };
    let on = |b: bool| if b { "on" } else { "off" };
    println!(
        "Voice (local, no network): speech-to-text {} (Whisper), spoken replies {} (Piper)",
        on(stt.is_some()),
        on(tts.is_some())
    );
    Voice { stt, tts }
}
