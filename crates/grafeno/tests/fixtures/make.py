"""Builds old versions of Grafeno (one commit per layout version) and writes a
fixture database with each, compressed into this folder. The upgrade test
(crates/grafeno/tests/upgrade.rs) opens them with the current code.

    python crates/grafeno/tests/fixtures/make.py [version ...]

A new layout version adds its commit to COMMITS (and what it introduced to
EXTRAS). Needs git, Rust and the repository's history.
"""
import gzip
import os
import shutil
import subprocess
import sys

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), '..', '..', '..', '..'))
import tempfile
WORK = os.path.join(tempfile.gettempdir(), 'grafeno-fixtures')
TARGET = os.path.join(WORK, 'target')
OUT = os.path.join(REPO, 'crates', 'grafeno', 'tests', 'fixtures')

COMMITS = [(1, 'f99ab08'), (2, '0622a34'), (3, '74a2cf8'), (4, '5fdcdc4'), (5, '07256b3'), (6, '50fda41'), (7, '1d262d4'), (8, '8e473f5'), (9, '25a14fb'), (10, 'c560ceb'), (11, 'ca015c2'), (12, 'fc66729'), (13, '2ddd51d'), (14, '8f920bc'), (15, '88d48dc'), (16, '0e3a886'), (17, 'be9227b')]

BASE = r'''
// Writes the fixture of this layout version (see crates/grafeno/tests/fixtures).
use grafeno::{Grafeno, NewEdge, NewNode, Timestamp};

const JAN_2020: i64 = 1_577_836_800_000_000;
const JAN_2022: i64 = 1_640_995_200_000_000;

fn main() {
    let path = std::env::args().nth(1).expect("output path");
    let db = Grafeno::open(&path).unwrap();
    db.write(|tx| {
        let ns = "fixture";
        let mut people = Vec::new();
        for i in 0..40u32 {
            let angle = i as f32;
            let id = tx.create_node(
                ns,
                NewNode::new("Person")
                    .prop("name", format!("Person {i}"))
                    .prop("age", 20 + i)
                    .text(format!("person {i} works on graphs and memory topic{}", i % 5))
                    .embedding(vec![angle.cos(), angle.sin(), 1.0]),
            )?;
            people.push(id);
        }
        let acme = tx.create_node(ns, NewNode::new("Company").prop("name", "Acme").text("Acme builds graph engines"))?;
        for (i, id) in people.iter().enumerate() {
            let mut fact = NewEdge::new(*id, "works_at", acme);
            if i % 2 == 0 {
                fact = fact
                    .valid_from(Timestamp::from_micros(JAN_2020))
                    .valid_to(Timestamp::from_micros(JAN_2022));
            }
            tx.create_edge(ns, fact)?;
            if i > 0 {
                tx.create_edge(ns, NewEdge::new(people[i - 1], "knows", *id))?;
            }
        }
        tx.create_node("other", NewNode::new("Note").prop("title", "elsewhere").text("another namespace"))?;
        EXTRA
        Ok(())
    })
    .unwrap();
    println!("wrote {path}");
}
'''

EXTRAS = {
    4: '''tx.set_importance(ns, people[0], 0.9)?;''',
    5: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef { name: "Person_age".into(), label: "Person".into(), key: "age".into(), unique: false })?;''',
    6: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef { name: "Person_age".into(), label: "Person".into(), key: "age".into(), unique: false })?;
        tx.create_namespace("empty", Some("kept empty"))?;''',
    7: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef { name: "Person_age".into(), label: "Person".into(), key: "age".into(), unique: false })?;
        tx.create_namespace("empty", Some("kept empty"))?;
        tx.create_principal(&grafeno::Principal { name: "reader".into(), secret_hash: "00".repeat(32), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], owner: false, own_only: false, description: None, created_at: Timestamp::from_micros(JAN_2022) })?;''',
    8: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef { name: "Person_age".into(), label: "Person".into(), key: "age".into(), unique: false })?;
        tx.create_namespace("empty", Some("kept empty"))?;
        tx.create_principal(&grafeno::Principal { name: "reader".into(), secret_hash: "00".repeat(32), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], owner: false, own_only: false, description: None, created_at: Timestamp::from_micros(JAN_2022) })?;''',
    9: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef::new("Person", "age"))?;
        tx.create_namespace("empty", Some("kept empty"))?;
        tx.create_principal(&grafeno::Principal { name: "reader".into(), secret_hash: "00".repeat(32), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], owner: false, own_only: false, description: None, created_at: Timestamp::from_micros(JAN_2022) })?;''',
    10: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef::new("Person", "age"))?;
        tx.create_namespace("empty", Some("kept empty"))?;
        tx.create_principal(&grafeno::Principal { name: "reader".into(), secret_hash: "00".repeat(32), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], owner: false, own_only: false, description: None, created_at: Timestamp::from_micros(JAN_2022) })?;''',
    11: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef::new("Person", "age"))?;
        tx.create_namespace("empty", Some("kept empty"))?;
        tx.create_principal(&grafeno::Principal { name: "reader".into(), secret_hash: "00".repeat(32), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], owner: false, own_only: false, description: None, created_at: Timestamp::from_micros(JAN_2022) })?;
        tx.create_constraint(ns, grafeno::Constraint::Functional { name: "Person_age_functional".into(), label: "Person".into(), key: "age".into() })?;''',
    12: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef::new("Person", "age"))?;
        tx.create_namespace("empty", Some("kept empty"))?;
        tx.create_principal(&grafeno::Principal { name: "reader".into(), secret_hash: "00".repeat(32), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], owner: false, own_only: false, description: None, created_at: Timestamp::from_micros(JAN_2022) })?;
        tx.create_constraint(ns, grafeno::Constraint::Functional { name: "Person_age_functional".into(), label: "Person".into(), key: "age".into() })?;''',
    13: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef::new("Person", "age"))?;
        tx.create_namespace("empty", Some("kept empty"))?;
        tx.create_principal(&grafeno::Principal { name: "reader".into(), secret_hash: "00".repeat(32), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], owner: false, own_only: false, description: None, created_at: Timestamp::from_micros(JAN_2022) })?;
        tx.create_constraint(ns, grafeno::Constraint::Functional { name: "Person_age_functional".into(), label: "Person".into(), key: "age".into() })?;''',
    14: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef::new("Person", "age"))?;
        tx.create_namespace("empty", Some("kept empty"))?;
        tx.create_principal(&grafeno::Principal { name: "reader".into(), secret_hash: "00".repeat(32), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], owner: false, own_only: false, description: None, created_at: Timestamp::from_micros(JAN_2022) })?;
        tx.create_principal(&grafeno::Principal { name: "admin-key".into(), secret_hash: "11".repeat(32), grants: Vec::new(), owner: true, own_only: false, description: Some("the owner's key".into()), created_at: Timestamp::from_micros(JAN_2022) })?;
        tx.create_principal(&grafeno::Principal { name: "agent".into(), secret_hash: "22".repeat(32), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Write }], owner: false, own_only: true, description: None, created_at: Timestamp::from_micros(JAN_2022) })?;
        tx.create_constraint(ns, grafeno::Constraint::Functional { name: "Person_age_functional".into(), label: "Person".into(), key: "age".into() })?;''',
    15: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef::new("Person", "age"))?;
        tx.create_namespace("empty", Some("kept empty"))?;
        let at = Timestamp::from_micros(JAN_2022);
        tx.create_account(&grafeno::Account { name: "reader".into(), kind: grafeno::AccountKind::Service, password_hash: None, grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], superuser: false, own_only: false, disabled: false, description: None, created_at: at })?;
        tx.create_api_key(&grafeno::ApiKey { name: "reader".into(), account: "reader".into(), secret_hash: "00".repeat(32), scope: None, expires_at: None, description: None, created_at: at, rotated_at: None })?;
        tx.create_account(&grafeno::Account { name: "ana".into(), kind: grafeno::AccountKind::User, password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g".into()), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Write }], superuser: false, own_only: true, disabled: false, description: None, created_at: at })?;
        tx.create_api_key(&grafeno::ApiKey { name: "ana-ro".into(), account: "ana".into(), secret_hash: "33".repeat(32), scope: Some(grafeno::KeyScope { namespaces: vec!["fixture".into()], read_only: true }), expires_at: None, description: None, created_at: at, rotated_at: None })?;
        tx.put_session(&grafeno::Session { id: "s1".into(), account: "ana".into(), token_hash: "44".repeat(32), expires_at: at, refresh_hash: "55".repeat(32), refresh_expires_at: at, created_at: at, refreshed_at: None, address: None })?;
        tx.create_constraint(ns, grafeno::Constraint::Functional { name: "Person_age_functional".into(), label: "Person".into(), key: "age".into() })?;''',
    16: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef::new("Person", "age"))?;
        tx.create_namespace("empty", Some("kept empty"))?;
        let at = Timestamp::from_micros(JAN_2022);
        tx.create_account(&grafeno::Account { name: "reader".into(), kind: grafeno::AccountKind::Service, password_hash: None, grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], roles: Vec::new(), superuser: false, own_only: false, disabled: false, expires_at: None, attributes: Default::default(), description: None, created_at: at })?;
        tx.create_api_key(&grafeno::ApiKey { name: "reader".into(), account: "reader".into(), secret_hash: "00".repeat(32), scope: None, expires_at: None, description: None, created_at: at, rotated_at: None })?;
        tx.put_role(&grafeno::RoleDef { name: "analyst".into(), privileges: vec![grafeno::Privilege::deny(grafeno::Act::Read, grafeno::Object::Label { namespace: "fixture".into(), label: "Salary".into(), edges: false })], grants: Vec::new(), roles: Vec::new(), policies: vec![grafeno::Policy { namespace: "fixture".into(), label: None, applies: grafeno::Applies::Write, condition: grafeno::Condition::RecordedByCaller }], description: None, created_at: at })?;
        tx.create_account(&grafeno::Account { name: "ana".into(), kind: grafeno::AccountKind::User, password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g".into()), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Write }], roles: vec!["analyst".into()], superuser: false, own_only: true, disabled: false, expires_at: None, attributes: [("team".to_owned(), serde_json::json!("red"))].into_iter().collect(), description: None, created_at: at })?;
        tx.create_api_key(&grafeno::ApiKey { name: "ana-ro".into(), account: "ana".into(), secret_hash: "33".repeat(32), scope: Some(grafeno::KeyScope { namespaces: vec!["fixture".into()], read_only: true }), expires_at: None, description: None, created_at: at, rotated_at: None })?;
        tx.put_session(&grafeno::Session { id: "s1".into(), account: "ana".into(), token_hash: "44".repeat(32), expires_at: at, refresh_hash: "55".repeat(32), refresh_expires_at: at, created_at: at, refreshed_at: None, address: None })?;
        tx.create_constraint(ns, grafeno::Constraint::Functional { name: "Person_age_functional".into(), label: "Person".into(), key: "age".into() })?;''',
    17: '''tx.set_importance(ns, people[0], 0.9)?;
        tx.create_index(ns, grafeno::IndexDef::new("Person", "age"))?;
        tx.create_namespace("empty", Some("kept empty"))?;
        let at = Timestamp::from_micros(JAN_2022);
        tx.create_account(&grafeno::Account { name: "reader".into(), kind: grafeno::AccountKind::Service, password_hash: None, grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Read }], roles: Vec::new(), superuser: false, own_only: false, disabled: false, expires_at: None, attributes: Default::default(), description: None, created_at: at })?;
        tx.create_api_key(&grafeno::ApiKey { name: "reader".into(), account: "reader".into(), secret_hash: "00".repeat(32), scope: None, expires_at: None, description: None, created_at: at, rotated_at: None })?;
        tx.put_role(&grafeno::RoleDef { name: "analyst".into(), privileges: vec![grafeno::Privilege::deny(grafeno::Act::Read, grafeno::Object::Label { namespace: "fixture".into(), label: "Salary".into(), edges: false })], grants: Vec::new(), roles: Vec::new(), policies: vec![grafeno::Policy { namespace: "fixture".into(), label: None, applies: grafeno::Applies::Write, condition: grafeno::Condition::RecordedByCaller }], description: None, created_at: at })?;
        tx.create_account(&grafeno::Account { name: "ana".into(), kind: grafeno::AccountKind::User, password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g".into()), grants: vec![grafeno::Grant { namespace: "fixture".into(), role: grafeno::Role::Write }], roles: vec!["analyst".into()], superuser: false, own_only: true, disabled: false, expires_at: None, attributes: [("team".to_owned(), serde_json::json!("red"))].into_iter().collect(), description: None, created_at: at })?;
        tx.create_api_key(&grafeno::ApiKey { name: "ana-ro".into(), account: "ana".into(), secret_hash: "33".repeat(32), scope: Some(grafeno::KeyScope { namespaces: vec!["fixture".into()], read_only: true }), expires_at: None, description: None, created_at: at, rotated_at: None })?;
        tx.put_session(&grafeno::Session { id: "s1".into(), account: "ana".into(), token_hash: "44".repeat(32), expires_at: at, refresh_hash: "55".repeat(32), refresh_expires_at: at, created_at: at, refreshed_at: None, address: None })?;
        tx.create_constraint(ns, grafeno::Constraint::Functional { name: "Person_age_functional".into(), label: "Person".into(), key: "age".into() })?;
        tx.create_index(ns, grafeno::IndexDef::composite("Person", &["name", "age"]))?;''',
}


def run(*args, cwd=None, env=None):
    print('>', ' '.join(args), flush=True)
    subprocess.run(args, cwd=cwd, check=True, env=env)


def main():
    os.makedirs(WORK, exist_ok=True)
    os.makedirs(OUT, exist_ok=True)
    env = dict(os.environ, CARGO_TARGET_DIR=TARGET)
    only = {int(v) for v in sys.argv[1:]}
    for version, commit in COMMITS:
        if only and version not in only:
            continue
        tree = os.path.join(WORK, f'v{version}')
        if os.path.exists(tree):
            run('git', 'worktree', 'remove', '--force', tree, cwd=REPO)
        run('git', 'worktree', 'add', '--detach', tree, commit, cwd=REPO)
        example = os.path.join(tree, 'crates', 'grafeno', 'examples')
        os.makedirs(example, exist_ok=True)
        with open(os.path.join(example, 'fixture_gen.rs'), 'w', encoding='utf-8') as f:
            f.write(BASE.replace('EXTRA', EXTRAS.get(version, '')))
        db = os.path.join(WORK, f'layout-v{version}.redb')
        if os.path.exists(db):
            os.remove(db)
        run('cargo', 'run', '-q', '-p', 'grafeno', '--example', 'fixture_gen', '--', db, cwd=tree, env=env)
        with open(db, 'rb') as source, gzip.open(os.path.join(OUT, f'layout-v{version}.redb.gz'), 'wb', 9) as target:
            shutil.copyfileobj(source, target)
        run('git', 'worktree', 'remove', '--force', tree, cwd=REPO)
    print('done')


main()
