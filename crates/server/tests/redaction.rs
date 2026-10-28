//! Acceptance test 5: the redaction linter flags account numbers, card
//! numbers, SSNs, routing numbers, emails, secrets, and street addresses, and
//! leaves ordinary financial text alone.

use server::redact::{lint, FlagKind};

fn kinds(text: &str) -> Vec<FlagKind> {
    lint(text).into_iter().map(|f| f.kind).collect()
}

#[test]
fn flags_identifiers() {
    use FlagKind::*;
    let cases: &[(&str, FlagKind)] = &[
        ("Brokerage account 123456789012", AccountNumber),
        ("acct #00098765", AccountNumber),
        ("account 1234-5678-9012", AccountNumber),
        ("Visa 4111 1111 1111 1111", CardNumber),
        ("card 4111111111111111 exp 09/29", CardNumber),
        ("Amex 3782-822463-10005", CardNumber),
        ("SSN 123-45-6789", Ssn),
        ("ssn 123 45 6789", Ssn),
        ("routing 021000021", RoutingNumber),
        ("ABA 011000015", RoutingNumber),
        ("contact jane.doe+bank@example.co.uk", Email),
        ("password: hunter2", Secret),
        ("my passcode is Tr0ub4dor", Secret),
        ("one-time code 552019", Secret),
        ("PIN 4821", Secret),
        ("login Xk9#mQ2!pL", Secret),
        ("mailing address 1600 Pennsylvania Avenue NW", StreetAddress),
        ("lives at 42 W Elm St.", StreetAddress),
        ("PO Box 7731", StreetAddress),
    ];
    for (text, kind) in cases {
        assert!(kinds(text).contains(kind), "{text:?} should be flagged as {kind:?}, got {:?}", kinds(text));
    }
}

#[test]
fn leaves_ordinary_financial_text_alone() {
    let clean = [
        "$12,345",
        "2026",
        "Balance $12345.67 as of 10/27/2026",
        "Fidelity brokerage ...1234",
        "Vanguard IRA xxxx9876, Chase ****4321",
        "VTSAX expense ratio 0.04%; FXAIX 0.015%; bond fund 0.00035 per dollar",
        "About 1,250 shares of VTI, roughly $310,000, 62% of the portfolio",
        "Total 1,234,567 dollars",
        "Q3 2026: 14 holdings, 401(k) and Roth IRA, S&P 500 index",
        "Target Date 2050 fund, 7-day yield 4.85%",
        "Let's pin down your allocation before the 2027 tax year",
        "Statement period 2026-09-01 to 2026-09-30",
    ];
    for text in clean {
        assert!(lint(text).is_empty(), "{text:?} should not be flagged, got {:?}", lint(text));
    }
}
