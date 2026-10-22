//! Hashes as Nix prints and parses them: base16, nix32, base64 and SRI.

use sha2::Digest as _;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Algo {
    Sha1,
    Sha256,
    Sha512,
}

impl Algo {
    pub fn name(self) -> &'static str {
        match self {
            Algo::Sha1 => "sha1",
            Algo::Sha256 => "sha256",
            Algo::Sha512 => "sha512",
        }
    }

    pub fn parse(s: &str) -> Option<Algo> {
        Some(match s {
            "sha1" => Algo::Sha1,
            "sha256" => Algo::Sha256,
            "sha512" => Algo::Sha512,
            _ => return None,
        })
    }

    pub fn size(self) -> usize {
        match self {
            Algo::Sha1 => 20,
            Algo::Sha256 => 32,
            Algo::Sha512 => 64,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash {
    pub algo: Algo,
    pub bytes: Digest,
}

/// A digest's bytes, held inline (sha512 at most): a hash never allocates.
#[derive(Clone, Copy)]
pub struct Digest {
    buf: [u8; 64],
    len: u8,
}

impl Digest {
    pub fn new(b: &[u8]) -> Digest {
        let mut buf = [0; 64];
        buf[..b.len()].copy_from_slice(b);
        Digest { buf, len: b.len() as u8 }
    }
}

impl std::ops::Deref for Digest {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

impl PartialEq for Digest {
    fn eq(&self, o: &Digest) -> bool {
        **self == **o
    }
}

impl Eq for Digest {}

impl PartialOrd for Digest {
    fn partial_cmp(&self, o: &Digest) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Digest {
    fn cmp(&self, o: &Digest) -> std::cmp::Ordering {
        (**self).cmp(&**o)
    }
}

impl std::hash::Hash for Digest {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        (**self).hash(h)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo.name(), self.to_base16())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid hash '{0}': {1}")]
pub struct HashError(pub String, pub &'static str);

const NIX32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// digit of each byte, 0xff for none
const NIX32_DIGIT: [u8; 256] = {
    let mut t = [0xffu8; 256];
    let mut i = 0;
    while i < 32 {
        t[NIX32[i] as usize] = i as u8;
        i += 1;
    }
    t
};

pub fn nix32_len(n: usize) -> usize {
    (n * 8 - 1) / 5 + 1
}

pub fn nix32_encode(bytes: &[u8]) -> String {
    let len = nix32_len(bytes.len());
    let mut s = String::with_capacity(len);
    for n in (0..len).rev() {
        let b = n * 5;
        let (i, j) = (b / 8, b % 8);
        let lo = (bytes[i] as u16) >> j;
        let hi = if i + 1 < bytes.len() { (bytes[i + 1] as u16) << (8 - j) } else { 0 };
        s.push(NIX32[((lo | hi) & 0x1f) as usize] as char);
    }
    s
}

pub fn nix32_decode(s: &str, size: usize) -> Option<Vec<u8>> {
    if s.len() != nix32_len(size) {
        return None;
    }
    let mut out = vec![0u8; size];
    for (n, c) in s.bytes().rev().enumerate() {
        let digit = match NIX32_DIGIT[c as usize] {
            0xff => return None,
            d => d as u16,
        };
        let b = n * 5;
        let (i, j) = (b / 8, b % 8);
        out[i] |= (digit << j) as u8;
        let carry = digit >> (8 - j);
        if i + 1 < size {
            out[i + 1] |= carry as u8;
        } else if carry != 0 {
            return None;
        }
    }
    Some(out)
}

fn base64_engine() -> &'static [u8; 64] {
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
}

pub fn base64_encode(bytes: &[u8]) -> String {
    let t = base64_engine();
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.len();
        let v = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= n {
                s.push(t[((v >> (18 - 6 * k)) & 0x3f) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let t = base64_engine();
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        acc = acc << 6 | t.iter().position(|&d| d == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

pub fn base16_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    base16_into(&mut s, bytes);
    s
}

/// `bytes` as lowercase hex, appended to `s`.
pub fn base16_into(s: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
}

fn base16_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

impl Hash {
    pub fn sha256(data: impl AsRef<[u8]>) -> Hash {
        Hash { algo: Algo::Sha256, bytes: Digest::new(&sha2::Sha256::digest(data.as_ref())) }
    }

    pub fn to_base16(&self) -> String {
        base16_encode(&self.bytes)
    }

    pub fn to_nix32(&self) -> String {
        nix32_encode(&self.bytes)
    }

    pub fn to_sri(&self) -> String {
        format!("{}-{}", self.algo.name(), base64_encode(&self.bytes))
    }

    /// `sha256:<hex>`, as store path fingerprints spell it.
    pub fn to_typed_base16(&self) -> String {
        let mut s = String::with_capacity(8 + self.bytes.len() * 2);
        self.typed_base16_into(&mut s);
        s
    }

    /// [`Hash::to_typed_base16`], appended to `s`.
    pub fn typed_base16_into(&self, s: &mut String) {
        s.push_str(self.algo.name());
        s.push(':');
        base16_into(s, &self.bytes);
    }

    /// XOR-fold to `size` bytes (store path hash parts are 20).
    pub fn compress(&self, size: usize) -> Digest {
        let mut out = Digest::new(&[0u8; 64][..size]);
        for (i, b) in self.bytes.iter().enumerate() {
            out.buf[i % size] ^= b;
        }
        out
    }

    /// Like Nix's `Hash::parseAny`/`newHashAllowEmpty`: SRI, `algo:digest`, or a bare digest
    /// whose algorithm comes from `algo` (outputHashAlgo).
    pub fn parse(s: &str, algo: Option<Algo>) -> Result<Hash, HashError> {
        let err = |m| HashError(s.to_string(), m);
        let (algo, digest, sri) = if let Some((a, d)) = s.split_once('-').filter(|(a, _)| Algo::parse(a).is_some()) {
            (Algo::parse(a).unwrap(), d, true)
        } else if let Some((a, d)) = s.split_once(':') {
            (Algo::parse(a).ok_or_else(|| err("unknown algorithm"))?, d, false)
        } else {
            (algo.ok_or_else(|| err("no algorithm given"))?, s, false)
        };
        let size = algo.size();
        let bytes = if sri {
            base64_decode(digest)
        } else if digest.len() == size * 2 {
            base16_decode(digest)
        } else if digest.len() == nix32_len(size) {
            nix32_decode(digest, size)
        } else {
            base64_decode(digest)
        }
        .filter(|b| b.len() == size)
        .ok_or_else(|| err("wrong length or bad digit"))?;
        Ok(Hash { algo, bytes: Digest::new(&bytes) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nix32_roundtrip() {
        let h = Hash::sha256("nix-output:out");
        assert_eq!(h.to_nix32(), "1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9");
        assert_eq!(nix32_decode(&h.to_nix32(), 32).unwrap(), &h.bytes[..]);
    }

    #[test]
    fn parse_forms() {
        let h = Hash::sha256("abc");
        for s in [h.to_sri(), h.to_typed_base16(), format!("sha256:{}", h.to_nix32())] {
            assert_eq!(Hash::parse(&s, None).unwrap(), h);
        }
        assert_eq!(Hash::parse(&h.to_nix32(), Some(Algo::Sha256)).unwrap(), h);
    }
}
