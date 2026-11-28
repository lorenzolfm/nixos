//! Paired JSON IPC encoding and decoding for `--_user-phase`.
//!
//! Zero external dependencies and zero recursion: every field is parsed directly
//! into validated Layer 3 types (`CleanupMode`, `RemovedBatch`, `ErrorMessage`,
//! `BoundedErrors`) so a corrupted unprivileged child cannot panic the root parent
//! or construct an inconsistent state.

use std::fmt::Write as _;

use crate::constants::{ERROR_MESSAGE_BYTES_MAX, ERRORS_MAX, SUBPROCESS_OUTPUT_BYTES_MAX};
use crate::types::{BoundedErrors, CleanupMode, ErrorMessage, RemovedBatch};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPhaseStats {
    pub mode: CleanupMode,
    pub result_links_removed_count: u64,
    pub targets_removed: RemovedBatch,
    pub targets_skipped_active_count: u64,
    pub targets_skipped_recent_count: u64,
    pub targets_skipped_unreadable_count: u64,
    pub user_profiles_pruned_count: u64,
    pub errors: BoundedErrors,
}

impl UserPhaseStats {
    pub fn new(mode: CleanupMode) -> Self {
        Self {
            mode,
            result_links_removed_count: 0,
            targets_removed: RemovedBatch::None,
            targets_skipped_active_count: 0,
            targets_skipped_recent_count: 0,
            targets_skipped_unreadable_count: 0,
            user_profiles_pruned_count: 0,
            errors: BoundedErrors::new(),
        }
    }
}

/// Serialize `UserPhaseStats` to a compact RFC 8259 JSON object.
pub fn encode_user_phase_stats_json(stats: &UserPhaseStats) -> String {
    let mut out = String::with_capacity(320);
    out.push('{');
    write!(
        out,
        "\"mode\":\"{}\",\"result_links_removed_count\":{},\"targets_removed_count\":{},\"targets_freed_bytes\":{},\"targets_skipped_active_count\":{},\"targets_skipped_recent_count\":{},\"targets_skipped_unreadable_count\":{},\"user_profiles_pruned_count\":{},\"errors_dropped_count\":{},\"errors\":[",
        stats.mode.as_ipc_str(),
        stats.result_links_removed_count,
        stats.targets_removed.count(),
        stats.targets_removed.freed_bytes(),
        stats.targets_skipped_active_count,
        stats.targets_skipped_recent_count,
        stats.targets_skipped_unreadable_count,
        stats.user_profiles_pruned_count,
        stats.errors.dropped_count(),
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
/// Structural mismatches, unknown/missing keys, mode mismatches, empty error
/// strings, and semantic invariant violations all return `Err` so a corrupted
/// unprivileged child cannot panic the root parent.
pub fn parse_user_phase_stats_json(
    raw_json: &str,
    expected_mode: CleanupMode,
) -> Result<UserPhaseStats, String> {
    if raw_json.len() > SUBPROCESS_OUTPUT_BYTES_MAX {
        return Err("user phase payload exceeded SUBPROCESS_OUTPUT_BYTES_MAX".to_string());
    }

    let mut cursor = JsonCursor::new(raw_json);
    cursor.skip_whitespace();
    cursor.expect_byte(b'{', "user phase payload must be a JSON object")?;

    let mut mode: Option<CleanupMode> = None;
    let mut result_links_removed_count: u64 = 0;
    let mut targets_removed_count: u64 = 0;
    let mut targets_freed_bytes: u64 = 0;
    let mut targets_skipped_active_count: u64 = 0;
    let mut targets_skipped_recent_count: u64 = 0;
    let mut targets_skipped_unreadable_count: u64 = 0;
    let mut user_profiles_pruned_count: u64 = 0;
    let mut errors_dropped_count: u64 = 0;
    let mut error_items: Vec<ErrorMessage> = Vec::new();

    const EXPECTED_KEYS_COUNT: usize = 10;
    const ALL_KEYS_MASK: u16 = (1 << EXPECTED_KEYS_COUNT) - 1;
    let mut seen_keys_mask: u16 = 0;

    cursor.skip_whitespace();
    if cursor.peek() == Some(b'}') {
        cursor.advance();
    } else {
        // Bounded loop: exactly 10 expected keys; anything beyond is invalid.
        for field_index in 0..=EXPECTED_KEYS_COUNT {
            if field_index >= EXPECTED_KEYS_COUNT {
                return Err("user phase payload has too many keys".to_string());
            }
            cursor.skip_whitespace();
            let key = cursor.parse_string()?;
            cursor.skip_whitespace();
            cursor.expect_byte(b':', "expected ':' after object key")?;
            cursor.skip_whitespace();

            let bit_index: u8 = match key.as_str() {
                "mode" => {
                    let raw_mode = cursor.parse_string()?;
                    mode = Some(CleanupMode::from_ipc_str(&raw_mode)?);
                    0
                }
                "result_links_removed_count" => {
                    result_links_removed_count = cursor.parse_u64(&key)?;
                    1
                }
                "targets_removed_count" => {
                    targets_removed_count = cursor.parse_u64(&key)?;
                    2
                }
                "targets_freed_bytes" => {
                    targets_freed_bytes = cursor.parse_u64(&key)?;
                    3
                }
                "targets_skipped_active_count" => {
                    targets_skipped_active_count = cursor.parse_u64(&key)?;
                    4
                }
                "targets_skipped_recent_count" => {
                    targets_skipped_recent_count = cursor.parse_u64(&key)?;
                    5
                }
                "targets_skipped_unreadable_count" => {
                    targets_skipped_unreadable_count = cursor.parse_u64(&key)?;
                    6
                }
                "user_profiles_pruned_count" => {
                    user_profiles_pruned_count = cursor.parse_u64(&key)?;
                    7
                }
                "errors_dropped_count" => {
                    errors_dropped_count = cursor.parse_u64(&key)?;
                    8
                }
                "errors" => {
                    error_items = cursor.parse_error_array()?;
                    9
                }
                _ => return Err(format!("unexpected user phase payload key: {key:?}")),
            };

            let bit = 1u16 << bit_index;
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

    let parsed_mode = mode.expect("mode key verified present by ALL_KEYS_MASK");
    if parsed_mode != expected_mode {
        return Err(format!(
            "user phase mode mismatch: expected {:?}, got {:?}",
            expected_mode.as_ipc_str(),
            parsed_mode.as_ipc_str(),
        ));
    }

    let targets_removed = RemovedBatch::from_parts(targets_removed_count, targets_freed_bytes)
        .map_err(|_| "targets_freed_bytes must be 0 when no targets were removed".to_string())?;
    let errors = BoundedErrors::from_parts(error_items, errors_dropped_count)?;

    Ok(UserPhaseStats {
        mode: parsed_mode,
        result_links_removed_count,
        targets_removed,
        targets_skipped_active_count,
        targets_skipped_recent_count,
        targets_skipped_unreadable_count,
        user_profiles_pruned_count,
        errors,
    })
}

fn write_json_escaped_string(out: &mut String, message: &ErrorMessage) {
    let text = message.as_str();
    let len_before = out.len();
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
    assert!(
        out.len() - len_before <= ERROR_MESSAGE_BYTES_MAX * 6 + 2,
        "escaped JSON string exceeded worst-case expansion bound"
    );
}

struct JsonCursor<'a> {
    text: &'a str,
    offset: usize,
}

impl<'a> JsonCursor<'a> {
    fn new(text: &'a str) -> Self {
        assert!(text.len() <= SUBPROCESS_OUTPUT_BYTES_MAX);
        Self { text, offset: 0 }
    }

    fn peek(&self) -> Option<u8> {
        assert!(self.offset <= self.text.len());
        self.text.as_bytes().get(self.offset).copied()
    }

    fn advance(&mut self) {
        assert!(self.offset < self.text.len());
        self.offset += 1;
    }

    fn skip_whitespace(&mut self) {
        assert!(self.offset <= self.text.len());
        let remaining = self.text.len().saturating_sub(self.offset);
        for _ in 0..remaining {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.advance(),
                _ => break,
            }
        }
        assert!(self.offset <= self.text.len());
    }

    fn expect_byte(&mut self, expected: u8, message: &str) -> Result<(), String> {
        assert!(!message.is_empty(), "error message must be non-empty");
        assert!(self.offset <= self.text.len());
        if self.peek() == Some(expected) {
            self.advance();
            Ok(())
        } else {
            Err(message.to_string())
        }
    }

    fn parse_u64(&mut self, field_name: &str) -> Result<u64, String> {
        assert!(!field_name.is_empty(), "field_name must be non-empty");
        assert!(self.offset <= self.text.len());
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

        let text = self
            .text
            .get(start..self.offset)
            .ok_or_else(|| format!("slice error parsing {field_name:?}"))?;
        text.parse::<u64>()
            .map_err(|_| format!("integer overflow in field {field_name:?}"))
    }

    fn parse_error_array(&mut self) -> Result<Vec<ErrorMessage>, String> {
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
            let raw_item = self.parse_string()?;
            let message = ErrorMessage::try_new(raw_item)?;
            items.push(message);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.advance(),
                Some(b']') => {
                    self.advance();
                    return Ok(items);
                }
                _ => return Err("expected ',' or ']' in errors array".to_string()),
            }
        }
        Err("errors field exceeded ERRORS_MAX".to_string())
    }

    fn parse_string(&mut self) -> Result<String, String> {
        assert!(self.offset <= self.text.len());
        self.expect_byte(b'"', "expected JSON string")?;
        let mut out = String::new();
        let max_steps = self.text.len().saturating_sub(self.offset);

        for _ in 0..max_steps {
            let byte = self
                .peek()
                .ok_or_else(|| "unterminated JSON string".to_string())?;
            if byte == b'"' {
                self.advance();
                assert!(out.len() <= SUBPROCESS_OUTPUT_BYTES_MAX);
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
                            if (0xDC00..=0xDFFF).contains(&low_unit) {
                                let combined = 0x10000
                                    + ((u32::from(code_unit) - 0xD800) << 10)
                                    + (u32::from(low_unit) - 0xDC00);
                                char::from_u32(combined)
                                    .ok_or_else(|| "invalid Unicode scalar".to_string())?
                            } else {
                                return Err("invalid low surrogate in JSON string".to_string());
                            }
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

            if byte < 0x80 {
                out.push(char::from(byte));
                self.advance();
            } else {
                // `self.text` is already validated UTF-8 and `self.offset` always lands
                // on a scalar boundary, so decoding the next char is O(1).
                let tail = self
                    .text
                    .get(self.offset..)
                    .ok_or_else(|| "slice error in JSON string".to_string())?;
                let ch = tail
                    .chars()
                    .next()
                    .ok_or_else(|| "unterminated JSON string".to_string())?;
                out.push(ch);
                self.offset = self.offset.saturating_add(ch.len_utf8());
            }
        }

        Err("unterminated JSON string".to_string())
    }

    fn parse_hex_u16(&mut self) -> Result<u16, String> {
        assert!(self.offset <= self.text.len());
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
        assert!(self.offset <= self.text.len());
        Ok(value)
    }
}
