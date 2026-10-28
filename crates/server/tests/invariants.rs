//! Privacy invariant checks: acceptance tests 1 (no control), 3 (no frames on
//! disk), 7 (localhost only), and 9 (JS budget).

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use common::*;
use protocol::{ClientMsg, ServerMsg};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

// ------------------------------------------------------------ test 1

const FORBIDDEN_CRATES: &[&str] = &[
    "enigo",
    "rdev",
    "autopilot",
    "fantoccini",
    "thirtyfour",
    "headless_chrome",
    "chromiumoxide",
];

#[test]
fn no_input_or_browser_automation_dependencies() {
    let out = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "-e", "normal", "--prefix", "none", "--format", "{p}"])
        .current_dir(repo_root())
        .output()
        .expect("run cargo tree");
    assert!(out.status.success(), "cargo tree failed: {}", String::from_utf8_lossy(&out.stderr));
    let tree = String::from_utf8(out.stdout).unwrap();
    assert!(tree.lines().any(|l| l.starts_with("server ")), "cargo tree output looks wrong");
    for line in tree.lines() {
        let name = line.split_whitespace().next().unwrap_or("");
        let normalized = name.replace('-', "_");
        assert!(
            !FORBIDDEN_CRATES.contains(&normalized.as_str()),
            "forbidden automation crate in non-dev dependencies: {line}"
        );
    }
}

#[test]
fn tool_list_is_view_screen_and_notes_only() {
    let mut cfg = server::config::Config::default();
    let names = |cfg: &server::config::Config| -> Vec<String> {
        server::claude::tools(cfg).iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(names(&cfg), vec!["view_screen", "record_screen_notes"]);
    cfg.web_search.enabled = true;
    assert_eq!(names(&cfg), vec!["view_screen", "record_screen_notes", "web_search"]);
    // The client-executed tools are view_screen and record_screen_notes, both
    // strict; web search is server-side.
    let tools = server::claude::tools(&cfg);
    assert!(tools[..2].iter().all(|t| t["strict"] == true));
    assert_eq!(tools[2]["type"], "web_search_20260209");
    assert!(tools[2]["max_uses"].as_u64().unwrap() <= 5);
}

// ------------------------------------------------------------ test 3

/// `frames.rs` must not touch the filesystem at all.
#[test]
fn frame_ring_has_no_disk_io() {
    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/frames.rs")).unwrap();
    let code: String = src.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    for needle in ["std::fs", "tokio::fs", "File::", "OpenOptions", "tempfile", "temp_dir", "std::io::Write", "BufWriter"] {
        assert!(!code.contains(needle), "frames.rs must not use {needle}");
    }
}

fn contains_image_magic(bytes: &[u8]) -> bool {
    const JPEG: &[u8] = b"\xFF\xD8\xFF";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    bytes.windows(JPEG.len()).any(|w| w == JPEG) || bytes.windows(PNG.len()).any(|w| w == PNG)
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Runs the real binary with its own data dir and TMPDIR, streams frames
/// through a full turn, then scans both directories for image bytes.
#[tokio::test]
async fn scripted_session_leaves_no_frames_on_disk() {
    let work = tempfile::tempdir().unwrap();
    let data = work.path().join("data");
    let tmp = work.path().join("tmp");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();

    let mock = spawn_mock_claude(vec![MockReply::Sse("tool_use.sse"), MockReply::Sse("text.sse")]).await;
    let port = free_port();
    let cfg_path = work.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "bind = \"127.0.0.1:{port}\"\ndata_dir = \"{}\"\n[claude]\nbase_url = \"{}\"\n",
            data.display(),
            mock.base_url
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_financial-advisor"))
        .arg(&cfg_path)
        .env("TMPDIR", &tmp)
        .env("ANTHROPIC_API_KEY", "test-key")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _guard = Running(child);
    let mut lines = BufReader::new(stdout).lines();
    let banner: Vec<String> = lines.by_ref().map(|l| l.unwrap()).take_while(|l| !l.starts_with("Open http")).collect();
    assert!(banner.iter().any(|l| l.contains("Anthropic Messages API")), "startup lists outbound services");
    assert!(banner.iter().any(|l| l.contains("web search: off")));

    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.set_viewing(true).await;
    // Real JPEG/PNG magic so a leak would be detectable.
    for t in 0..5u64 {
        let mut jpeg = b"\xFF\xD8\xFF\xE0\x00\x10JFIF".to_vec();
        jpeg.extend_from_slice(&t.to_be_bytes());
        c.send_frame(t, &jpeg).await;
    }
    // ...and a page scan, whose segments also stay in RAM only.
    c.start_scan(1).await;
    for i in 0..4u16 {
        let mut jpeg = b"\xFF\xD8\xFF\xE0\x00\x10JFIF-segment".to_vec();
        jpeg.extend_from_slice(&i.to_be_bytes());
        c.send_segment(1, i, (i > 0).then_some(600), false, &jpeg).await;
        assert!(matches!(c.recv().await, ServerMsg::ScanState { segments, .. } if segments == u32::from(i) + 1));
    }
    c.send(ClientMsg::EndScan { scan_id: 1 }).await;
    assert!(matches!(c.recv().await, ServerMsg::ScanState { active: false, segments: 4, .. }));
    c.send(ClientMsg::UserText { text: "look at my holdings".into() }).await;
    let seen = c.recv_until(done).await;
    assert!(seen.iter().any(|m| matches!(m, ServerMsg::ToolActivity { detail, .. } if detail.contains("frames"))));
    c.send(ClientMsg::EndSession).await;
    c.recv_until(|m| matches!(m, ServerMsg::SessionEnded { .. })).await;

    for dir in [&data, &tmp] {
        for f in files_under(dir) {
            let bytes = std::fs::read(&f).unwrap();
            assert!(!contains_image_magic(&bytes), "image bytes written to disk: {}", f.display());
        }
    }
}

// ------------------------------------------------------------ test 7

#[test]
fn binary_refuses_non_loopback_bind() {
    for bind in ["0.0.0.0:8765", "[::]:8765", "192.168.1.20:8765"] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, format!("bind = \"{bind}\"\n")).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_financial-advisor")).arg(&cfg).output().unwrap();
        assert!(!out.status.success(), "{bind} must be refused");
        assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"), "{bind}: explains why");
    }
}

// ------------------------------------------------------------ test 9

#[test]
fn only_hand_written_js_is_the_audio_worklet() {
    const SKIP_DIRS: &[&str] = &["target", "dist", "node_modules", ".git", "data", "pkg"];
    fn walk(dir: &Path, root: &Path, found: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    walk(&p, root, found);
                }
            } else if [".js", ".mjs", ".cjs", ".ts", ".tsx", ".jsx"].iter().any(|x| name.ends_with(x)) {
                found.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let root = repo_root();
    let mut found = Vec::new();
    walk(&root, &root, &mut found);
    let extra: Vec<&String> = found.iter().filter(|f| f.as_str() != "web/audio-worklet.js").collect();
    assert!(extra.is_empty(), "hand-written JS/TS outside the budget: {extra:?}");

    let worklet = root.join("web/audio-worklet.js");
    if worklet.exists() {
        let lines = std::fs::read_to_string(worklet).unwrap().lines().count();
        assert!(lines <= 40, "web/audio-worklet.js must stay under ~40 lines (has {lines})");
    }
}

// ------------------------------------------------------------ prompts

/// The fenced block under a PROMPT.md heading, as the prompt files store it.
fn appendix_block(heading: &str, fence: &str) -> String {
    let spec = std::fs::read_to_string(repo_root().join("PROMPT.md")).unwrap();
    let section = spec.split(heading).nth(1).expect("appendix heading");
    let body = section.split(fence).nth(1).expect("opening fence");
    let block = body.split("\n```").next().expect("closing fence");
    format!("{}\n", block.trim_start_matches('\n').trim_end_matches('\n'))
}

#[test]
fn advisor_prompt_is_appendix_a_verbatim() {
    let expected = appendix_block("## Appendix A", "```\n");
    assert_eq!(server::claude::ADVISOR_SYSTEM_PROMPT, expected, "prompts/advisor_system.md must match Appendix A");
}

#[test]
fn summary_template_contains_appendix_b() {
    let template = appendix_block("## Appendix B", "```markdown\n");
    assert!(server::claude::SUMMARY_TEMPLATE.contains(template.trim_end()), "summary_template.md must contain Appendix B");
}
