//! Bond tokens: the relay signs one for a Pro account (HMAC-SHA256 with a secret it shares
//! with this service), good for a few hours: `v1.<account>.<expires ms>.<signature hex>`.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

fn signature(secret: &[u8], account: &str, expires_ms: i64) -> Option<Vec<u8>> {
    let mut m = <HmacSha256 as Mac>::new_from_slice(secret).ok()?;
    m.update(format!("v1.{account}.{expires_ms}").as_bytes());
    Some(m.finalize().into_bytes().to_vec())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// An account id fit for a token: letters, digits, `-` and `_`, 1 to 64.
fn fit(account: &str) -> bool {
    (1..=64).contains(&account.len()) && account.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A token for `account` until `expires_ms` (Unix ms); None for an account id that can't be one.
pub fn sign(secret: &[u8], account: &str, expires_ms: i64) -> Option<String> {
    let sig = signature(secret, account, expires_ms)?;
    fit(account).then(|| format!("v1.{account}.{expires_ms}.{}", hex(&sig)))
}

/// The account a token is for, when it's ours and not expired at `now_ms`.
pub fn verify(secret: &[u8], token: &str, now_ms: i64) -> Option<String> {
    let mut parts = token.split('.');
    let (Some("v1"), Some(account), Some(exp), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let exp: i64 = exp.parse().ok()?;
    if !fit(account) || exp <= now_ms || secret.is_empty() {
        return None;
    }
    let want = signature(secret, account, exp)?;
    let got = unhex(sig)?;
    bool::from(want.ct_eq(&got)).then(|| account.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_holds_its_account_until_it_expires() {
        let t = sign(b"s3cret", "acct_42", 2_000).unwrap();
        assert_eq!(verify(b"s3cret", &t, 1_000).as_deref(), Some("acct_42"));
        assert_eq!(verify(b"s3cret", &t, 2_000), None, "expired");
        assert_eq!(verify(b"other", &t, 1_000), None, "another secret");
        assert_eq!(verify(b"", &t, 1_000), None, "no secret: nothing passes");
        let forged = t.replace("acct_42", "acct_43");
        assert_eq!(verify(b"s3cret", &forged, 1_000), None);
        let later = t.replace(".2000.", ".9999.");
        assert_eq!(verify(b"s3cret", &later, 1_000), None, "a later expiry isn't signed");
        assert_eq!(sign(b"s3cret", "has.dot", 2_000), None);
        assert_eq!(verify(b"s3cret", "v1.a.b", 0), None);
        // The relay signs this one (its own test makes the same): the two agree.
        let relay = "v1.acct_1.5000.7c11d94be57630efbfc3faad7cf28c0a46b84e7d2d54407d2344ddf3f7892b9d";
        assert_eq!(verify(b"a-bond-secret-of-thirty-two-chars!!", relay, 4_999).as_deref(), Some("acct_1"));
    }
}
