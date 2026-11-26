//! Namespace aliases: a name that stands for one namespace wherever a
//! namespace name goes, and that can be moved to another namespace in one
//! commit (build `people_v2`, then point `people` at it).
//!
//! An alias is kept in the state store under a name no namespace can have,
//! so it is written in the same transactions as the graph, replicated as
//! any write and backed up with the file. Alias names and namespace names
//! share one space: an alias cannot be named as a namespace, and a
//! namespace cannot be created with an alias's name.

use std::collections::HashMap;
use std::sync::Arc;

use grafeno_core::{Error, Result, Timestamp};
use serde::{Deserialize, Serialize};

use crate::db::{Grafeno, ReadTx, WriteTx};
use crate::namespace::check_new_namespace;

/// Where aliases are kept in the state store: a name with a control
/// character, which no namespace can have.
pub(crate) const ALIAS_NS: &str = "\u{1}aliases";
const KIND: &str = "alias";

/// An alias and the namespace it stands for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alias {
    pub name: String,
    pub namespace: String,
    /// When it was last pointed at its namespace.
    pub updated_at: Timestamp,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    namespace: String,
    updated_at: Timestamp,
}

fn decode(name: &str, bytes: &[u8]) -> Result<Alias> {
    let stored: Stored = serde_json::from_slice(bytes)
        .map_err(|error| Error::Serialization(format!("alias {name:?}: {error}")))?;
    Ok(Alias {
        name: name.to_owned(),
        namespace: stored.namespace,
        updated_at: stored.updated_at,
    })
}

impl ReadTx {
    /// The alias `name`, if there is one.
    pub fn alias(&self, name: &str) -> Result<Option<Alias>> {
        self.state(ALIAS_NS, KIND, name)?
            .map(|bytes| decode(name, &bytes))
            .transpose()
    }

    /// Every alias, by name.
    pub fn aliases(&self) -> Result<Vec<Alias>> {
        self.state_entries(ALIAS_NS, KIND, "", usize::MAX)?
            .into_iter()
            .map(|(name, bytes)| decode(&name, &bytes))
            .collect()
    }

    /// The namespace `name` stands for: its alias's target, or itself.
    pub fn resolve_namespace(&self, name: &str) -> Result<String> {
        Ok(match self.alias(name)? {
            Some(alias) => alias.namespace,
            None => name.to_owned(),
        })
    }
}

/// Every alias and its namespace, as of a commit.
pub(crate) type AliasMap = Arc<HashMap<String, String>>;

impl Grafeno {
    /// The namespace alias `name` points at now, or `None` when it is no
    /// alias. Kept in memory until the next commit, so that resolving a
    /// request's namespace costs a lookup.
    pub fn resolve_alias(&self, name: &str) -> Result<Option<String>> {
        if let Some(map) = self.shared.cached_aliases() {
            return Ok(map.get(name).cloned());
        }
        let read = self.begin_read()?;
        let map: AliasMap = Arc::new(
            read.aliases()?
                .into_iter()
                .map(|alias| (alias.name, alias.namespace))
                .collect(),
        );
        if let Some(generation) = read.generation {
            self.shared.keep_aliases(generation, map.clone());
        }
        Ok(map.get(name).cloned())
    }
}

impl WriteTx {
    pub fn alias(&self, name: &str) -> Result<Option<Alias>> {
        self.state(ALIAS_NS, KIND, name)?
            .map(|bytes| decode(name, &bytes))
            .transpose()
    }

    pub fn aliases(&self) -> Result<Vec<Alias>> {
        self.state_entries(ALIAS_NS, KIND, "", usize::MAX)?
            .into_iter()
            .map(|(name, bytes)| decode(&name, &bytes))
            .collect()
    }

    /// Points alias `name` at namespace `target`, creating it or moving it
    /// (in this transaction: readers see the old target until it commits,
    /// then the new one). The target must be a namespace, not an alias, and
    /// the name must not be a namespace's. Returns the namespace it pointed
    /// at before, if any.
    pub fn set_alias(&mut self, name: &str, target: &str) -> Result<Option<String>> {
        check_new_namespace(name)?;
        if self.namespace(name)?.is_some() {
            return Err(Error::Invalid(format!(
                "{name:?} is a namespace: an alias cannot have its name"
            )));
        }
        if self.alias(target)?.is_some() {
            return Err(Error::Invalid(format!(
                "{target:?} is an alias: point {name:?} at a namespace"
            )));
        }
        if self.namespace(target)?.is_none() {
            return Err(Error::Invalid(format!(
                "there is no namespace {target:?} to point {name:?} at"
            )));
        }
        let before = self.alias(name)?.map(|alias| alias.namespace);
        let stored = Stored {
            namespace: target.to_owned(),
            updated_at: Timestamp::now(),
        };
        let bytes = serde_json::to_vec(&stored).map_err(|e| Error::Serialization(e.to_string()))?;
        self.put_state(ALIAS_NS, KIND, name, &bytes)?;
        Ok(before)
    }

    /// Removes alias `name` (its namespace stays); whether it was there.
    pub fn drop_alias(&mut self, name: &str) -> Result<bool> {
        self.delete_state(ALIAS_NS, KIND, name)
    }

    /// The namespace `name` stands for: its alias's target, or itself.
    pub fn resolve_namespace(&self, name: &str) -> Result<String> {
        Ok(match self.alias(name)? {
            Some(alias) => alias.namespace,
            None => name.to_owned(),
        })
    }

    /// The aliases that point at `ns`.
    pub fn aliases_of(&self, ns: &str) -> Result<Vec<String>> {
        Ok(self
            .aliases()?
            .into_iter()
            .filter(|alias| alias.namespace == ns)
            .map(|alias| alias.name)
            .collect())
    }
}
