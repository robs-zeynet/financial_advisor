//! Long-term memory (P4). The agent's cross-session context is built only by
//! [`load_memory`], from two sources:
//!
//! - `data/memory/profile.md`, written by hand (the agent never writes it)
//! - `data/memory/sessions/*.md`, summaries the user approved
//!
//! Nothing else (transcripts, records, drafts) ever enters a future session.

use std::path::{Path, PathBuf};

pub fn memory_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("memory")
}

pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    memory_dir(data_dir).join("sessions")
}

/// Renders the memory block appended after the system prompt:
///
/// ```text
/// <user_profile>
/// ...profile.md...
/// </user_profile>
/// <session_summaries>
/// <summary date="2026-10-20">...</summary>
/// </session_summaries>
/// ```
///
/// Summaries are ordered by file name, which starts with the session date.
/// Missing files are treated as empty.
pub fn load_memory(data_dir: &Path) -> std::io::Result<String> {
    let profile = read_optional(&memory_dir(data_dir).join("profile.md"))?;

    let mut summaries: Vec<PathBuf> = match std::fs::read_dir(sessions_dir(data_dir)) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    summaries.sort();

    let mut out = String::new();
    out.push_str("<user_profile>\n");
    push_body(&mut out, &profile);
    out.push_str("</user_profile>\n<session_summaries>\n");
    for path in summaries {
        let body = std::fs::read_to_string(&path)?;
        let date = summary_date(&path);
        out.push_str(&format!("<summary date=\"{date}\">"));
        out.push_str(body.trim());
        out.push_str("</summary>\n");
    }
    out.push_str("</session_summaries>");
    Ok(out)
}

fn push_body(out: &mut String, body: &str) {
    let body = body.trim();
    if !body.is_empty() {
        out.push_str(body);
        out.push('\n');
    }
}

fn read_optional(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// `2026-10-20.md` or `2026-10-20-2.md` -> `2026-10-20`.
fn summary_date(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    stem.get(..10).unwrap_or(stem).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_data_dir_renders_empty_tags() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_memory(dir.path()).unwrap(),
            "<user_profile>\n</user_profile>\n<session_summaries>\n</session_summaries>"
        );
    }

    #[test]
    fn renders_profile_and_sorted_summaries() {
        let dir = tempfile::tempdir().unwrap();
        let s = sessions_dir(dir.path());
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(memory_dir(dir.path()).join("profile.md"), "Retire at 60.\n").unwrap();
        std::fs::write(s.join("2026-10-20.md"), "B\n").unwrap();
        std::fs::write(s.join("2026-10-01.md"), "A\n").unwrap();
        std::fs::write(s.join("notes.txt"), "ignored").unwrap();
        assert_eq!(
            load_memory(dir.path()).unwrap(),
            "<user_profile>\nRetire at 60.\n</user_profile>\n<session_summaries>\n\
             <summary date=\"2026-10-01\">A</summary>\n\
             <summary date=\"2026-10-20\">B</summary>\n</session_summaries>"
        );
    }
}
