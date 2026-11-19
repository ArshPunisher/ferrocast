//! SRP-6a, the client side, as librist computes it for RIST's username and password
//! (VSF TR-06-2's EAP-SRP; librist 0.2.16 and later, RFC 5054 padding):
//!
//! - x = H(salt | H(user ":" password)), k = H(PAD(N) | PAD(g)), u = H(PAD(A) | PAD(B))
//! - S = (B - k·g^x)^(a + u·x) mod N, K = H(S)
//! - M1 = H(H(N) xor H(g) | H(user) | salt | A | B | K), M2 = H(A | M1 | K)
//!
//! H is SHA-256; numbers outside PAD go in their shortest big-endian form, as librist writes
//! them.

use num_bigint::BigUint;
use sha2::{Digest, Sha256};

/// RFC 5054's 2048-bit group, librist's default when the receiver sends none.
const N_2048: &str = "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A37329CBB4\
A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8083969EDB767B0CF60\
95179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF\
747359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A436C6481F1D2B907\
8717461A5B9D32E688F87748544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB37861\
60279004E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DB\
FBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73";

pub(crate) type Hash = [u8; 32];

/// One login's client side: the group, the salt, our secret a and public A.
pub(crate) struct SrpClient {
    n: BigUint,
    g: BigUint,
    salt: BigUint,
    a: BigUint,
    big_a: BigUint,
    /// After B: the session key, and our proof (M1).
    key: Hash,
    m1: Hash,
}

/// The shortest big-endian bytes of `n` (none for zero), as librist writes a number.
fn bytes(n: &BigUint) -> Vec<u8> {
    if n.bits() == 0 { Vec::new() } else { n.to_bytes_be() }
}

/// `n` left-padded with zeros to `len` bytes.
fn padded(n: &BigUint, len: usize) -> Option<Vec<u8>> {
    let b = bytes(n);
    (b.len() <= len).then(|| {
        let mut out = vec![0u8; len - b.len()];
        out.extend_from_slice(&b);
        out
    })
}

fn h(parts: &[&[u8]]) -> Hash {
    let mut d = Sha256::new();
    for p in parts {
        d.update(p);
    }
    d.finalize().into()
}

impl SrpClient {
    /// From the receiver's challenge: its salt, and its group (`None`: librist's default).
    /// `a` is our secret (random in use; fixed in tests).
    pub(crate) fn new(group: Option<(&[u8], &[u8])>, salt: &[u8], a: &[u8]) -> Result<Self, String> {
        if salt.is_empty() || salt.len() > 64 {
            return Err("the receiver's salt doesn't look right".into());
        }
        let (n, g) = match group {
            None => (BigUint::parse_bytes(N_2048.as_bytes(), 16).ok_or("group")?, BigUint::from(2u8)),
            Some((n, g)) => {
                if !(128..=1024).contains(&n.len()) || g.is_empty() || g.len() > n.len() {
                    return Err("the receiver's group doesn't look right".into());
                }
                (BigUint::from_bytes_be(n), BigUint::from_bytes_be(g))
            }
        };
        let a = BigUint::from_bytes_be(a);
        let big_a = g.modpow(&a, &n);
        Ok(Self { n, g, salt: BigUint::from_bytes_be(salt), a, big_a, key: [0; 32], m1: [0; 32] })
    }

    /// A, as sent.
    pub(crate) fn public(&self) -> Vec<u8> {
        bytes(&self.big_a)
    }

    /// The receiver's B: the session key and our proof (M1).
    pub(crate) fn handle_b(&mut self, b: &[u8], user: &str, password: &str) -> Result<(Hash, Hash), String> {
        let refused = || "the receiver's answer doesn't add up".to_string();
        let pad = bytes(&self.n).len();
        if b.is_empty() || b.len() > pad {
            return Err(refused());
        }
        let big_b = BigUint::from_bytes_be(b);
        if (&big_b % &self.n).bits() == 0 {
            return Err(refused());
        }
        let u = BigUint::from_bytes_be(&h(&[
            &padded(&self.big_a, pad).ok_or_else(refused)?,
            &padded(&big_b, pad).ok_or_else(refused)?,
        ]));
        if (&u % &self.n).bits() == 0 {
            return Err(refused());
        }
        let k = BigUint::from_bytes_be(&h(&[
            &padded(&self.n, pad).ok_or_else(refused)?,
            &padded(&self.g, pad).ok_or_else(refused)?,
        ]));
        let inner = h(&[user.as_bytes(), b":", password.as_bytes()]);
        let x = BigUint::from_bytes_be(&h(&[&bytes(&self.salt), &inner]));
        let kv = (k * self.g.modpow(&x, &self.n)) % &self.n;
        // B - k·g^x, kept positive mod N.
        let base = ((&big_b % &self.n) + &self.n - kv) % &self.n;
        let s = base.modpow(&(&self.a + u * x), &self.n);
        self.key = h(&[&bytes(&s)]);
        let (hn, hg) = (h(&[&bytes(&self.n)]), h(&[&bytes(&self.g)]));
        let mut xor = [0u8; 32];
        for (i, v) in xor.iter_mut().enumerate() {
            *v = hn[i] ^ hg[i];
        }
        self.m1 = h(&[
            &xor,
            &h(&[user.as_bytes()]),
            &bytes(&self.salt),
            &bytes(&self.big_a),
            &big_b.to_bytes_be(),
            &self.key,
        ]);
        Ok((self.key, self.m1))
    }

    /// The receiver's proof (M2): that it knew the verifier, so it's the real receiver.
    pub(crate) fn verify(&self, m2: &[u8]) -> bool {
        let want = h(&[&bytes(&self.big_a), &self.m1, &self.key]);
        m2.len() == 32 && m2.iter().zip(want).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The receiver's side, to check ours against: v = g^x, B = k·v + g^b.
    pub(crate) fn server(user: &str, password: &str, salt: &[u8], b: &[u8], big_a: &[u8]) -> (Vec<u8>, Hash, Hash) {
        let n = BigUint::parse_bytes(N_2048.as_bytes(), 16).unwrap();
        let g = BigUint::from(2u8);
        let pad = bytes(&n).len();
        let x = BigUint::from_bytes_be(&h(&[
            &bytes(&BigUint::from_bytes_be(salt)),
            &h(&[user.as_bytes(), b":", password.as_bytes()]),
        ]));
        let v = g.modpow(&x, &n);
        let k = BigUint::from_bytes_be(&h(&[&padded(&n, pad).unwrap(), &padded(&g, pad).unwrap()]));
        let b = BigUint::from_bytes_be(b);
        let big_b = (k * &v + g.modpow(&b, &n)) % &n;
        let a = BigUint::from_bytes_be(big_a);
        let u = BigUint::from_bytes_be(&h(&[&padded(&a, pad).unwrap(), &padded(&big_b, pad).unwrap()]));
        let s = (a * v.modpow(&u, &n)).modpow(&b, &n);
        let key = h(&[&bytes(&s)]);
        (bytes(&big_b), key, [0; 32])
    }

    #[test]
    fn both_sides_reach_the_same_key_and_proofs_check() {
        let salt = [7u8; 32];
        let mut c = SrpClient::new(None, &salt, &[0x13; 32]).unwrap();
        let (big_b, server_key, _) = server("alice", "pass word", &salt, &[0x42; 32], &c.public());
        let (key, m1) = c.handle_b(&big_b, "alice", "pass word").unwrap();
        assert_eq!(key, server_key, "the same session key");
        let m2 = h(&[&c.public(), &m1, &key]);
        assert!(c.verify(&m2));
        assert!(!c.verify(&[0u8; 32]));
        // A wrong password: another key.
        let mut wrong = SrpClient::new(None, &salt, &[0x13; 32]).unwrap();
        let (k2, _) = wrong.handle_b(&big_b, "alice", "nope").unwrap();
        assert_ne!(k2, server_key);
        // B = 0 (mod N) is refused.
        assert!(SrpClient::new(None, &salt, &[1; 32]).unwrap().handle_b(&[0], "alice", "x").is_err());
    }
}
