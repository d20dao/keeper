//! Data templates: the exact signed-data grammar of an epoch recipe, with the verdicts of EpochEntropy's
//! DataTemplate library. A template is a sequence of segments, each an opcode byte and its operands:
//!
//! - `0x01 len bytes[len]` LITERAL: exactly these bytes (1 <= len <= 128)
//! - `0x02 n` HEX: exactly n characters 0-9 or a-f (1 <= n <= 128)
//! - `0x03 flags` DECIMAL: an unsigned JSON number, 0 or a nonzero digit then digits, with an optional
//!   fraction when flags & 1 and an optional exponent when flags & 2 (flags <= 3)
//! - `0x04 min max` INTEGER: a nonzero digit then digits, min <= digit count <= max (1 <= min <= max <= 128)
//!
//! Variable segments are greedy and never backtrack; data matches when the segments consume it exactly. A
//! well-formed template has at most 256 bytes, at least one variable segment and a shortest match of at most
//! 128 bytes.

pub const MAX_DATA_BYTES: usize = 128;
pub const MAX_TEMPLATE_BYTES: usize = 256;
const LITERAL: u8 = 0x01;
const HEX: u8 = 0x02;
const DECIMAL: u8 = 0x03;
const INTEGER: u8 = 0x04;
const FRACTION: u8 = 0x01;
const EXPONENT: u8 = 0x02;

/// Whether a template is well-formed.
pub fn is_valid(template: &[u8]) -> bool {
    if template.is_empty() || template.len() > MAX_TEMPLATE_BYTES {
        return false;
    }
    let (mut t, mut shortest, mut variable) = (0usize, 0usize, false);
    let operand = |at: usize| template.get(at).map(|b| usize::from(*b));
    while t < template.len() {
        match template[t] {
            LITERAL => {
                let Some(n) = operand(t + 1) else {
                    return false;
                };
                if n == 0 || n > MAX_DATA_BYTES || t + 2 + n > template.len() {
                    return false;
                }
                shortest += n;
                t += 2 + n;
            }
            HEX => {
                let Some(n) = operand(t + 1) else {
                    return false;
                };
                if n == 0 || n > MAX_DATA_BYTES {
                    return false;
                }
                shortest += n;
                t += 2;
                variable = true;
            }
            DECIMAL => {
                if operand(t + 1).is_none_or(|flags| flags > usize::from(FRACTION | EXPONENT)) {
                    return false;
                }
                shortest += 1;
                t += 2;
                variable = true;
            }
            INTEGER => {
                let (Some(min), Some(max)) = (operand(t + 1), operand(t + 2)) else {
                    return false;
                };
                if min == 0 || min > max || max > MAX_DATA_BYTES {
                    return false;
                }
                shortest += min;
                t += 3;
                variable = true;
            }
            _ => return false,
        }
    }
    variable && shortest <= MAX_DATA_BYTES
}

/// Whether data is exactly a record the template describes; false for a malformed template.
pub fn matches(template: &[u8], data: &[u8]) -> bool {
    if !is_valid(template) || data.is_empty() || data.len() > MAX_DATA_BYTES {
        return false;
    }
    let (mut t, mut p) = (0usize, 0usize);
    while t < template.len() {
        let (next, width) = match template[t] {
            LITERAL => {
                let n = usize::from(template[t + 1]);
                let end = p + n;
                (
                    (data.get(p..end) == Some(&template[t + 2..t + 2 + n])).then_some(end),
                    2 + n,
                )
            }
            HEX => {
                let end = p + usize::from(template[t + 1]);
                let hex = data.get(p..end).is_some_and(|chars| {
                    chars
                        .iter()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
                });
                (hex.then_some(end), 2)
            }
            DECIMAL => (
                decimal(
                    data,
                    p,
                    template[t + 1] & FRACTION != 0,
                    template[t + 1] & EXPONENT != 0,
                ),
                2,
            ),
            _ => (
                integer(
                    data,
                    p,
                    usize::from(template[t + 1]),
                    usize::from(template[t + 2]),
                ),
                3,
            ),
        };
        let Some(end) = next else {
            return false;
        };
        p = end;
        t += width;
    }
    p == data.len()
}

fn digits(data: &[u8], mut p: usize) -> usize {
    while data.get(p).is_some_and(u8::is_ascii_digit) {
        p += 1;
    }
    p
}

/// End of an unsigned JSON number starting at p.
fn decimal(data: &[u8], mut p: usize, fraction: bool, exponent: bool) -> Option<usize> {
    match data.get(p) {
        Some(b'0') => {
            p += 1;
            if data.get(p).is_some_and(u8::is_ascii_digit) {
                return None;
            }
        }
        Some(c) if c.is_ascii_digit() => p = digits(data, p),
        _ => return None,
    }
    if fraction && data.get(p) == Some(&b'.') {
        let first = p + 1;
        p = digits(data, first);
        if p == first {
            return None;
        }
    }
    if exponent && matches!(data.get(p), Some(b'e' | b'E')) {
        p += 1;
        if matches!(data.get(p), Some(b'+' | b'-')) {
            p += 1;
        }
        let first = p;
        p = digits(data, first);
        if p == first {
            return None;
        }
    }
    Some(p)
}

/// End of a positive integer with a digit count in [min, max] starting at p.
fn integer(data: &[u8], p: usize, min: usize, max: usize) -> Option<usize> {
    if !data.get(p).is_some_and(|c| (b'1'..=b'9').contains(c)) {
        return None;
    }
    let end = digits(data, p);
    (min..=max).contains(&(end - p)).then_some(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn hex_bytes(text: &str) -> Vec<u8> {
        hex::decode(text.trim_start_matches("0x")).unwrap()
    }

    #[test]
    fn fixture_verdicts_match_the_contract_and_the_replay_library() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../test/fixtures/epoch-data-cases.json"))
                .unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() > 2000);
        let mut accepted = 0;
        for case in cases {
            let name = case["template"].as_str().unwrap();
            let template = hex_bytes(fixture["templates"][name]["template"].as_str().unwrap());
            let data = case["data"].as_str().unwrap();
            let valid = case["valid"].as_bool().unwrap();
            assert_eq!(
                matches(&template, data.as_bytes()),
                valid,
                "{name} ({}): {} {data:?}",
                case["origin"],
                case["note"]
            );
            accepted += usize::from(valid);
        }
        assert!(accepted > 200);
        let validity = fixture["templateValidity"].as_array().unwrap();
        assert!(validity.len() > 400);
        for case in validity {
            let template = hex_bytes(case["template"].as_str().unwrap());
            assert_eq!(
                is_valid(&template),
                case["valid"].as_bool().unwrap(),
                "{}: {}",
                case["note"],
                case["template"]
            );
        }
    }

    #[test]
    fn malformed_templates_never_match_and_numbers_follow_the_json_grammar() {
        let record = br#"{"a":1}"#;
        for template in [
            &b""[..],
            b"\x01\x05{\"a\":",
            b"\x05\x01",
            b"\x03\x04",
            b"\x04\x02\x01",
        ] {
            assert!(!matches(template, record));
        }
        let number = |text: &str| matches(b"\x03\x03", text.as_bytes());
        for valid in ["0", "0.00001", "6e-8", "1E+2", "12.3", "76634.54000000001"] {
            assert!(number(valid), "{valid}");
        }
        for bad in [
            "-1", "01", ".1", "1.", "1e", "1e+", "NaN", "Infinity", "1,2", "",
        ] {
            assert!(!number(bad), "{bad}");
        }
        let timestamp = |text: &str| matches(b"\x04\x01\x10", text.as_bytes());
        assert!(timestamp("9999999999999999"));
        assert!(!timestamp("0"));
        assert!(!timestamp("12345678901234567"));
        assert!(matches(b"\x02\x02", b"af"));
        assert!(!matches(b"\x02\x02", b"aF"));
    }
}
