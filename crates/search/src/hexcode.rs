//! hashcat writes a password it cannot print plainly as `$HEX[...]`: its
//! bytes in hexadecimal. `decode` turns such text back into those bytes.
use std::borrow::Cow;

const OPEN: &[u8] = b"$HEX[";

/// `s` with every well-formed `$HEX[...]` (an even number of hex digits)
/// replaced by the bytes it stands for. Anything else is left as it is.
pub fn decode(s: &[u8]) -> Cow<'_, [u8]> {
    let Some(first) = memchr::memmem::find(s, OPEN) else {
        return Cow::Borrowed(s);
    };
    let mut out = Vec::with_capacity(s.len());
    let mut at = 0;
    let mut open = first;
    loop {
        out.extend_from_slice(&s[at..open]);
        let body = open + OPEN.len();
        let decoded = memchr::memchr(b']', &s[body..])
            .map(|p| body + p)
            .and_then(|end| hex(&s[body..end]).map(|bytes| (end, bytes)));
        match decoded {
            Some((end, bytes)) => {
                out.extend_from_slice(&bytes);
                at = end + 1;
            }
            None => {
                out.extend_from_slice(OPEN);
                at = body;
            }
        }
        match memchr::memmem::find(&s[at..], OPEN) {
            Some(p) => open = at + p,
            None => break,
        }
    }
    out.extend_from_slice(&s[at..]);
    Cow::Owned(out)
}

fn hex(digits: &[u8]) -> Option<Vec<u8>> {
    if digits.len() % 2 != 0 {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hex_passwords_become_their_bytes_and_anything_else_stays() {
        assert_eq!(&*decode(b"ab:$HEX[70617373]"), b"ab:pass");
        assert_eq!(&*decode(b"$HEX[3a3a] and $HEX[]x"), b":: and x");
        assert_eq!(&*decode(b"$HEX[7] $HEX[zz] $HEX[61"), b"$HEX[7] $HEX[zz] $HEX[61");
        assert_eq!(&*decode(b"$HEX[$HEX[62]"), b"$HEX[b");
        assert!(matches!(decode(b"plain"), Cow::Borrowed(_)));
    }
}
