//! Web Push to the phone page (installed on the home screen): "Signal lost", "Live again",
//! "Stream ended", so a streamer who switched apps knows. Messages are encrypted for the phone
//! as RFC 8291 says (aes128gcm) and signed with the service's own key (VAPID, RFC 8292); they
//! only ever go to the browsers' own push services.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{aead, agreement, hkdf, signature};
use serde::Deserialize;

/// Where a phone's browser takes pushes, as its page gave it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: Keys,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Keys {
    /// The browser's public key (P-256, uncompressed) and its secret, base64url.
    pub p256dh: String,
    pub auth: String,
}

/// The browsers' push services: messages go nowhere else (a page can't make the service
/// call an address of its choosing).
pub fn allowed(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("https://") else { return false };
    // Only a plain host name: a backslash, an `@` or a port could lead elsewhere.
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || !host.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
    {
        return false;
    }
    ["fcm.googleapis.com", "updates.push.services.mozilla.com", "web.push.apple.com"].contains(&host)
        || host.ends_with(".push.apple.com")
        || host.ends_with(".notify.windows.com")
}

/// The service's signing key and a client to send with.
pub struct Pusher {
    key: signature::EcdsaKeyPair,
    rng: SystemRandom,
    http: reqwest::Client,
}

impl std::fmt::Debug for Pusher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pusher").field("public", &self.public_key()).finish_non_exhaustive()
    }
}

/// A new signing key, as `BOND_VAPID_KEY` takes it (PKCS#8, base64url).
pub fn new_key() -> Option<String> {
    let rng = SystemRandom::new();
    let doc = signature::EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).ok()?;
    Some(B64.encode(doc.as_ref()))
}

impl Pusher {
    /// From `BOND_VAPID_KEY` (PKCS#8, base64url).
    pub fn new(key_b64: &str) -> Option<Self> {
        let rng = SystemRandom::new();
        let der = B64.decode(key_b64.trim()).ok()?;
        let key = signature::EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &der, &rng).ok()?;
        // A push service never redirects elsewhere: none is followed.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()?;
        Some(Pusher { key, rng, http })
    }

    /// The public key pages subscribe with (`applicationServerKey`), base64url.
    pub fn public_key(&self) -> String {
        use signature::KeyPair as _;
        B64.encode(self.key.public_key().as_ref())
    }

    /// A VAPID token for `endpoint`'s push service, good for 12 hours.
    fn vapid(&self, endpoint: &str, now_s: u64) -> Option<String> {
        let origin = endpoint.split('/').take(3).collect::<Vec<_>>().join("/");
        let head = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = B64.encode(
            serde_json::json!({ "aud": origin, "exp": now_s + 12 * 3600, "sub": "mailto:support@clearcraftlabs.com" })
                .to_string(),
        );
        let signed = format!("{head}.{claims}");
        let sig = self.key.sign(&self.rng, signed.as_bytes()).ok()?;
        Some(format!("{signed}.{}", B64.encode(sig.as_ref())))
    }

    /// Sends `message` (JSON for the page's worker) to one phone.
    pub async fn send(&self, to: &Subscription, message: &serde_json::Value) -> Result<(), String> {
        if !allowed(&to.endpoint) {
            return Err("not a push service".into());
        }
        let ua = B64.decode(&to.keys.p256dh).map_err(|_| "bad key")?;
        let auth = B64.decode(&to.keys.auth).map_err(|_| "bad secret")?;
        let body = encrypt(&ua, &auth, message.to_string().as_bytes(), &self.rng).ok_or("couldn't encrypt")?;
        let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let jwt = self.vapid(&to.endpoint, now_s).ok_or("couldn't sign")?;
        let res = self
            .http
            .post(&to.endpoint)
            .header("TTL", "60")
            .header("Urgency", "high")
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("Authorization", format!("vapid t={jwt}, k={}", self.public_key()))
            .body(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().is_success() { Ok(()) } else { Err(format!("the push service answered {}", res.status())) }
    }
}

/// An HKDF output length ring can use.
struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Option<Vec<u8>> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    let mut out = vec![0u8; len];
    prk.expand(&[info], Len(len)).ok()?.fill(&mut out).ok()?;
    Some(out)
}

/// The message encrypted for a browser (`ua_public`, `auth`): a fresh key of ours each time.
pub fn encrypt(ua_public: &[u8], auth: &[u8], payload: &[u8], rng: &SystemRandom) -> Option<Vec<u8>> {
    let mine = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, rng).ok()?;
    let as_public = mine.compute_public_key().ok()?.as_ref().to_vec();
    let theirs = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let ecdh = agreement::agree_ephemeral(mine, &theirs, |s| s.to_vec()).ok()?;
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).ok()?;
    encrypt_with(&ecdh, ua_public, &as_public, auth, &salt, payload)
}

/// RFC 8291's steps from the shared secret on (separate, so its worked example checks them).
fn encrypt_with(
    ecdh: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    auth: &[u8],
    salt: &[u8; 16],
    payload: &[u8],
) -> Option<Vec<u8>> {
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let ikm = hkdf(auth, ecdh, &key_info, 32)?;
    let cek = hkdf(salt, &ikm, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = hkdf(salt, &ikm, b"Content-Encoding: nonce\0", 12)?;
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).ok()?);
    let mut record = payload.to_vec();
    // The last (only) record's delimiter.
    record.push(2);
    key.seal_in_place_append_tag(aead::Nonce::try_assume_unique_for_key(&nonce).ok()?, aead::Aad::empty(), &mut record)
        .ok()?;
    let mut out = Vec::with_capacity(16 + 4 + 1 + as_public.len() + record.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&4096u32.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&record);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Vec<u8> {
        B64.decode(s.split_whitespace().collect::<String>()).unwrap()
    }

    /// RFC 8291 section 5 and appendix A: the same message, byte for byte.
    #[test]
    fn the_rfcs_example_encrypts_the_same() {
        let ecdh = b("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs");
        let ua = b("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let us = b("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIg Dll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        let auth = b("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = b("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let body = encrypt_with(&ecdh, &ua, &us, &auth, &salt, b"When I grow up, I want to be a watermelon").unwrap();
        let want = b("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml
            mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT
            pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN");
        assert_eq!(body, want);
    }

    #[test]
    fn a_key_signs_tokens_its_public_key_checks() {
        let p = Pusher::new(&new_key().unwrap()).unwrap();
        assert_eq!(B64.decode(p.public_key()).unwrap().len(), 65);
        let t = p.vapid("https://fcm.googleapis.com/fcm/send/abc", 1000).unwrap();
        let (signed, sig) = t.rsplit_once('.').unwrap();
        let key =
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, B64.decode(p.public_key()).unwrap());
        assert!(key.verify(signed.as_bytes(), &B64.decode(sig).unwrap()).is_ok());
        let claims: serde_json::Value =
            serde_json::from_slice(&B64.decode(signed.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert!(Pusher::new("not-a-key").is_none());
    }

    #[test]
    fn only_push_services_are_sent_to() {
        assert!(allowed("https://fcm.googleapis.com/fcm/send/x"));
        assert!(allowed("https://web.push.apple.com/QG/abc"));
        assert!(allowed("https://updates.push.services.mozilla.com/wpush/v2/x"));
        assert!(!allowed("http://fcm.googleapis.com/fcm/send/x"), "https only");
        assert!(!allowed("https://169.254.169.254/latest"));
        assert!(!allowed("https://fcm.googleapis.com@evil.example/x"));
        assert!(!allowed("https://fcm.googleapis.com.evil.example/x"));
        assert!(!allowed("https://fcm.googleapis.com:8443/x"));
        assert!(!allowed("https://10.0.0.5\\.notify.windows.com/x"), "a backslash is a slash to URL parsers");
        assert!(!allowed("https://FCM.googleapis.com/x"));
        assert!(!allowed("https:///x"));
    }

    /// A browser's side: what we encrypt for it, it can read.
    #[test]
    fn a_browser_can_read_what_is_sent() {
        let rng = SystemRandom::new();
        let ua_key = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let ua_public = ua_key.compute_public_key().unwrap().as_ref().to_vec();
        let auth = [7u8; 16];
        let body = encrypt(&ua_public, &auth, b"Signal lost", &rng).unwrap();
        let (salt, rest) = body.split_at(16);
        let idlen = rest[4] as usize;
        let as_public = &rest[5..5 + idlen];
        let mut record = rest[5 + idlen..].to_vec();
        let theirs = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public.to_vec());
        let ecdh = agreement::agree_ephemeral(ua_key, &theirs, |s| s.to_vec()).unwrap();
        let mut info = b"WebPush: info\0".to_vec();
        info.extend_from_slice(&ua_public);
        info.extend_from_slice(as_public);
        let ikm = hkdf(&auth, &ecdh, &info, 32).unwrap();
        let cek = hkdf(salt, &ikm, b"Content-Encoding: aes128gcm\0", 16).unwrap();
        let nonce = hkdf(salt, &ikm, b"Content-Encoding: nonce\0", 12).unwrap();
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
        let plain = key
            .open_in_place(aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(), aead::Aad::empty(), &mut record)
            .unwrap();
        assert_eq!(plain, b"Signal lost\x02");
    }
}
