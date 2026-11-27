//! The audit log (phase 14.5): security events, numbered.
//!
//! Every change to accounts, API keys, sessions and roles appends an event
//! in the transaction that makes it, so an event is there exactly when its
//! change is, once. What happens outside the database (a failed sign-in, a
//! refused request) is appended by the server with [`WriteTx::audit`].
//! Events name who acted (the account, and the key or session behind the
//! request), from where, on what, and the outcome; they never hold a secret.
//!
//! The log is replicated state (it travels in the replication log as
//! itself, and a member that applies an entry writes the events it carries,
//! deciding nothing), backed up with the database, and kept by count and
//! age ([`AuditRetention`]).

use std::sync::atomic::{AtomicU64, Ordering};

use grafeno_core::{Result, Timestamp};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::db::{ReadTx, WriteTx};
use crate::store::{META, decode, encode, storage};

/// `seq -> event`.
pub(crate) const AUDIT: TableDefinition<u64, &[u8]> = TableDefinition::new("audit");
/// The last sequence number given, in `meta`.
pub(crate) const AUDIT_SEQ: &str = "audit_seq";

/// How an event ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    #[default]
    Ok,
    /// Refused: a missing or wrong credential, or a missing privilege.
    Refused,
    /// Tried and failed (a wrong password, a lockout).
    Failed,
}

/// One security event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub seq: u64,
    pub at: Timestamp,
    /// What happened: `account_created`, `signin_failed`, `refused`, …
    pub kind: String,
    /// The account that acted, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The key or session behind the request (`key:name`, `session:id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Where the request came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// What it was done to: an account, a key, a role, a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub outcome: Outcome,
    /// More about it (never a secret or a hash).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

/// An event to append: [`WriteTx::audit`] numbers and dates it, and fills
/// in who acted from the transaction's actor when not given.
#[derive(Clone, Debug, Default)]
pub struct NewAuditEvent {
    pub kind: String,
    pub account: Option<String>,
    pub via: Option<String>,
    pub address: Option<String>,
    pub target: Option<String>,
    pub outcome: Outcome,
    pub detail: serde_json::Value,
}

impl NewAuditEvent {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            ..Self::default()
        }
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    pub fn detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }
}

/// How much of the audit log is kept: events beyond the newest
/// `max_entries` and older than `max_age_secs` go (0: no limit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditRetention {
    pub max_entries: u64,
    pub max_age_secs: u64,
}

impl Default for AuditRetention {
    /// Ten million events, a year.
    fn default() -> Self {
        Self {
            max_entries: 10_000_000,
            max_age_secs: 365 * 24 * 3600,
        }
    }
}

/// The retention, shared by the database's transactions.
#[derive(Debug)]
pub(crate) struct Retention {
    max_entries: AtomicU64,
    max_age_secs: AtomicU64,
}

impl Default for Retention {
    fn default() -> Self {
        let defaults = AuditRetention::default();
        Self {
            max_entries: AtomicU64::new(defaults.max_entries),
            max_age_secs: AtomicU64::new(defaults.max_age_secs),
        }
    }
}

impl Retention {
    pub(crate) fn get(&self) -> AuditRetention {
        AuditRetention {
            max_entries: self.max_entries.load(Ordering::Relaxed),
            max_age_secs: self.max_age_secs.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set(&self, retention: AuditRetention) {
        self.max_entries
            .store(retention.max_entries, Ordering::Relaxed);
        self.max_age_secs
            .store(retention.max_age_secs, Ordering::Relaxed);
    }
}

/// A page of the audit log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditPage {
    pub events: Vec<AuditEvent>,
    /// The last sequence number given (0 before any event).
    pub last: u64,
    /// The oldest kept (0 when the log is empty).
    pub oldest: u64,
    /// Events after `since` were pruned before they could be read.
    pub gap: bool,
}

fn page(
    table: &impl ReadableTable<u64, &'static [u8]>,
    last: u64,
    since: u64,
    limit: usize,
) -> Result<AuditPage> {
    let oldest = match table.first().map_err(storage)? {
        Some((key, _)) => key.value(),
        None => 0,
    };
    let mut events = Vec::new();
    if limit > 0 {
        for item in table.range(since.saturating_add(1)..).map_err(storage)? {
            let (_, value) = item.map_err(storage)?;
            events.push(decode::<AuditEvent>(value.value())?);
            if events.len() >= limit {
                break;
            }
        }
    }
    Ok(AuditPage {
        events,
        last,
        oldest,
        gap: oldest > 0 && since + 1 < oldest,
    })
}

impl ReadTx {
    /// Up to `limit` audit events after `since`, oldest first.
    pub fn audit(&self, since: u64, limit: usize) -> Result<AuditPage> {
        let table = self.tx.open_table(AUDIT).map_err(storage)?;
        let last = self
            .tx
            .open_table(META)
            .map_err(storage)?
            .get(AUDIT_SEQ)
            .map_err(storage)?
            .map_or(0, |guard| guard.value());
        page(&table, last, since, limit)
    }
}

impl WriteTx {
    /// Appends an audit event and returns its number. Who acted, the key or
    /// session and the address come from the transaction's origin (and
    /// actor) when the event does not say. Nothing is appended while a replicated entry is
    /// applied (its events come with it).
    pub fn audit(&mut self, event: NewAuditEvent) -> Result<u64> {
        if self.applying {
            return Ok(0);
        }
        let actor = self.actor.as_ref();
        let origin = self.origin.as_ref();
        let mut meta = self.tx.open_table(META).map_err(storage)?;
        let seq = meta
            .get(AUDIT_SEQ)
            .map_err(storage)?
            .map_or(0, |guard| guard.value())
            + 1;
        meta.insert(AUDIT_SEQ, seq).map_err(storage)?;
        drop(meta);
        let record = AuditEvent {
            seq,
            at: self.commit_at.unwrap_or_else(Timestamp::now),
            kind: event.kind,
            account: event
                .account
                .or_else(|| origin.and_then(|o| o.account.clone()))
                .or_else(|| actor.map(|a| a.name.clone())),
            via: event.via.or_else(|| origin.and_then(|o| o.via.clone())),
            address: event
                .address
                .or_else(|| origin.and_then(|o| o.address.clone())),
            target: event.target,
            outcome: event.outcome,
            detail: event.detail,
        };
        self.note(crate::replication::Touch::Audit(seq));
        self.tx
            .open_table(AUDIT)
            .map_err(storage)?
            .insert(
                seq,
                self.pack("", crate::compress::StoredKind::Audit, encode(&record)?)
                    .as_slice(),
            )
            .map_err(storage)?;
        self.prune_audit(seq, record.at)?;
        Ok(seq)
    }

    /// Drops what retention no longer keeps, at most a hundred events per
    /// append (an append never removes more than it can afford).
    fn prune_audit(&mut self, last: u64, now: Timestamp) -> Result<()> {
        let retention = self.shared.audit_retention.get();
        let mut gone = Vec::new();
        {
            let table = self.tx.open_table(AUDIT).map_err(storage)?;
            for item in table.iter().map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                let seq = key.value();
                let too_many = retention.max_entries > 0 && last - seq >= retention.max_entries;
                let too_old = retention.max_age_secs > 0 && {
                    let event: AuditEvent = decode(value.value())?;
                    now.as_micros() - event.at.as_micros()
                        > (retention.max_age_secs as i64).saturating_mul(1_000_000)
                };
                if !(too_many || too_old) || gone.len() >= 100 {
                    break;
                }
                gone.push(seq);
            }
        }
        if gone.is_empty() {
            return Ok(());
        }
        for &seq in &gone {
            self.note(crate::replication::Touch::Audit(seq));
        }
        let mut table = self.tx.open_table(AUDIT).map_err(storage)?;
        for seq in gone {
            table.remove(seq).map_err(storage)?;
        }
        Ok(())
    }

    /// Writes (or with `None` removes) a replicated audit event as it is,
    /// keeping the counter at the newest.
    pub(crate) fn apply_audit(&mut self, seq: u64, event: Option<&[u8]>) -> Result<()> {
        let mut table = self.tx.open_table(AUDIT).map_err(storage)?;
        match event {
            Some(bytes) => {
                table.insert(seq, bytes).map_err(storage)?;
                drop(table);
                let mut meta = self.tx.open_table(META).map_err(storage)?;
                let last = meta
                    .get(AUDIT_SEQ)
                    .map_err(storage)?
                    .map_or(0, |guard| guard.value());
                if seq > last {
                    meta.insert(AUDIT_SEQ, seq).map_err(storage)?;
                }
            }
            None => {
                table.remove(seq).map_err(storage)?;
            }
        }
        Ok(())
    }

    /// Appends an audit event for a change to access state (accounts, keys,
    /// sessions, roles), in the transaction that makes it.
    pub(crate) fn audit_access(
        &mut self,
        kind: &str,
        target: &str,
        detail: serde_json::Value,
    ) -> Result<()> {
        self.audit(NewAuditEvent::new(kind).target(target).detail(detail))
            .map(|_| ())
    }
}

impl crate::Grafeno {
    /// How much of the audit log is kept. Applies from the next append.
    pub fn set_audit_retention(&self, retention: AuditRetention) {
        self.shared.audit_retention.set(retention);
    }

    pub fn audit_retention(&self) -> AuditRetention {
        self.shared.audit_retention.get()
    }
}
