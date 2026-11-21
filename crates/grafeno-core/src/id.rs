use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// UUID v7 (RFC 9562): 48 bits of milliseconds, 12 of sequence and 62 random.
///
/// The sequence (`rand_a`) starts at a random value every millisecond and goes
/// up with each id, so ids from one process are strictly increasing even if the
/// clock goes back. It avoids `getrandom`: `RandomState` is already seeded by
/// the operating system, and these ids do not need to be cryptographic.
fn now_v7() -> Uuid {
    static LAST: Mutex<(u64, u64)> = Mutex::new((0, 0));
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        & 0xFFFF_FFFF_FFFF;
    let (millis, sequence) = {
        let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if millis > last.0 {
            *last = (millis, random_u64() & 0x7FF);
        } else if last.1 < 0xFFF {
            last.1 += 1;
        } else {
            *last = (last.0 + 1, 0);
        }
        *last
    };
    let value = (millis as u128) << 80
        | 0x7 << 76
        | (sequence as u128) << 64
        | 0b10 << 62
        | (random_u64() as u128 & ((1 << 62) - 1));
    Uuid::from_u128(value)
}

fn random_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(0);
    hasher.finish()
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// A new id (UUID v7: ordered by creation time).
            pub fn new() -> Self {
                Self(now_v7())
            }

            pub fn from_u128(value: u128) -> Self {
                Self(Uuid::from_u128(value))
            }

            pub fn as_u128(self) -> u128 {
                self.0.as_u128()
            }

            pub fn uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// Identifier of a node.
    NodeId
);
id_type!(
    /// Identifier of an edge.
    EdgeId
);
id_type!(
    /// Identifier of a hyperedge.
    HyperEdgeId
);
id_type!(
    /// Identifier of a recorded merge of two nodes.
    MergeId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_v7_and_increasing() {
        let ids: Vec<NodeId> = (0..10_000).map(|_| NodeId::new()).collect();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(ids[0].uuid().get_version_num(), 7);
        assert_eq!(ids[0].uuid().get_variant(), uuid::Variant::RFC4122);
    }
}
