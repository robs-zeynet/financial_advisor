//! Uploaded PDFs. Like screen frames, an uploaded PDF lives only in memory:
//! never written to disk, wiped when replaced, removed, on hang-up, and on
//! exit. This module must not touch the filesystem (checked by a test).

use protocol::{DocumentInfo, MAX_PDF_BYTES};
use sha2::{Digest, Sha256};

use crate::redact;

pub struct UploadedPdf {
    pub name: String,
    pub bytes: Vec<u8>,
    /// Estimated from the PDF's page objects; `None` when they're compressed.
    pub pages: Option<u32>,
    pub sha256: String,
    viewed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfReject {
    Empty,
    NotPdf,
    TooLarge { max_mb: usize },
}

impl PdfReject {
    pub fn message(&self) -> String {
        match self {
            Self::Empty => "That file is empty.".into(),
            Self::NotPdf => "That file isn't a PDF. Print the page to PDF (Ctrl+P or Cmd+P, then Save as PDF) \
                             and upload that."
                .into(),
            Self::TooLarge { max_mb } => format!(
                "That PDF is larger than {max_mb} MB. Try printing just the pages you need, or a smaller \
                 page range."
            ),
        }
    }
}

impl std::fmt::Debug for UploadedPdf {
    // Never print the document's bytes or name in logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadedPdf")
            .field("bytes", &self.bytes.len())
            .field("pages", &self.pages)
            .field("sha256", &self.sha256)
            .finish()
    }
}

/// Checks and keeps an uploaded PDF.
pub fn accept(name: &str, bytes: &[u8]) -> Result<UploadedPdf, PdfReject> {
    if bytes.is_empty() {
        return Err(PdfReject::Empty);
    }
    if bytes.len() > MAX_PDF_BYTES {
        return Err(PdfReject::TooLarge { max_mb: MAX_PDF_BYTES / (1024 * 1024) });
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err(PdfReject::NotPdf);
    }
    Ok(UploadedPdf {
        name: sanitize_name(name),
        pages: estimate_pages(bytes),
        sha256: Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect(),
        bytes: bytes.to_vec(),
        viewed: false,
    })
}

/// A display name safe to show the model: base name only, no control
/// characters, bounded length, and nothing the redaction linter flags (a
/// statement named after its account number becomes `uploaded.pdf`).
pub fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base.chars().filter(|c| !c.is_control()).take(80).collect();
    let clean = clean.trim();
    if clean.is_empty() || !redact::lint(clean).is_empty() {
        return "uploaded.pdf".into();
    }
    if clean.to_lowercase().ends_with(".pdf") { clean.to_string() } else { format!("{clean}.pdf") }
}

/// Counts `/Type /Page` objects (not `/Pages`). PDFs that keep page objects
/// in compressed object streams report `None`.
pub fn estimate_pages(bytes: &[u8]) -> Option<u32> {
    static PAGE: std::sync::LazyLock<regex::bytes::Regex> =
        std::sync::LazyLock::new(|| regex::bytes::Regex::new(r"/Type\s*/Page(?:[^s]|$)").expect("valid regex"));
    let n = PAGE.find_iter(bytes).count() as u32;
    (n > 0).then_some(n)
}

impl UploadedPdf {
    pub fn info(&self) -> DocumentInfo {
        DocumentInfo { name: self.name.clone(), bytes: self.bytes.len() as u64, pages: self.pages }
    }

    pub fn mark_viewed(&mut self) {
        self.viewed = true;
    }

    pub fn viewed(&self) -> bool {
        self.viewed
    }

    /// "about 6 pages" / "page count unknown", for messages to the model.
    pub fn pages_phrase(&self) -> String {
        match self.pages {
            Some(1) => "1 page".into(),
            Some(n) => format!("about {n} pages"),
            None => "page count unknown".into(),
        }
    }
}

impl Drop for UploadedPdf {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_pdfs_only_within_the_size_cap() {
        assert!(accept("a.pdf", b"%PDF-1.7\n/Type /Page\n/Type /Page\n/Type /Pages").is_ok());
        assert_eq!(accept("a.pdf", b"").err(), Some(PdfReject::Empty));
        assert_eq!(accept("a.pdf", b"\x89PNG....").err(), Some(PdfReject::NotPdf));
        let big = [b"%PDF-".as_slice(), &vec![0u8; MAX_PDF_BYTES]].concat();
        assert!(matches!(accept("a.pdf", &big), Err(PdfReject::TooLarge { .. })));
    }

    #[test]
    fn estimates_pages() {
        assert_eq!(estimate_pages(b"%PDF /Type /Page x /Type/Page /Type /Pages"), Some(2));
        assert_eq!(estimate_pages(b"%PDF-1.7 compressed"), None);
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_name("C:\\Users\\me\\Fidelity positions.pdf"), "Fidelity positions.pdf");
        assert_eq!(sanitize_name("/home/me/holdings"), "holdings.pdf");
        assert_eq!(sanitize_name("Statement_000123456789.pdf"), "uploaded.pdf");
        assert_eq!(sanitize_name("jane@example.com.pdf"), "uploaded.pdf");
        assert_eq!(sanitize_name("\u{7}"), "uploaded.pdf");
    }
}
