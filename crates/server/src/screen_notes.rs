//! Screen notes (P9): the advisor's plain-text record of facts it read off
//! the user's screen this session. RAM only; never written to `data/memory/`.
//! Every note is linted before it's accepted.

use crate::redact::{self, Flag};

/// Longest accepted page label, in characters.
pub const MAX_PAGE_CHARS: usize = 120;
/// Longest accepted note body, in characters.
pub const MAX_NOTES_CHARS: usize = 8_000;
/// Most distinct pages kept per session.
pub const MAX_PAGES: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenNote {
    /// Stable per page label within the session.
    pub id: u32,
    pub page: String,
    pub notes: String,
    /// 1 for the first note on a page; incremented each time it's superseded.
    pub revision: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteError {
    Empty,
    TooLong { field: &'static str, max: usize },
    TooManyPages,
    /// The redaction linter flagged the text; the message masks identifiers.
    Flagged { field: &'static str, flags: Vec<Flag>, description: String },
}

impl NoteError {
    /// Short reason, safe to log (no note text).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong { .. } => "too long",
            Self::TooManyPages => "too many pages",
            Self::Flagged { .. } => "flagged by the redaction linter",
        }
    }

    /// Text for the tool_result, telling the model how to fix the note.
    pub fn message(&self) -> String {
        match self {
            Self::Empty => "Note not saved: page and notes must both be non-empty.".into(),
            Self::TooLong { field, max } => {
                format!("Note not saved: {field} is longer than {max} characters. Keep only the facts you need.")
            }
            Self::TooManyPages => format!(
                "Note not saved: this session already has notes for {MAX_PAGES} pages. Update an existing \
                 page label instead."
            ),
            Self::Flagged { field, description, .. } => format!(
                "Note not saved: the redaction linter flagged the {field}: {description}. Rewrite it with \
                 institution plus last four digits at most, rounded figures where exact ones aren't needed, \
                 and no credentials or personal identifiers, then call record_screen_notes again."
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct ScreenNotes {
    notes: Vec<ScreenNote>,
    next_id: u32,
}

fn key(page: &str) -> String {
    page.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn check(field: &'static str, text: &str, max: usize) -> Result<(), NoteError> {
    if text.trim().is_empty() {
        return Err(NoteError::Empty);
    }
    if text.chars().count() > max {
        return Err(NoteError::TooLong { field, max });
    }
    let flags = redact::lint(text);
    if !flags.is_empty() {
        let description = redact::describe(text, &flags);
        return Err(NoteError::Flagged { field, flags, description });
    }
    Ok(())
}

impl ScreenNotes {
    /// Records a note, or supersedes the note with the same page label
    /// (compared case- and whitespace-insensitively).
    pub fn record(&mut self, page: &str, notes: &str) -> Result<ScreenNote, NoteError> {
        let page = page.trim();
        let notes = notes.trim();
        check("page label", page, MAX_PAGE_CHARS)?;
        check("notes", notes, MAX_NOTES_CHARS)?;
        let k = key(page);
        if let Some(existing) = self.notes.iter_mut().find(|n| key(&n.page) == k) {
            existing.page = page.to_string();
            existing.notes = notes.to_string();
            existing.revision += 1;
            return Ok(existing.clone());
        }
        if self.notes.len() >= MAX_PAGES {
            return Err(NoteError::TooManyPages);
        }
        self.next_id += 1;
        let note = ScreenNote { id: self.next_id, page: page.to_string(), notes: notes.to_string(), revision: 1 };
        self.notes.push(note.clone());
        Ok(note)
    }

    /// Current notes, one per page, in the order pages were first noted.
    pub fn current(&self) -> &[ScreenNote] {
        &self.notes
    }

    pub fn wipe(&mut self) {
        self.notes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_supersedes_by_page_label() {
        let mut n = ScreenNotes::default();
        let a = n.record("Fidelity brokerage ...1234 positions", "VTI about $42,000 (61%).").unwrap();
        assert_eq!((a.id, a.revision), (1, 1));
        let b = n.record("Vanguard IRA ...9876", "Target 2050 fund.").unwrap();
        assert_eq!(b.id, 2);
        let a2 = n.record("  fidelity BROKERAGE ...1234   positions ", "VTI about $43,000 (62%).").unwrap();
        assert_eq!((a2.id, a2.revision), (1, 2));
        assert_eq!(n.current().len(), 2);
        assert_eq!(n.current()[0].notes, "VTI about $43,000 (62%).");
    }

    #[test]
    fn rejects_flagged_empty_and_oversized() {
        let mut n = ScreenNotes::default();
        let err = n.record("Chase checking", "Account 000123456789, balance $3,100").unwrap_err();
        let msg = err.message();
        assert!(matches!(err, NoteError::Flagged { field: "notes", .. }));
        assert!(msg.contains("…6789") && !msg.contains("000123456789"), "{msg}");
        assert!(matches!(n.record("Chase ...6789", " "), Err(NoteError::Empty)));
        assert!(matches!(
            n.record("x", &"a".repeat(MAX_NOTES_CHARS + 1)),
            Err(NoteError::TooLong { field: "notes", .. })
        ));
        assert!(matches!(n.record("me@example.com", "x"), Err(NoteError::Flagged { field: "page label", .. })));
        assert!(n.current().is_empty());
    }
}
