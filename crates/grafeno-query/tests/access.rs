//! Phase 14.3: access statements run by callers who may not manage
//! everything. Managing accounts and managing roles are separate; only
//! superusers make, change or drop superusers; passwords go through the
//! hasher the caller gives; a suspended account loses its sessions; the
//! privilege catalog; and what `requirements` says an `EXPLAIN` lacks.

use std::sync::Arc;

use grafeno::{Grafeno, Session, Timestamp};
use grafeno_query::{
    AccessContext, Limits, Params, PasswordHasher, StatementKind, Value, execute, execute_with,
    requirements,
};

fn context(manage_accounts: bool, manage_roles: bool) -> AccessContext {
    let hasher: PasswordHasher = Arc::new(|name: &str, password: &str| {
        if password.contains(name) {
            Err("the password contains the account's name".into())
        } else {
            Ok(format!("hash:{password}"))
        }
    });
    AccessContext {
        account: Some("carol".into()),
        superuser: false,
        manage_accounts,
        manage_roles,
        read_audit: false,
        effective_roles: vec!["people".into()],
        hash_password: Some(hasher),
    }
}

fn run_as(db: &Grafeno, context: &AccessContext, query: &str) -> Result<Vec<Vec<Value>>, String> {
    let limits = Limits {
        access: Some(context.clone()),
        ..Limits::none()
    };
    execute_with(db, "x", query, &Params::new(), &limits)
        .map(|result| result.rows)
        .map_err(|error| {
            if error.forbidden {
                format!("forbidden: {error}")
            } else {
                error.to_string()
            }
        })
}

fn owner(db: &Grafeno, query: &str) {
    execute(db, "x", query, &Params::new()).unwrap_or_else(|e| panic!("{query}: {e}"));
}

#[test]
fn managing_accounts_and_roles_are_separate() {
    let db = Grafeno::in_memory().unwrap();
    owner(&db, "CREATE SERVICE ACCOUNT carol");
    owner(&db, "CREATE SERVICE ACCOUNT root SET SUPERUSER true");
    owner(&db, "CREATE ROLE analyst");

    let roles_only = context(false, true);
    let refused = run_as(&db, &roles_only, "CREATE SERVICE ACCOUNT bot").unwrap_err();
    assert!(
        refused.starts_with("forbidden") && refused.contains("`manage_accounts`"),
        "{refused}"
    );
    assert!(
        run_as(&db, &roles_only, "SHOW USERS")
            .unwrap_err()
            .starts_with("forbidden")
    );
    run_as(&db, &roles_only, "GRANT read ON NAMESPACE crm TO analyst").unwrap();
    run_as(&db, &roles_only, "CREATE ROLE helper").unwrap();
    run_as(&db, &roles_only, "GRANT ROLE helper TO analyst").unwrap();
    // A role to an account is managing accounts.
    let refused = run_as(&db, &roles_only, "GRANT ROLE analyst TO carol").unwrap_err();
    assert!(refused.contains("`manage_accounts`"), "{refused}");

    let accounts_only = context(true, false);
    assert!(
        run_as(&db, &accounts_only, "CREATE ROLE x")
            .unwrap_err()
            .contains("`manage_roles`")
    );
    assert!(run_as(&db, &accounts_only, "DENY read ON SERVER TO analyst").is_err());
    run_as(&db, &accounts_only, "GRANT ROLE analyst TO carol").unwrap();
    run_as(
        &db,
        &accounts_only,
        "CREATE USER dave SET PASSWORD 'a long passphrase'",
    )
    .unwrap();
    assert_eq!(
        db.begin_read()
            .unwrap()
            .account("dave")
            .unwrap()
            .unwrap()
            .password_hash
            .as_deref(),
        Some("hash:a long passphrase")
    );
    // The policy speaks through the hasher.
    let refused = run_as(
        &db,
        &accounts_only,
        "CREATE USER erin SET PASSWORD 'erin is here'",
    )
    .unwrap_err();
    assert!(refused.contains("contains the account's name"), "{refused}");
    // Superusers are only a superuser's.
    for query in [
        "CREATE SERVICE ACCOUNT boss SET SUPERUSER true",
        "ALTER USER root SET STATUS SUSPENDED",
        "DROP USER root",
        "GRANT ROLE analyst TO root",
        "ALTER USER dave SET SUPERUSER true",
    ] {
        let refused = run_as(&db, &accounts_only, query).unwrap_err();
        assert!(refused.contains("only superusers"), "{query}: {refused}");
    }
    let superuser = AccessContext {
        superuser: true,
        ..context(true, true)
    };
    run_as(&db, &superuser, "ALTER USER dave SET SUPERUSER true").unwrap();

    // Anyone lists their own privileges and the catalog, and sees who they are.
    let nobody = context(false, false);
    let mine = run_as(&db, &nobody, "SHOW PRIVILEGES FOR carol").unwrap();
    assert!(
        mine.iter()
            .any(|row| row[0] == Value::Str("analyst".into())),
        "{mine:?}"
    );
    assert!(
        run_as(&db, &nobody, "SHOW PRIVILEGES FOR analyst")
            .unwrap_err()
            .starts_with("forbidden")
    );
    let catalog = run_as(&db, &nobody, "SHOW PRIVILEGES").unwrap();
    assert!(catalog.contains(&vec![
        Value::Str("manage_roles".into()),
        Value::Str("server".into())
    ]));
    assert!(catalog.contains(&vec![
        Value::Str("write".into()),
        Value::Str("property".into())
    ]));
    let me = run_as(&db, &nobody, "SHOW CURRENT USER").unwrap();
    assert_eq!(me[0][0], Value::Str("carol".into()));
    assert_eq!(me[0][3], Value::List(vec![Value::Str("analyst".into())]));
}

#[test]
fn suspending_ends_sessions_and_explain_names_what_is_missing() {
    let db = Grafeno::in_memory().unwrap();
    owner(&db, "CREATE SERVICE ACCOUNT bot");
    db.write(|tx| {
        tx.put_session(&Session {
            id: "s1".into(),
            account: "bot".into(),
            token_hash: "t".into(),
            expires_at: Timestamp::from_secs(4_000_000_000),
            refresh_hash: "r".into(),
            refresh_expires_at: Timestamp::from_secs(4_000_000_000),
            replaced_refresh_hash: None,
            created_at: Timestamp::now(),
            refreshed_at: None,
            address: None,
        })
    })
    .unwrap();
    owner(&db, "ALTER USER bot SET DESCRIPTION 'still here'");
    assert!(db.begin_read().unwrap().session("s1").unwrap().is_some());
    owner(
        &db,
        "ALTER USER bot SET STATUS SUSPENDED SET EXPIRES 1700000000000000",
    );
    assert!(db.begin_read().unwrap().session("s1").unwrap().is_none());
    let bot = db.begin_read().unwrap().account("bot").unwrap().unwrap();
    assert!(bot.disabled && bot.expired(Timestamp::now()));

    let nobody = context(false, false);
    let needs = requirements("EXPLAIN DROP USER bot", Some(&nobody)).unwrap();
    assert!(needs.explain);
    assert_eq!(needs.kind, StatementKind::Access);
    assert_eq!(needs.lacking, Some("manage_accounts"));
    assert_eq!(
        requirements("GRANT read ON SERVER TO x", Some(&nobody))
            .unwrap()
            .lacking,
        Some("manage_roles")
    );
    assert_eq!(
        requirements("SHOW CURRENT USER", Some(&nobody))
            .unwrap()
            .lacking,
        None
    );
    let needs = requirements("EXPLAIN MATCH (n) DELETE n", None).unwrap();
    assert_eq!((needs.explain, needs.kind), (true, StatementKind::Write));
    // EXPLAIN runs nothing, whoever asks.
    let plan = run_as(&db, &nobody, "EXPLAIN DROP USER bot").unwrap();
    assert!(
        format!("{:?}", plan[0][2]).contains("manage_accounts"),
        "{plan:?}"
    );
    assert!(db.begin_read().unwrap().account("bot").unwrap().is_some());
}
