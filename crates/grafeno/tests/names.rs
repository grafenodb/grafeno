//! The documentation and the sources describe Grafeno on its own terms: they
//! name no other company or product, no client application and no game.
//! Operating systems and distributions stay (the installation guide needs
//! them), as do Grafeno's own dependencies.

use std::path::{Path, PathBuf};

/// Words that must not appear, matched without case at word boundaries.
const DENIED: &[&str] = &[
    "neo4j",
    "apoc",
    "cypher",
    "opencypher",
    "bolt",
    "tck",
    "grafito",
    "postgres",
    "postgresql",
    "pgadmin",
    "psql",
    "pg_dump",
    "initdb",
    "gephi",
    "yed",
    "networkx",
    "igraph",
    "sqlite",
    "jepsen",
    "etcd",
    "zookeeper",
    "openraft",
    "claude",
    "anthropic",
    "oracle",
    "memgraph",
    "tigergraph",
    "arangodb",
    "janusgraph",
    "dgraph",
    "elasticsearch",
    "mongodb",
    "rimworld",
    "mem0",
    "graphiti",
    "memgpt",
    "letta",
    "mempalace",
    "cognee",
    "minecraft",
];

/// Uses of a denied word that name a platform, not a product to compare.
const ALLOWED: &[&str] = &["oracle linux"];

/// Folders left out when there is no git checkout to list the files:
/// build output, dependencies, binaries.
const SKIPPED_DIRS: &[&str] = &["target", "node_modules", ".git", "dist"];
const TEXT: &[&str] = &[
    "md", "rs", "ts", "toml", "yml", "yaml", "sh", "py", "iss", "txt", "json", "service", "html",
    "css", "mjs", "js",
];

/// Whether a file is one this test reads: text, not this test itself.
fn checked(path: &Path) -> bool {
    let name = path.file_name().unwrap().to_string_lossy();
    if name == "names.rs" || name == "package-lock.json" {
        return false;
    }
    path.extension()
        .is_some_and(|ext| TEXT.contains(&ext.to_string_lossy().as_ref()))
        || ["Dockerfile", "LICENSE-MIT", "install.sh"].contains(&name.as_ref())
}

/// Every file of the repository, as git knows them (committed or staged):
/// what CI checks, without a developer's own untracked notes. Outside a
/// git checkout (a source archive), every file under `root`.
fn files(root: &Path) -> Vec<PathBuf> {
    let listed = std::process::Command::new("git")
        .args(["ls-files", "--cached", "-z"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success());
    let mut out = Vec::new();
    match listed {
        Some(output) => {
            for name in output.stdout.split(|byte| *byte == 0) {
                if name.is_empty() {
                    continue;
                }
                let path = root.join(String::from_utf8_lossy(name).as_ref());
                if path.is_file() && checked(&path) {
                    out.push(path);
                }
            }
        }
        None => walk(root, &mut out),
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if path.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_str()) {
                walk(&path, out);
            }
        } else if checked(&path) {
            out.push(path);
        }
    }
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The denied words in `text` (lowercased), each with its line.
fn denied_in(text: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        for word in DENIED {
            let mut from = 0;
            while let Some(at) = lower[from..].find(word) {
                let start = from + at;
                let end = start + word.len();
                from = end;
                let before = lower[..start].chars().next_back();
                let after = lower[end..].chars().next();
                if before.is_some_and(word_char) || after.is_some_and(word_char) {
                    continue;
                }
                if ALLOWED
                    .iter()
                    .any(|allowed| lower[start..].starts_with(allowed))
                {
                    continue;
                }
                found.push((number + 1, *word));
            }
        }
    }
    found
}

#[test]
fn documentation_and_sources_name_no_other_product() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let all = files(&root);
    assert!(all.len() > 100, "found only {} files", all.len());
    let mut problems = Vec::new();
    for path in all {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (line, word) in denied_in(&text) {
            let shown = path.strip_prefix(&root).unwrap_or(&path).display();
            problems.push(format!("{shown}:{line}: {word}"));
        }
    }
    assert!(
        problems.is_empty(),
        "names of other products or applications:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_check_finds_words_and_respects_boundaries() {
    assert_eq!(denied_in("Like PSQL does"), [(1, "psql")]);
    assert_eq!(denied_in("an Oracle Linux machine"), []);
    assert_eq!(denied_in("as Oracle does"), [(1, "oracle")]);
    assert_eq!(denied_in("played yesterday, postgresqlish"), []);
}
