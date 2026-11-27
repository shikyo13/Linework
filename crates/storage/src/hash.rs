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

/// One piece of an input hashed on its own, so pieces can be hashed on several
/// threads. Every piece but the last is `piece_len` bytes, a power of two of at
/// least one BLAKE3 chunk, and piece `index` starts at `index * piece_len`, so
/// each is a whole subtree of the input's BLAKE3 tree. Returns the piece's
/// chaining value and, for the first piece, also its ordinary hash (the
/// input's digest when the input is that piece alone). None for no bytes.
pub(crate) fn piece(index: usize, piece_len: usize, bytes: &[u8]) -> Option<([u8; 32], Option<[u8; 32]>)> {
    use blake3::hazmat::HasherExt;
    if bytes.is_empty() {
        return None;
    }
    let mut hasher = Hasher::new();
    if index > 0 {
        hasher.set_input_offset((index * piece_len) as u64);
    }
    hasher.update(bytes);
    Some((hasher.finalize_non_root(), (index == 0).then(|| finish(&hasher))))
}

/// The digest of an input of `total` bytes from its pieces' chaining values in
/// order (see `piece`): the same value as hashing the input in one pass.
pub(crate) fn join(values: &[[u8; 32]], piece_len: usize, total: u64, first: Option<[u8; 32]>) -> [u8; 32] {
    use blake3::hazmat::{left_subtree_len, merge_subtrees_non_root, merge_subtrees_root, Mode};
    if total <= piece_len as u64 {
        return first.unwrap_or_else(|| of(b""));
    }
    // A subtree's left child is the largest power of two below its length,
    // which for more than one piece is a whole number of pieces.
    fn subtree(values: &[[u8; 32]], piece_len: usize, len: u64) -> [u8; 32] {
        if values.len() == 1 {
            return values[0];
        }
        let left = left_subtree_len(len);
        let k = (left / piece_len as u64) as usize;
        let l = subtree(&values[..k], piece_len, left);
        let r = subtree(&values[k..], piece_len, len - left);
        merge_subtrees_non_root(&l, &r, Mode::Hash)
    }
    let left = left_subtree_len(total);
    let k = (left / piece_len as u64) as usize;
    let l = subtree(&values[..k], piece_len, left);
    let r = subtree(&values[k..], piece_len, total - left);
    *merge_subtrees_root(&l, &r, Mode::Hash).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pieces joined give `blake3::hash` for every shape of tree: empty, one
    /// chunk, one piece, exact multiples, and uneven last pieces.
    #[test]
    fn pieces_joined_equal_one_pass() {
        const PIECE: usize = 4096;
        let input: Vec<u8> = (0..40 * PIECE + 3000).map(|i| (i * 131 % 251) as u8).collect();
        let mut lengths = vec![0, 1, 1023, 1024, 1025, PIECE - 1, PIECE, PIECE + 1, 2 * PIECE, 2 * PIECE + 1];
        lengths.extend((3..=40).flat_map(|n| [n * PIECE - 1, n * PIECE, n * PIECE + 1, n * PIECE + 2047]));
        for len in lengths {
            let bytes = &input[..len];
            let mut values = Vec::new();
            let mut first = None;
            for (index, part) in bytes.chunks(PIECE).enumerate() {
                let (value, root) = piece(index, PIECE, part).unwrap();
                values.push(value);
                if index == 0 {
                    first = root;
                }
            }
            assert_eq!(join(&values, PIECE, len as u64, first), of(bytes), "length {len}");
        }
    }
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
