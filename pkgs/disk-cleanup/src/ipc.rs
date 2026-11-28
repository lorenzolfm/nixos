//! Paired JSON IPC encoding, decoding, and invariant validation for `--_user-phase`.
//!
//! Zero external dependencies and zero recursion: the schema is a flat object with
//! six non-negative integers and a bounded array of error strings.

use std::fmt::Write as _;

use crate::constants::{ERRORS_MAX, SUBPROCESS_OUTPUT_BYTES_MAX};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UserPhaseStats {
    pub result_links_removed_count: u64,
    pub targets_removed_count: u64,
    pub targets_freed_bytes: u64,
    pub targets_skipped_active_count: u64,
    pub targets_skipped_recent_count: u64,
    pub user_profiles_pruned_count: u64,
    pub errors: Vec<String>,
}

/// Verify `UserPhaseStats` invariants (paired across JSON IPC write/read).
pub fn assert_user_phase_stats_valid(stats: &UserPhaseStats) {
    if stats.targets_removed_count == 0 {
        assert_eq!(
            stats.targets_freed_bytes, 0,
            "targets_freed_bytes must be 0 when no targets were removed"
        );
    }
    assert!(
        stats.errors.len() <= ERRORS_MAX,
        "errors list exceeded ERRORS_MAX"
    );
}

/// Serialize `UserPhaseStats` to a compact RFC 8259 JSON object.
pub fn encode_user_phase_stats_json(stats: &UserPhaseStats) -> String {
    assert_user_phase_stats_valid(stats);

    let mut out = String::with_capacity(256);
    out.push('{');
    write!(
        out,
        "\"result_links_removed_count\":{},\"targets_removed_count\":{},\"targets_freed_bytes\":{},\"targets_skipped_active_count\":{},\"targets_skipped_recent_count\":{},\"user_profiles_pruned_count\":{},\"errors\":[",
        stats.result_links_removed_count,
        stats.targets_removed_count,
        stats.targets_freed_bytes,
        stats.targets_skipped_active_count,
        stats.targets_skipped_recent_count,
        stats.user_profiles_pruned_count,
    )
    .expect("writing to String is infallible");

    for (index, item) in stats.errors.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_json_escaped_string(&mut out, item);
    }
    out.push_str("]}");

    assert!(
        out.len() <= SUBPROCESS_OUTPUT_BYTES_MAX,
        "encoded user phase JSON exceeded SUBPROCESS_OUTPUT_BYTES_MAX"
    );
    out
}

/// Deserialize and validate `UserPhaseStats` from the privilege-dropped child.
///
/// Structural/type mismatches return `Err`; semantic invariant violations trip
/// `assert_user_phase_stats_valid` (paired with `encode_user_phase_stats_json`).
pub fn parse_user_phase_stats_json(raw_json: &str) -> Result<UserPhaseStats, String> {
    assert!(
        raw_json.len() <= SUBPROCESS_OUTPUT_BYTES_MAX,
        "raw_json exceeded SUBPROCESS_OUTPUT_BYTES_MAX"
    );

    let mut cursor = JsonCursor::new(raw_json.as_bytes());
    cursor.skip_whitespace();
    cursor.expect_byte(b'{', "user phase payload must be a JSON object")?;

    let mut stats = UserPhaseStats::default();
    let mut seen_keys_mask: u8 = 0;
    const ALL_KEYS_MASK: u8 = (1 << 7) - 1;

    cursor.skip_whitespace();
    if cursor.peek() == Some(b'}') {
        cursor.advance();
    } else {
        // Bounded loop: exactly 7 expected keys; anything beyond is invalid.
        for field_index in 0..8 {
            if field_index >= 7 {
                return Err("user phase payload has too many keys".to_string());
            }
            cursor.skip_whitespace();
            let key = cursor.parse_string()?;
            cursor.skip_whitespace();
            cursor.expect_byte(b':', "expected ':' after object key")?;
            cursor.skip_whitespace();

            let bit_index: u8 = match key.as_str() {
                "result_links_removed_count" => {
                    stats.result_links_removed_count = cursor.parse_u64(&key)?;
                    0
                }
                "targets_removed_count" => {
                    stats.targets_removed_count = cursor.parse_u64(&key)?;
                    1
                }
                "targets_freed_bytes" => {
                    stats.targets_freed_bytes = cursor.parse_u64(&key)?;
                    2
                }
                "targets_skipped_active_count" => {
                    stats.targets_skipped_active_count = cursor.parse_u64(&key)?;
                    3
                }
                "targets_skipped_recent_count" => {
                    stats.targets_skipped_recent_count = cursor.parse_u64(&key)?;
                    4
                }
                "user_profiles_pruned_count" => {
                    stats.user_profiles_pruned_count = cursor.parse_u64(&key)?;
                    5
                }
                "errors" => {
                    stats.errors = cursor.parse_string_array()?;
                    6
                }
                _ => return Err(format!("unexpected user phase payload key: {key:?}")),
            };

            let bit = 1u8 << bit_index;
            if (seen_keys_mask & bit) != 0 {
                return Err(format!("duplicate user phase payload key: {key:?}"));
            }
            seen_keys_mask |= bit;

            cursor.skip_whitespace();
            match cursor.peek() {
                Some(b',') => cursor.advance(),
                Some(b'}') => {
                    cursor.advance();
                    break;
                }
                _ => return Err("expected ',' or '}' in user phase JSON object".to_string()),
            }
        }
    }

    cursor.skip_whitespace();
    if cursor.peek().is_some() {
        return Err("trailing data after user phase JSON object".to_string());
    }
    if seen_keys_mask != ALL_KEYS_MASK {
        return Err("missing required key(s) in user phase payload".to_string());
    }

    assert_user_phase_stats_valid(&stats);
    Ok(stats)
}

fn write_json_escaped_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                write!(out, "\\u{:04x}", u32::from(c)).expect("writing to String is infallible");
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct JsonCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> JsonCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        assert!(bytes.len() <= SUBPROCESS_OUTPUT_BYTES_MAX);
        Self { bytes, offset: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn advance(&mut self) {
        assert!(self.offset < self.bytes.len());
        self.offset += 1;
    }

    fn skip_whitespace(&mut self) {
        let remaining = self.bytes.len().saturating_sub(self.offset);
        for _ in 0..remaining {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.advance(),
                _ => break,
            }
        }
    }

    fn expect_byte(&mut self, expected: u8, message: &str) -> Result<(), String> {
        if self.peek() == Some(expected) {
            self.advance();
            Ok(())
        } else {
            Err(message.to_string())
        }
    }

    fn parse_u64(&mut self, field_name: &str) -> Result<u64, String> {
        let start = self.offset;
        let first = self
            .peek()
            .ok_or_else(|| format!("unexpected EOF parsing {field_name:?}"))?;
        if !first.is_ascii_digit() {
            return Err(format!("invalid integer field {field_name:?}"));
        }
        if first == b'0' {
            self.advance();
            if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                return Err(format!("leading zero in integer field {field_name:?}"));
            }
        } else {
            // Bounded: a u64 has at most 20 decimal digits.
            for _ in 0..21 {
                if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        // Reject floats (`.` or `e`/`E`) in integer fields.
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(format!("non-integer number in field {field_name:?}"));
        }

        let slice = self
            .bytes
            .get(start..self.offset)
            .ok_or_else(|| format!("slice error parsing {field_name:?}"))?;
        let text = std::str::from_utf8(slice)
            .map_err(|_| format!("invalid UTF-8 in field {field_name:?}"))?;
        text.parse::<u64>()
            .map_err(|_| format!("integer overflow in field {field_name:?}"))
    }

    fn parse_string_array(&mut self) -> Result<Vec<String>, String> {
        self.expect_byte(b'[', "invalid errors field in user phase payload")?;
        self.skip_whitespace();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.advance();
            return Ok(items);
        }

        for _ in 0..=ERRORS_MAX {
            self.skip_whitespace();
            if items.len() >= ERRORS_MAX {
                return Err("errors field exceeded ERRORS_MAX".to_string());
            }
            let item = self.parse_string()?;
            items.push(item);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.advance(),
                Some(b']') => {
                    self.advance();
                    assert!(items.len() <= ERRORS_MAX);
                    return Ok(items);
                }
                _ => return Err("expected ',' or ']' in errors array".to_string()),
            }
        }
        Err("errors field exceeded ERRORS_MAX".to_string())
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect_byte(b'"', "expected JSON string")?;
        let mut out = String::new();
        let max_steps = self.bytes.len().saturating_sub(self.offset);

        for _ in 0..max_steps {
            let byte = self
                .peek()
                .ok_or_else(|| "unterminated JSON string".to_string())?;
            if byte == b'"' {
                self.advance();
                return Ok(out);
            }
            if byte == b'\\' {
                self.advance();
                let esc = self
                    .peek()
                    .ok_or_else(|| "unterminated JSON escape".to_string())?;
                self.advance();
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{08}'),
                    b'f' => out.push('\u{0C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code_unit = self.parse_hex_u16()?;
                        let decoded = if (0xD800..=0xDBFF).contains(&code_unit) {
                            self.expect_byte(b'\\', "unpaired high surrogate in JSON string")?;
                            self.expect_byte(b'u', "unpaired high surrogate in JSON string")?;
                            let low_unit = self.parse_hex_u16()?;
                            if !(0xDC00..=0xDFFF).contains(&low_unit) {
                                return Err("invalid low surrogate in JSON string".to_string());
                            }
                            let combined = 0x10000
                                + ((u32::from(code_unit) - 0xD800) << 10)
                                + (u32::from(low_unit) - 0xDC00);
                            char::from_u32(combined)
                                .ok_or_else(|| "invalid Unicode scalar".to_string())?
                        } else {
                            char::from_u32(u32::from(code_unit))
                                .ok_or_else(|| "invalid surrogate in JSON string".to_string())?
                        };
                        out.push(decoded);
                    }
                    _ => return Err("invalid escape sequence in JSON string".to_string()),
                }
                continue;
            }
            if byte < 0x20 {
                return Err("unescaped control character in JSON string".to_string());
            }

            // Consume a full valid UTF-8 scalar value.
            let remaining = self
                .bytes
                .get(self.offset..)
                .ok_or_else(|| "slice error in JSON string".to_string())?;
            let text = std::str::from_utf8(remaining)
                .map_err(|_| "invalid UTF-8 in JSON string".to_string())?;
            let ch = text
                .chars()
                .next()
                .ok_or_else(|| "unterminated JSON string".to_string())?;
            out.push(ch);
            self.offset = self.offset.saturating_add(ch.len_utf8());
        }

        Err("unterminated JSON string".to_string())
    }

    fn parse_hex_u16(&mut self) -> Result<u16, String> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let digit = self
                .peek()
                .ok_or_else(|| "incomplete \\u escape in JSON string".to_string())?;
            self.advance();
            let nibble: u16 = match digit {
                b'0'..=b'9' => u16::from(digit - b'0'),
                b'a'..=b'f' => u16::from(digit - b'a') + 10,
                b'A'..=b'F' => u16::from(digit - b'A') + 10,
                _ => return Err("invalid hex digit in \\u escape".to_string()),
            };
            value = (value << 4) | nibble;
        }
        Ok(value)
    }
}
