//! `config.toml` loading and validation. Secrets never live here: the API key
//! comes from the `ANTHROPIC_API_KEY` environment variable.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use protocol::{CaptureSettings, MAX_SCAN_SEGMENTS_CAP};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Must be a loopback address (P7).
    pub bind: String,
    /// Root of `memory/` and `records/`.
    pub data_dir: PathBuf,
    pub claude: ClaudeConfig,
    pub capture: CaptureConfig,
    pub web_search: WebSearchConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeConfig {
    pub model: String,
    /// Overridable so tests can point at a local mock.
    pub base_url: String,
    pub api_version: String,
    pub max_tokens: u32,
    /// Effort for ordinary conversation turns.
    pub effort: String,
    /// Effort for deep review and the end-of-session summary.
    pub deep_effort: String,
    /// `view_screen` results kept in context by server-side context editing.
    pub keep_screen_views: u32,
    /// Prompt size (input tokens) at which old screen views start being cleared.
    pub clear_trigger_tokens: u32,
    /// A clear only happens if it frees at least this many tokens, so the
    /// cached conversation is rewritten rarely and in large batches.
    pub clear_at_least_tokens: u32,
    pub max_retries: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub sample_interval_ms: u32,
    pub scan_sample_interval_ms: u32,
    /// At most 2000 (the API's per-image limit for requests with >20 images).
    pub max_long_edge: u32,
    pub jpeg_quality: f64,
    pub keepalive_ms: u32,
    pub change_threshold_bits: u32,
    /// At most `MAX_SCAN_SEGMENTS_CAP` (20).
    pub max_scan_segments: u32,
}

/// Largest frame long edge the app will ever send or accept (P3).
pub const MAX_LONG_EDGE_CAP: u32 = 2000;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    pub enabled: bool,
    pub max_uses: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8765".into(),
            data_dir: PathBuf::from("data"),
            claude: ClaudeConfig::default(),
            capture: CaptureConfig::default(),
            web_search: WebSearchConfig::default(),
        }
    }
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-5-5".into(),
            base_url: "https://api.anthropic.com".into(),
            api_version: "2026-10-21".into(),
            max_tokens: 16_000,
            effort: "low".into(),
            deep_effort: "high".into(),
            keep_screen_views: 2,
            clear_trigger_tokens: 60_000,
            clear_at_least_tokens: 15_000,
            max_retries: 4,
        }
    }
}

impl Default for CaptureConfig {
    fn default() -> Self {
        let d = CaptureSettings::default();
        Self {
            sample_interval_ms: d.sample_interval_ms,
            scan_sample_interval_ms: d.scan_sample_interval_ms,
            max_long_edge: d.max_long_edge,
            jpeg_quality: d.jpeg_quality,
            keepalive_ms: d.keepalive_ms,
            change_threshold_bits: d.change_threshold_bits,
            max_scan_segments: d.max_scan_segments,
        }
    }
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self { enabled: false, max_uses: 3 }
    }
}

impl CaptureConfig {
    pub fn settings(&self) -> CaptureSettings {
        CaptureSettings {
            sample_interval_ms: self.sample_interval_ms,
            scan_sample_interval_ms: self.scan_sample_interval_ms,
            max_long_edge: self.max_long_edge,
            jpeg_quality: self.jpeg_quality,
            keepalive_ms: self.keepalive_ms,
            change_threshold_bits: self.change_threshold_bits,
            max_scan_segments: self.max_scan_segments,
        }
    }
}

impl Config {
    /// Loads `path` if it exists, otherwise the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Enforces the hard caps from the privacy invariants.
    pub fn validate(&self) -> anyhow::Result<()> {
        let c = &self.capture;
        if c.max_long_edge == 0 || c.max_long_edge > MAX_LONG_EDGE_CAP {
            bail!("capture.max_long_edge must be 1..={MAX_LONG_EDGE_CAP} (got {})", c.max_long_edge);
        }
        if c.max_scan_segments == 0 || c.max_scan_segments > MAX_SCAN_SEGMENTS_CAP {
            bail!("capture.max_scan_segments must be 1..={MAX_SCAN_SEGMENTS_CAP} (got {})", c.max_scan_segments);
        }
        Ok(())
    }

    /// The address to bind, refusing anything that isn't loopback (P7).
    pub fn bind_addr(&self) -> anyhow::Result<SocketAddr> {
        let addr: SocketAddr = self
            .bind
            .parse()
            .with_context(|| format!("bind = {:?} is not an IP:port address", self.bind))?;
        if !addr.ip().is_loopback() {
            bail!(
                "refusing to bind {addr}: this app only listens on loopback (127.0.0.1 or ::1)"
            );
        }
        Ok(addr)
    }

    /// One line per outbound service, for the startup banner (P7).
    pub fn outbound_services(&self) -> Vec<String> {
        let mut out = vec![format!(
            "Anthropic Messages API at {} (model {})",
            self.claude.base_url, self.claude.model
        )];
        if self.web_search.enabled {
            out.push(format!(
                "Anthropic server-side web search (max {} uses per request)",
                self.web_search.max_uses
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_bind(bind: &str) -> Config {
        Config { bind: bind.into(), ..Config::default() }
    }

    #[test]
    fn loopback_addresses_are_accepted() {
        assert!(with_bind("127.0.0.1:8765").bind_addr().is_ok());
        assert!(with_bind("[::1]:8765").bind_addr().is_ok());
    }

    #[test]
    fn non_loopback_addresses_are_refused() {
        for bind in ["0.0.0.0:8765", "[::]:8765", "192.168.1.10:8765", "8.8.8.8:80", "localhost:8765"] {
            assert!(with_bind(bind).bind_addr().is_err(), "{bind} should be refused");
        }
    }

    #[test]
    fn hard_caps_are_enforced() {
        assert!(Config::default().validate().is_ok());
        let mut c = Config::default();
        c.capture.max_long_edge = 2576;
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.capture.max_scan_segments = 21;
        assert!(c.validate().is_err());
    }

    #[test]
    fn web_search_is_off_by_default() {
        let c = Config::default();
        assert!(!c.web_search.enabled);
        assert_eq!(c.outbound_services().len(), 1);
    }
}
