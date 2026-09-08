//! Audit records (plan Phase 10, Task 16).
//!
//! Every permission-relevant event lands here: approvals, denials, blocked
//! attempts, executions. Records carry metadata (who, what, where, outcome)
//! — never file contents or secrets (see [`redact_detail`]). SQLite
//! persistence arrives in Phase 32; this crate defines the record shape,
//! the sink interface, and an in-memory sink for tests.

use capability_core::{Capability, Principal, Resource};
use serde::{Deserialize, Serialize};

/// What happened, as the audit log sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuditOutcome {
    /// Policy allowed (ticket minted).
    Authorized,
    /// Policy denied outright.
    Denied,
    /// Execution blocked: no ticket, expired ticket, out-of-scope, …
    Blocked,
    /// Stopped for user approval.
    ApprovalRequested,
    /// User approved (grant recorded).
    Approved,
    /// User denied.
    ApprovalDenied,
    /// Tool executed.
    Executed,
    /// Tool failed.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Unix millis. The store assigns sequence numbers.
    pub timestamp_ms: u64,
    pub principal: Principal,
    pub capability: Option<Capability>,
    pub resource: Option<Resource>,
    pub outcome: AuditOutcome,
    /// Short human reason. Redacted — see [`redact_detail`].
    pub detail: String,
    /// Execution wall-clock for completed tool calls, when measured.
    /// Absent for policy-only records (requests, decisions without run).
    pub duration_ms: Option<u64>,
    /// Mutation evidence for file writes (plan Phase 10): path plus
    /// before/after sha256. Hashes, never contents — the log stays a
    /// witness, not a backup.
    pub mutation: Option<tool_core::MutationEvidence>,
}

impl AuditRecord {
    pub fn now(
        principal: Principal,
        capability: Option<Capability>,
        resource: Option<Resource>,
        outcome: AuditOutcome,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            timestamp_ms: unix_millis(),
            principal,
            capability,
            resource,
            outcome,
            detail: redact_detail(&detail.into()),
            duration_ms: None,
            mutation: None,
        }
    }

    pub fn with_duration(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    pub fn with_mutation(mut self, evidence: tool_core::MutationEvidence) -> Self {
        self.mutation = Some(evidence);
        self
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Redact free-text detail before it touches the log: truncate length and
/// mask plausible secret tokens (`sk-…`, `AKIA…`, `ghp_…`, `xox…`,
/// `-----BEGIN … PRIVATE KEY-----` blocks). Audit must never become a
/// secret store.
pub fn redact_detail(detail: &str) -> String {
    const MAX_LEN: usize = 512;
    let mut out = String::with_capacity(detail.len().min(MAX_LEN));
    let bytes = detail.as_bytes();
    let mut i = 0;
    while i < bytes.len() && out.len() < MAX_LEN {
        let rest = &detail[i..];
        let token_len = secret_prefix_len(rest);
        if token_len > 0 {
            out.push_str("[redacted]");
            i += token_len;
            // Consume the token body: base64-ish run after the prefix.
            while i < bytes.len() && is_token_char(bytes[i]) && out.len() < MAX_LEN {
                i += 1;
            }
        } else if rest.starts_with("-----BEGIN ") {
            out.push_str("[redacted-private-key]");
            if let Some(end) = rest.find("-----END ") {
                let tail = &rest[end..];
                i += if let Some(nl) = tail.find('\n') {
                    end + nl + 1
                } else {
                    rest.len()
                };
            } else {
                break;
            }
        } else {
            // Push one char (UTF-8 safe).
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                out.push(c);
                i += c.len_utf8();
            } else {
                break;
            }
        }
    }
    if i < bytes.len() {
        out.push_str("…[truncated]");
    }
    out
}

fn secret_prefix_len(rest: &str) -> usize {
    for prefix in ["sk-", "AKIA", "ghp_", "gho_", "xoxb-", "xoxp-", "xapp-"] {
        if rest.starts_with(prefix) {
            return prefix.len();
        }
    }
    0
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'~' | b'/')
}

/// Where records go. SQLite implements this in Phase 32.
pub trait AuditSink: Send + Sync {
    fn record(&self, record: AuditRecord);
}

/// Test/development sink: appends to a mutex-guarded vec.
/// Bounded ring: the oldest records drop past capacity, so a long session
/// cannot grow memory without limit. (Durable audit storage is Phase 32
/// follow-up work.)
#[derive(Debug)]
pub struct InMemorySink {
    records: std::sync::Mutex<Vec<AuditRecord>>,
    capacity: usize,
}

/// Default ring capacity for the shared host sink.
pub const DEFAULT_SINK_CAPACITY: usize = 1000;

impl Default for InMemorySink {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_SINK_CAPACITY)
    }
}

impl InMemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            records: std::sync::Mutex::new(Vec::new()),
            capacity: capacity.max(1),
        }
    }

    pub fn records(&self) -> Vec<AuditRecord> {
        self.records.lock().expect("audit lock").clone()
    }

    pub fn len(&self) -> usize {
        self.records.lock().expect("audit lock").len()
    }

    pub fn outcomes(&self) -> Vec<AuditOutcome> {
        self.records().iter().map(|r| r.outcome).collect()
    }
}

impl AuditSink for InMemorySink {
    fn record(&self, record: AuditRecord) {
        let mut records = self.records.lock().expect("audit lock");
        if records.len() >= self.capacity {
            let drop_count = records.len() - self.capacity + 1;
            records.drain(..drop_count);
        }
        records.push(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal() -> Principal {
        Principal::User
    }

    #[test]
    fn approve_and_deny_records_round_trip() {
        let sink = InMemorySink::new();
        sink.record(AuditRecord::now(
            principal(),
            Some(Capability::FilesystemWrite),
            Some(Resource::Path("/work/f".into())),
            AuditOutcome::Approved,
            "session grant",
        ));
        sink.record(AuditRecord::now(
            principal(),
            Some(Capability::ProcessSpawn),
            Some(Resource::Executable("/bin/x".into())),
            AuditOutcome::ApprovalDenied,
            "user said no",
        ));
        let records = sink.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].outcome, AuditOutcome::Approved);
        assert_eq!(records[1].outcome, AuditOutcome::ApprovalDenied);
        assert!(records[0].timestamp_ms > 0);
    }

    #[test]
    fn ring_drops_oldest_past_capacity() {
        let sink = InMemorySink::with_capacity(3);
        for i in 0..5 {
            sink.record(AuditRecord::now(
                principal(),
                None,
                None,
                AuditOutcome::Executed,
                format!("step-{i}"),
            ));
        }
        assert_eq!(sink.len(), 3);
        let records = sink.records();
        let details: Vec<&str> = records.iter().map(|r| r.detail.as_str()).collect();
        assert_eq!(details, vec!["step-2", "step-3", "step-4"]);
    }

    #[test]
    fn durations_attach_to_records() {
        let record = AuditRecord::now(principal(), None, None, AuditOutcome::Executed, "ok")
            .with_duration(42);
        assert_eq!(record.duration_ms, Some(42));
        let plain = AuditRecord::now(principal(), None, None, AuditOutcome::Denied, "no");
        assert_eq!(plain.duration_ms, None);
    }

    #[test]
    fn blocked_read_and_write_records() {
        let sink = InMemorySink::new();
        sink.record(AuditRecord::now(
            principal(),
            Some(Capability::FilesystemRead),
            Some(Resource::Path("/etc/passwd".into())),
            AuditOutcome::Blocked,
            "outside granted scope",
        ));
        sink.record(AuditRecord::now(
            principal(),
            Some(Capability::ProcessSpawn),
            None,
            AuditOutcome::Denied,
            "no grant",
        ));
        assert_eq!(
            sink.outcomes(),
            vec![AuditOutcome::Blocked, AuditOutcome::Denied]
        );
    }

    #[test]
    fn secrets_are_masked_and_long_text_truncated() {
        let redacted = redact_detail("key=sk-abcDEF123 rest");
        assert!(!redacted.contains("abcDEF123"), "{redacted}");
        assert!(redacted.contains("[redacted]"), "{redacted}");

        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIB\n-----END RSA PRIVATE KEY-----\ndone";
        let redacted = redact_detail(pem);
        assert!(!redacted.contains("MIIB"), "{redacted}");
        assert!(redacted.contains("[redacted-private-key]"), "{redacted}");
        assert!(redacted.contains("done"), "{redacted}");

        let long = "x".repeat(2000);
        let redacted = redact_detail(&long);
        assert!(redacted.ends_with("…[truncated]"), "{redacted}");
        assert!(redacted.len() < 2000);

        // Ordinary text passes through untouched.
        assert_eq!(redact_detail("read /work/main.rs"), "read /work/main.rs");
    }
}
