//! Redaction linter (P5, P9). Flags text that looks like it contains:
//!
//! - an account or card number: more than 4 consecutive digits, other than
//!   dollar amounts or years (grouped digits such as `1234 5678 9012` count)
//! - an SSN or bank routing number
//! - a password or one-time code
//! - an email address or street address
//!
//! Used on screen notes before they're accepted (M2) and on session summaries
//! before approval (M5). It errs toward flagging: a false positive costs a
//! rewrite, a miss can leak an identifier into memory.

use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagKind {
    AccountNumber,
    CardNumber,
    Ssn,
    RoutingNumber,
    Secret,
    Email,
    StreetAddress,
}

impl FlagKind {
    pub fn describe(self) -> &'static str {
        match self {
            Self::AccountNumber => "an account-number-like run of digits",
            Self::CardNumber => "a card number",
            Self::Ssn => "a Social Security number",
            Self::RoutingNumber => "a bank routing number",
            Self::Secret => "a password or one-time code",
            Self::Email => "an email address",
            Self::StreetAddress => "a street address",
        }
    }
}

/// One finding: `text[start..end]` looks like `kind`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Flag {
    pub kind: FlagKind,
    pub start: usize,
    pub end: usize,
}

impl Flag {
    /// The flagged text with everything but its last 4 characters masked, so
    /// error messages and logs don't repeat the identifier.
    pub fn masked(&self, text: &str) -> String {
        let s = &text[self.start..self.end];
        let n = s.chars().count();
        if n <= 4 {
            return "…".into();
        }
        let tail: String = s.chars().skip(n - 4).collect();
        format!("…{tail}")
    }
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

static EMAIL: LazyLock<Regex> = LazyLock::new(|| re(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}"));
static SSN: LazyLock<Regex> = LazyLock::new(|| re(r"\b\d{3}[- ]\d{2}[- ]\d{4}\b"));
/// Digit groups joined by single spaces or dashes (`4111 1111 1111 1111`).
static DIGIT_GROUPS: LazyLock<Regex> = LazyLock::new(|| re(r"\d+(?:[ -]\d+)+"));
static DIGIT_RUN: LazyLock<Regex> = LazyLock::new(|| re(r"\d+"));
/// "password: hunter2", "passcode is ..." (needs `:`, `=`, or "is").
static SECRET_LABELED: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(?:password|passcode|passwd|pwd)\b\s*(?:is|was|:|=)\s*\S+"));
/// "PIN 4821", "one-time code: 552 019" (needs digits, so "pin down" is fine).
static SECRET_CODE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(?:pin|one[- ]time (?:code|passcode|password)|otp|verification code|security code|2fa code|auth(?:entication)? code)\b(?:\s+(?:is|was))?\s*[:=]?\s*\d[\d -]{2,}\d")
});
static TOKEN: LazyLock<Regex> = LazyLock::new(|| re(r"\S{8,}"));
static STREET: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b\d{1,6}\s+(?:[A-Za-z0-9.'-]+\s+){1,4}(?:street|st|avenue|ave|road|rd|boulevard|blvd|lane|ln|drive|dr|court|ct|way|place|pl|terrace|ter|circle|cir|parkway|pkwy|highway|hwy)\b\.?")
});
static PO_BOX: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bp\.?\s?o\.?\s+box\s+\d+"));

/// Luhn checksum, used to tell card numbers from other long digit strings.
fn luhn(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            let d = d as u32;
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// ABA routing number checksum.
fn aba(digits: &[u8]) -> bool {
    digits.len() == 9 && {
        let w = [3, 7, 1, 3, 7, 1, 3, 7, 1];
        digits.iter().zip(w).map(|(&d, w)| d as u32 * w).sum::<u32>().is_multiple_of(10)
    }
}

fn digits_of(s: &str) -> Vec<u8> {
    s.bytes().filter(u8::is_ascii_digit).map(|b| b - b'0').collect()
}

/// Kind for a long digit string: card, routing, or account number.
fn classify_digits(digits: &[u8]) -> FlagKind {
    if (13..=19).contains(&digits.len()) && luhn(digits) {
        FlagKind::CardNumber
    } else if aba(digits) {
        FlagKind::RoutingNumber
    } else {
        FlagKind::AccountNumber
    }
}

/// A digit run that's part of a dollar amount: preceded by `$` (with optional
/// space and digits/commas in between, e.g. `$1,234567`) or followed by
/// "dollars" / "USD".
fn is_dollar_amount(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].trim_end_matches([',', '.']);
    let before = before.trim_end_matches(|c: char| c.is_ascii_digit() || c == ',' || c == '.');
    if before.trim_end().ends_with('$') {
        return true;
    }
    let after = text[end..].trim_start_matches(|c: char| c.is_ascii_digit() || c == ',' || c == '.');
    let after = after.trim_start().to_ascii_lowercase();
    after.starts_with("dollars") || after.starts_with("usd")
}

/// The digits after a decimal point (`0.00035`): a ratio, not an identifier.
fn is_decimal_fraction(text: &str, start: usize) -> bool {
    let b = text.as_bytes();
    start >= 2 && b[start - 1] == b'.' && b[start - 2].is_ascii_digit()
}

/// A token that mixes letters, digits, and symbols like a password would.
fn looks_like_password(token: &str) -> bool {
    if token.contains('@') || token.contains("://") || token.starts_with("www.") {
        return false;
    }
    let token = token.trim_end_matches(['.', ',', ';', ':', ')', '!', '?']);
    if token.chars().count() < 8 {
        return false;
    }
    let lower = token.chars().any(|c| c.is_ascii_lowercase());
    let upper = token.chars().any(|c| c.is_ascii_uppercase());
    let digit = token.chars().any(|c| c.is_ascii_digit());
    let symbol = token.chars().any(|c| c.is_ascii_punctuation() && !matches!(c, '-' | '/' | '.' | ',' | '(' | ')' | '%' | '$'));
    let letters = lower || upper;
    letters && digit && [lower, upper, digit, symbol].iter().filter(|&&b| b).count() >= 3
}

/// Lints `text`, returning flags in order of position (overlaps removed).
pub fn lint(text: &str) -> Vec<Flag> {
    let mut flags: Vec<Flag> = Vec::new();
    let mut add = |kind, start, end| flags.push(Flag { kind, start, end });

    for m in EMAIL.find_iter(text) {
        add(FlagKind::Email, m.start(), m.end());
    }
    for m in SSN.find_iter(text) {
        add(FlagKind::Ssn, m.start(), m.end());
    }
    for m in DIGIT_GROUPS.find_iter(text) {
        let digits = digits_of(m.as_str());
        if digits.len() > 8 && !is_dollar_amount(text, m.start(), m.end()) {
            add(classify_digits(&digits), m.start(), m.end());
        }
    }
    for m in DIGIT_RUN.find_iter(text) {
        if m.len() > 4 && !is_dollar_amount(text, m.start(), m.end()) && !is_decimal_fraction(text, m.start()) {
            add(classify_digits(&digits_of(m.as_str())), m.start(), m.end());
        }
    }
    for m in SECRET_LABELED.find_iter(text).chain(SECRET_CODE.find_iter(text)) {
        add(FlagKind::Secret, m.start(), m.end());
    }
    for m in TOKEN.find_iter(text) {
        if looks_like_password(m.as_str()) {
            add(FlagKind::Secret, m.start(), m.end());
        }
    }
    for m in STREET.find_iter(text).chain(PO_BOX.find_iter(text)) {
        add(FlagKind::StreetAddress, m.start(), m.end());
    }

    // Keep the most specific flag for overlapping spans: SSN/email/card/routing
    // beat a generic account-number match over the same digits.
    let rank = |k: FlagKind| match k {
        FlagKind::Ssn | FlagKind::Email | FlagKind::CardNumber | FlagKind::RoutingNumber => 0,
        FlagKind::Secret | FlagKind::StreetAddress => 1,
        FlagKind::AccountNumber => 2,
    };
    flags.sort_by_key(|f| (f.start, rank(f.kind), std::cmp::Reverse(f.end)));
    let mut out: Vec<Flag> = Vec::new();
    for f in flags {
        match out.iter_mut().find(|o| f.start < o.end && o.start < f.end) {
            None => out.push(f),
            Some(o) if rank(f.kind) < rank(o.kind) => *o = f,
            Some(o) => o.end = o.end.max(f.end),
        }
    }
    out.sort_by_key(|f| f.start);
    out
}

/// A one-line description of every flag, with identifiers masked.
pub fn describe(text: &str, flags: &[Flag]) -> String {
    flags
        .iter()
        .map(|f| format!("{} ({})", f.kind.describe(), f.masked(text)))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<FlagKind> {
        lint(text).into_iter().map(|f| f.kind).collect()
    }

    #[test]
    fn checksums() {
        assert!(luhn(&digits_of("4111111111111111")));
        assert!(!luhn(&digits_of("4111111111111112")));
        assert!(aba(&digits_of("021000021")));
        assert!(!aba(&digits_of("021000022")));
    }

    #[test]
    fn masks_all_but_last_four() {
        let text = "acct 123456789";
        let f = &lint(text)[0];
        assert_eq!(f.masked(text), "…6789");
    }

    #[test]
    fn overlapping_flags_keep_the_specific_kind() {
        assert_eq!(kinds("card 4111 1111 1111 1111"), vec![FlagKind::CardNumber]);
        assert_eq!(kinds("ssn 123-45-6789"), vec![FlagKind::Ssn]);
    }
}
