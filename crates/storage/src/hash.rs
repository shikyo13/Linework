//! The digest that identifies a saved revision and a recovery base: BLAKE3,
//! about 9 GB/s on one thread against SHA-256's 2.5, so a large save is bound
//! by the disk rather than the hash. Recovery records written before BLAKE3
//! carry SHA-256 digests; each stored digest names its kind, and a record
//! without one is SHA-256.
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

pub(crate) type Hasher = blake3::Hasher;

pub(crate) fn finish(hasher: &Hasher) -> [u8; 32] {
    *hasher.finalize().as_bytes()
}

pub(crate) fn of(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    #[default]
    Sha256,
    Blake3,
}

/// A hasher of either kind, for checking a stored digest.
pub(crate) enum Any {
    Sha256(sha2::Sha256),
    Blake3(Box<Hasher>),
}
impl Any {
    pub(crate) fn new(kind: Kind) -> Self {
        match kind {
            Kind::Sha256 => Self::Sha256(sha2::Sha256::new()),
            Kind::Blake3 => Self::Blake3(Box::new(Hasher::new())),
        }
    }
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha256(h) => h.update(bytes),
            Self::Blake3(h) => {
                h.update(bytes);
            }
        }
    }
    pub(crate) fn finish(self) -> [u8; 32] {
        match self {
            Self::Sha256(h) => h.finalize().into(),
            Self::Blake3(h) => finish(&h),
        }
    }
}
