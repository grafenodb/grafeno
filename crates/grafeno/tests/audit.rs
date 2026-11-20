//! Phase 14.5: the audit log in the engine. Each change to accounts, keys,
//! sessions and roles appends one event in its own transaction (none when
//! it is rolled back), naming the origin of the write and never a secret;
//! events replicate exactly once, survive a reopen and a backup, and are
//! kept by count and age.

use grafeno::{
    Account, AccountKind, ApiKey, AuditRetention, Grafeno, NewAuditEvent, Origin, Outcome, RoleDef,
    Session, Timestamp,
};

fn account(name: &str) -> Account {
    Account {
        name: name.into(),
        kind: AccountKind::User,
        password_hash: Some("$argon2id$secret-hash".into()),
        grants: Vec::new(),
        roles: Vec::new(),
        superuser: false,
        own_only: false,
        disabled: false,
        expires_at: None,
        attributes: Default::default(),
        description: None,
        created_at: Timestamp::now(),
    }
}

fn kinds(db: &Grafeno) -> Vec<(String, Option<String>)> {
    db.begin_read()
        .unwrap()
        .audit(0, usize::MAX)
        .unwrap()
        .events
        .into_iter()
        .map(|e| (e.kind, e.target))
        .collect()
}

fn churn(db: &Grafeno) {
    db.write(|tx| {
        tx.set_origin(Some(Origin {
            account: Some("root".into()),
            via: Some("session:s0".into()),
            address: Some("10.0.0.1".into()),
        }));
        tx.create_account(&account("ana"))
    })
    .unwrap();
    db.write(|tx| {
        let mut ana = tx.account("ana")?.unwrap();
        ana.password_hash = Some("$argon2id$another-hash".into());
        ana.disabled = true;
        tx.put_account(&ana)?;
        tx.put_role(&RoleDef {
            name: "analyst".into(),
            privileges: Vec::new(),
            grants: Vec::new(),
            roles: Vec::new(),
            policies: Vec::new(),
            description: None,
            created_at: Timestamp::now(),
        })?;
        tx.create_api_key(&ApiKey {
            name: "ana-key".into(),
            account: "ana".into(),
            secret_hash: "first".into(),
            scope: None,
            expires_at: None,
            description: None,
            created_at: Timestamp::now(),
            rotated_at: None,
        })?;
        let mut key = tx.api_key("ana-key")?.unwrap();
        key.secret_hash = "second".into();
        tx.put_api_key(&key)?;
        let session = Session {
            id: "s1".into(),
            account: "ana".into(),
            token_hash: "t".into(),
            expires_at: Timestamp::from_secs(4_000_000_000),
            refresh_hash: "r".into(),
            refresh_expires_at: Timestamp::from_secs(4_000_000_000),
            replaced_refresh_hash: None,
            created_at: Timestamp::now(),
            refreshed_at: None,
            address: Some("10.0.0.2".into()),
        };
        tx.put_session(&session)?;
        tx.put_session(&session)?;
        tx.audit(
            NewAuditEvent::new("signin_failed")
                .target("bob")
                .outcome(Outcome::Failed),
        )?;
        Ok(())
    })
    .unwrap();
    db.write(|tx| tx.delete_account("ana").map(|_| ())).unwrap();
}

const EXPECTED: [(&str, &str); 11] = [
    ("account_created", "ana"),
    ("account_changed", "ana"),
    ("role_created", "analyst"),
    ("key_created", "ana-key"),
    ("key_rotated", "ana-key"),
    ("session_created", "ana"),
    ("session_refreshed", "ana"),
    ("signin_failed", "bob"),
    ("key_revoked", "ana-key"),
    ("session_ended", "ana"),
    ("account_deleted", "ana"),
];

#[test]
fn every_change_of_access_is_audited_once() {
    let db = Grafeno::in_memory().unwrap();
    churn(&db);
    let got = kinds(&db);
    let expected: Vec<(String, Option<String>)> = EXPECTED
        .iter()
        .map(|(k, t)| ((*k).to_owned(), Some((*t).to_owned())))
        .collect();
    assert_eq!(got, expected);
    let page = db.begin_read().unwrap().audit(0, usize::MAX).unwrap();
    // Numbered from one, in order; the origin is named; no secret is kept.
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (1..=11).collect::<Vec<u64>>()
    );
    assert_eq!(page.last, 11);
    let first = &page.events[0];
    assert_eq!(first.account.as_deref(), Some("root"));
    assert_eq!(first.via.as_deref(), Some("session:s0"));
    assert_eq!(first.address.as_deref(), Some("10.0.0.1"));
    assert_eq!(first.detail["has_password"], true);
    assert_eq!(page.events[1].detail["password"], "changed");
    assert_eq!(page.events[1].detail["disabled"], true);
    let text = serde_json::to_string(&page.events).unwrap();
    for secret in [
        "secret-hash",
        "another-hash",
        "first",
        "second",
        "\"t\"",
        "\"r\"",
    ] {
        assert!(!text.contains(secret), "{secret} in the audit log: {text}");
    }
    assert_eq!(page.events[7].outcome, Outcome::Failed);
    // A change rolled back leaves no event.
    let failed = db.write(|tx| {
        tx.create_account(&account("cleo"))?;
        Err::<(), _>(grafeno::Error::Invalid("changed my mind".into()))
    });
    assert!(failed.is_err());
    assert_eq!(
        db.begin_read().unwrap().audit(0, usize::MAX).unwrap().last,
        11
    );
}

#[test]
fn the_log_replicates_once_and_survives_reopening_and_backups() {
    let dir = std::env::temp_dir().join(format!("grafeno-audit-{}", grafeno::NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("leader.redb");
    {
        let leader = Grafeno::open(&path).unwrap();
        leader.enable_replication_log().unwrap();
        churn(&leader);
        // Replayed onto a member, the events are the leader's, once.
        let member = Grafeno::in_memory().unwrap();
        member.enable_replication_log().unwrap();
        let entries = leader
            .begin_read()
            .unwrap()
            .log_entries(0, usize::MAX)
            .unwrap();
        member.apply_entries(&entries).unwrap();
        let left = leader.begin_read().unwrap().audit(0, usize::MAX).unwrap();
        let right = member.begin_read().unwrap().audit(0, usize::MAX).unwrap();
        assert_eq!(left.events, right.events);
        assert_eq!(right.last, 11);
        // The member's own next event follows the leader's numbering.
        member
            .write(|tx| tx.audit(NewAuditEvent::new("refused")).map(|_| ()))
            .unwrap();
        assert_eq!(
            member.begin_read().unwrap().audit(11, 10).unwrap().events[0].seq,
            12
        );
        leader
            .begin_read()
            .unwrap()
            .backup(&dir.join("copy.redb"))
            .unwrap();
    }
    let reopened = Grafeno::open(&path).unwrap();
    assert_eq!(kinds(&reopened).len(), 11);
    let copy = Grafeno::open(dir.join("copy.redb")).unwrap();
    assert_eq!(kinds(&copy), kinds(&reopened));
    drop((reopened, copy));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retention_keeps_the_newest() {
    let db = Grafeno::in_memory().unwrap();
    db.set_audit_retention(AuditRetention {
        max_entries: 5,
        max_age_secs: 0,
    });
    for _ in 0..12 {
        db.write(|tx| tx.audit(NewAuditEvent::new("refused")).map(|_| ()))
            .unwrap();
    }
    let page = db.begin_read().unwrap().audit(0, usize::MAX).unwrap();
    assert_eq!(page.events.len(), 5);
    assert_eq!((page.oldest, page.last), (8, 12));
    assert!(page.gap, "events after 0 were pruned");
    assert!(!db.begin_read().unwrap().audit(7, 10).unwrap().gap);
}
