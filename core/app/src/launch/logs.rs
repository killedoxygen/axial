//! Session-local bounded output ingestion. Raw bytes never enter history or public events.

fn log_text_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("appdata")
        || lower.contains("/private/")
        || lower.contains("://")
        || lower.contains("setting user:")
        || lower.contains("uuid of player")
        || lower.contains("/home/")
        || lower.contains("/users/")
        || lower.contains("/var/")
        || lower.contains("/tmp/")
        || lower.contains("/opt/")
        || lower.contains("/usr/")
        || lower.contains("/etc/")
        || lower.contains("/library/")
        || lower.contains("/applications/")
        || lower.contains("/mnt/")
        || lower.contains("/volumes/")
        || lower.contains("~/")
        || lower.contains("\\users\\")
        || lower.contains("\\appdata\\")
        || contains_windows_drive_path(value)
    {
        return true;
    }
    if lower.contains(".minecraft")
        || lower.contains(".jar")
        || lower.contains(".exe")
        || lower.contains(".dll")
        || lower.contains(".dylib")
        || lower.contains(".so")
    {
        return true;
    }
    if lower.contains("-xmx")
        || lower.contains("-xms")
        || lower.contains("-xx:")
        || lower.starts_with("-d")
        || lower.contains(" -d")
        || lower.contains("--access")
        || lower.contains("--username")
        || lower.contains("--uuid")
        || lower.contains("--xuid")
        || lower.contains("--user_properties")
        || lower.contains("--")
        || lower.contains("-x")
        || lower.contains("--classpath")
        || lower.contains(" -cp ")
        || lower.contains(" -classpath ")
    {
        return true;
    }
    if lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("provider_payload")
        || lower.contains("account_id")
        || lower.contains("username=")
        || lower.contains("xuid=")
        || lower.contains("authorization")
        || lower.contains("credential")
        || lower.contains("bearer ")
    {
        return true;
    }
    if value.contains('@') && value.contains('.') {
        return true;
    }

    looks_like_jwt(value) || has_long_secret_like_run(value)
}

fn contains_windows_drive_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.windows(3).any(|window| {
        window[0].is_ascii_alphabetic() && window[1] == b':' && matches!(window[2], b'\\' | b'/')
    })
}

fn looks_like_jwt(value: &str) -> bool {
    value.split_whitespace().any(|token| {
        let token = token.trim_matches(|value: char| {
            !(value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'))
        });
        let parts = token.split('.').collect::<Vec<_>>();
        parts.len() >= 3
            && parts.iter().take(3).all(|part| {
                part.len() >= 12
                    && part
                        .chars()
                        .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
            })
    })
}

fn has_long_secret_like_run(value: &str) -> bool {
    value
        .split(|value: char| !(value.is_ascii_alphanumeric() || matches!(value, '-' | '_')))
        .any(|part| {
            part.len() >= 48
                && part.chars().any(|value| value.is_ascii_alphabetic())
                && part.chars().any(|value| value.is_ascii_digit())
        })
}

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::outcome::LaunchEvidence;

pub const MAX_RAW_LINE_BYTES: usize = 16 * 1024;
pub const MAX_LOG_LINE_CHARS: usize = 1_000;
pub const MAX_LOG_ENTRIES: usize = 2_000;
pub const MAX_LOG_HISTORY_BYTES: usize = 512 * 1024;
pub const MAX_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
pub const REDACTED_LINE: &str = "[Private launch output redacted]";
pub const OVERSIZED_LINE: &str = "[Oversized launch output omitted]";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogEntry {
    pub sequence: u64,
    pub source: LogStream,
    pub text: String,
    pub truncated: bool,
}

/// No Debug/Serialize implementation: exact credentials remain private to the session.
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new(secrets: Vec<String>) -> Self {
        Self {
            secrets: secrets
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect(),
        }
    }

    pub fn contains_secret(&self, value: &str) -> bool {
        self.secrets.iter().any(|secret| value.contains(secret))
    }

    /// Evaluate the complete admitted line before truncating anything.
    pub fn redact_line(&self, value: &str) -> String {
        if value.len() > MAX_RAW_LINE_BYTES {
            return OVERSIZED_LINE.to_owned();
        }
        if self.contains_secret(value)
            || value
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\r' | '\t'))
            || log_text_looks_sensitive(value)
        {
            return REDACTED_LINE.to_owned();
        }
        value
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_LOG_LINE_CHARS)
            .collect()
    }

    pub fn redact_bytes(&self, value: &[u8]) -> String {
        match std::str::from_utf8(value) {
            Ok(value) => self.redact_line(value),
            Err(_) => REDACTED_LINE.to_owned(),
        }
    }
}

#[derive(Default)]
struct PendingLine {
    bytes: Vec<u8>,
    discarding: bool,
    finished: bool,
}

/// Create one collector per accepted session incarnation. The session actor fences
/// reader events before calling it, and never reuses a collector for a later attempt.
pub struct LogCollector {
    redactor: Redactor,
    stdout: PendingLine,
    stderr: PendingLine,
    history: VecDeque<LogEntry>,
    history_bytes: usize,
    next_sequence: u64,
    dropped_entries: u64,
    evidence: LaunchEvidence,
}

impl LogCollector {
    pub fn new(secrets: Vec<String>) -> Self {
        Self {
            redactor: Redactor::new(secrets),
            stdout: PendingLine::default(),
            stderr: PendingLine::default(),
            history: VecDeque::new(),
            history_bytes: 0,
            next_sequence: 1,
            dropped_entries: 0,
            evidence: LaunchEvidence::default(),
        }
    }

    /// The process reader supplies fixed, bounded chunks. Oversized caller input is
    /// discarded in full and produces one marker, preventing unbounded return vectors.
    pub fn push(&mut self, stream: LogStream, bytes: &[u8]) -> Vec<LogEntry> {
        if self.pending(stream).finished {
            return Vec::new();
        }
        if bytes.len() > MAX_OUTPUT_CHUNK_BYTES {
            let pending = self.pending(stream);
            pending.bytes.clear();
            pending.discarding = !bytes.ends_with(b"\n");
            return vec![self.publish(stream, OVERSIZED_LINE.to_owned(), true)];
        }
        let mut emitted = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                if let Some(entry) = self.complete_line(stream) {
                    emitted.push(entry);
                }
                continue;
            }
            let pending = self.pending(stream);
            if pending.discarding {
                continue;
            }
            if pending.bytes.len() == MAX_RAW_LINE_BYTES {
                let prefix = std::mem::take(&mut pending.bytes);
                pending.discarding = true;
                if let Ok(text) = std::str::from_utf8(&prefix) {
                    self.evidence.observe_line(text);
                }
                emitted.push(self.publish(stream, OVERSIZED_LINE.to_owned(), true));
            } else {
                pending.bytes.push(byte);
            }
        }
        emitted
    }

    /// Flush only after actual EOF. Calling this on a read error would manufacture
    /// complete output drainage; the session owner instead retains unresolved state.
    pub fn finish_stream(&mut self, stream: LogStream) -> Vec<LogEntry> {
        if self.pending(stream).finished {
            return Vec::new();
        }
        let entry = self.complete_line(stream);
        self.pending(stream).finished = true;
        entry.into_iter().collect()
    }

    pub fn outputs_drained(&self) -> bool {
        self.stdout.finished && self.stderr.finished
    }

    /// Drop credentials after both streams have reached EOF. Until then late
    /// output still needs the complete session redaction context.
    pub fn clear_secrets(&mut self) -> bool {
        if !self.outputs_drained() {
            return false;
        }
        self.redactor.secrets.clear();
        true
    }

    pub fn entries(&self) -> Vec<LogEntry> {
        self.history.iter().cloned().collect()
    }

    pub(super) fn has_entries(&self) -> bool {
        !self.history.is_empty() || self.dropped_entries != 0
    }

    pub fn evidence(&self) -> &LaunchEvidence {
        &self.evidence
    }

    pub fn dropped_entries(&self) -> u64 {
        self.dropped_entries
    }

    fn pending(&mut self, stream: LogStream) -> &mut PendingLine {
        match stream {
            LogStream::Stdout => &mut self.stdout,
            LogStream::Stderr => &mut self.stderr,
        }
    }

    fn complete_line(&mut self, stream: LogStream) -> Option<LogEntry> {
        let pending = self.pending(stream);
        if pending.discarding {
            pending.discarding = false;
            pending.bytes.clear();
            return None;
        }
        if pending.bytes.is_empty() {
            return None;
        }
        let bytes = std::mem::take(&mut pending.bytes);
        if let Ok(text) = std::str::from_utf8(&bytes) {
            self.evidence.observe_line(text);
        }
        let text = self.redactor.redact_bytes(&bytes);
        let truncated =
            std::str::from_utf8(&bytes).is_ok_and(|raw| raw.chars().count() > MAX_LOG_LINE_CHARS);
        Some(self.publish(stream, text, truncated))
    }

    fn publish(&mut self, source: LogStream, text: String, truncated: bool) -> LogEntry {
        let entry = LogEntry {
            sequence: self.next_sequence,
            source,
            text,
            truncated,
        };
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("launch log sequence exhausted");
        self.history_bytes += entry.text.len();
        self.history.push_back(entry.clone());
        while self.history.len() > MAX_LOG_ENTRIES || self.history_bytes > MAX_LOG_HISTORY_BYTES {
            if let Some(removed) = self.history.pop_front() {
                self.history_bytes -= removed.text.len();
                self.dropped_entries += 1;
            }
        }
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::super::outcome::FailureClass;
    use super::*;

    #[test]
    fn credentials_crossing_every_chunk_boundary_are_never_published() {
        let raw = b"Before synthetic-value-12345 after\n";
        for split in 0..raw.len() {
            let mut collector = LogCollector::new(vec!["synthetic-value-12345".into()]);
            assert!(collector.push(LogStream::Stdout, &raw[..split]).is_empty());
            let events = collector.push(LogStream::Stdout, &raw[split..]);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].text, REDACTED_LINE);
            assert!(
                !serde_json::to_string(&collector.entries())
                    .unwrap()
                    .contains("synthetic-value")
            );
        }
    }

    #[test]
    fn privacy_checks_precede_display_truncation() {
        let mut collector = LogCollector::new(Vec::new());
        let line = format!("{} accessToken=credential\n", "visible ".repeat(200));
        let events = collector.push(LogStream::Stderr, line.as_bytes());
        assert_eq!(events[0].text, REDACTED_LINE);
        assert!(events[0].truncated);
    }

    #[test]
    fn oversized_non_newline_output_is_bounded_and_recovers_at_the_next_line() {
        let mut collector = LogCollector::new(Vec::new());
        let mut events = Vec::new();
        for _ in 0..256 {
            events.extend(collector.push(LogStream::Stdout, &[b'x'; MAX_OUTPUT_CHUNK_BYTES]));
            assert!(collector.stdout.bytes.len() <= MAX_RAW_LINE_BYTES);
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, OVERSIZED_LINE);
        let events = collector.push(LogStream::Stdout, b"\nordinary game output\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "ordinary game output");
    }

    #[test]
    fn stream_eof_is_independent_and_a_closed_stream_cannot_accept_late_output() {
        let mut collector = LogCollector::new(Vec::new());
        collector.push(LogStream::Stdout, b"stdout tail");
        collector.push(LogStream::Stderr, b"stderr tail");
        assert_eq!(
            collector.finish_stream(LogStream::Stdout)[0].text,
            "stdout tail"
        );
        assert!(!collector.outputs_drained());
        assert!(
            collector
                .push(LogStream::Stdout, b"stale attempt\n")
                .is_empty()
        );
        assert_eq!(
            collector.finish_stream(LogStream::Stderr)[0].text,
            "stderr tail"
        );
        assert!(collector.outputs_drained());
        assert!(collector.finish_stream(LogStream::Stderr).is_empty());
    }

    #[test]
    fn logs_retain_classification_when_sensitive_output_is_hidden() {
        let mut collector = LogCollector::new(Vec::new());
        let events = collector.push(
            LogStream::Stderr,
            b"Unrecognized VM option in /home/alice/game\n",
        );
        assert_eq!(events[0].text, REDACTED_LINE);
        assert_eq!(
            collector.evidence().failure_classes,
            vec![FailureClass::JvmUnsupportedOption]
        );
        let events = collector.push(
            LogStream::Stdout,
            b"[Render thread/INFO]: LWJGL Version: 3.3.3\n",
        );
        assert!(collector.evidence().boot_observed);
        assert!(events[0].text.contains("LWJGL"));
    }

    #[test]
    fn invalid_utf8_and_terminal_controls_are_not_partially_exposed() {
        let mut collector = LogCollector::new(Vec::new());
        assert_eq!(
            collector.push(LogStream::Stdout, b"private\xffvalue\n")[0].text,
            REDACTED_LINE
        );
        assert_eq!(
            collector.push(LogStream::Stdout, b"\x1b[31mprivate\n")[0].text,
            REDACTED_LINE
        );
        assert_eq!(
            collector.push(LogStream::Stdout, b"ordinary\r\n")[0].text,
            "ordinary"
        );
    }

    #[test]
    fn history_is_bounded_and_sequence_cursors_expose_eviction() {
        let mut collector = LogCollector::new(Vec::new());
        for _ in 0..(MAX_LOG_ENTRIES + 10) {
            collector.push(LogStream::Stdout, b"hello\n");
        }
        assert_eq!(collector.entries().len(), MAX_LOG_ENTRIES);
        assert_eq!(collector.entries()[0].sequence, 11);
        assert_eq!(collector.dropped_entries(), 10);
        let line = format!("{}\n", "text ".repeat(200));
        for _ in 0..MAX_LOG_ENTRIES {
            collector.push(LogStream::Stderr, line.as_bytes());
        }
        assert!(
            collector
                .entries()
                .iter()
                .map(|entry| entry.text.len())
                .sum::<usize>()
                <= MAX_LOG_HISTORY_BYTES
        );
    }
}
