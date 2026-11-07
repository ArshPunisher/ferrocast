//! RIST's login (VSF TR-06-2: EAP-SRP inside EAPOL, carried in GRE), the client side as
//! librist runs it: we say hello (EAPOL-Start), the receiver asks who we are, challenges us
//! (salt, group), we answer with A, it sends B, we prove the password (M1) and it proves it
//! knew it (M2). As librist, the login proves who we are; the stream is encrypted only with a
//! shared secret (`secret=`), which we hand over under the session key when asked. The receiver
//! logs us in again every minute.

use crate::rist_srp::{Hash, SrpClient};

const EAPOL_EAP: u8 = 0;
const EAPOL_START: u8 = 1;
const REQUEST: u8 = 1;
const RESPONSE: u8 = 2;
const SUCCESS: u8 = 3;
const FAILURE: u8 = 4;
const TYPE_IDENTITY: u8 = 1;
const TYPE_SRP: u8 = 19;
const SRP_CHALLENGE: u8 = 1;
const SRP_SERVER_KEY: u8 = 2;
const SRP_SERVER_VALIDATOR: u8 = 3;
const SRP_PASSPHRASE: u8 = 0x10;
/// The EAP version we speak: 3 (SHA-256 done right; we send no passphrase, so not 4's GCM).
const VERSION: u8 = 3;

/// What a login packet led to.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Step {
    /// EAPOL packets to send back.
    pub replies: Vec<Vec<u8>>,
}

pub(crate) struct Login {
    user: String,
    password: String,
    /// The stream's shared secret, handed over (encrypted) when the receiver asks.
    secret: Option<Vec<u8>>,
    srp: Option<SrpClient>,
    /// The last login's session key.
    key: Option<Hash>,
    /// Logged in (the receiver proved itself).
    pub authenticated: bool,
}

/// An EAPOL packet carrying one EAP packet.
fn eap(code: u8, id: u8, payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() + 4) as u16;
    let mut p = vec![VERSION, EAPOL_EAP];
    p.extend_from_slice(&len.to_be_bytes());
    p.extend_from_slice(&[code, id]);
    p.extend_from_slice(&len.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// A length-prefixed field of the challenge.
fn field<'a>(b: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
    let n = usize::from(u16::from_be_bytes([*b.get(*at)?, *b.get(*at + 1)?]));
    let v = b.get(*at + 2..*at + 2 + n)?;
    *at += 2 + n;
    Some(v)
}

impl Login {
    pub(crate) fn new(user: &str, password: &str, secret: Option<&[u8]>) -> Self {
        Self {
            user: user.to_string(),
            password: password.to_string(),
            secret: secret.map(<[u8]>::to_vec),
            srp: None,
            key: None,
            authenticated: false,
        }
    }

    /// The hello that starts it (and starts it again when the receiver went quiet).
    /// The last login's session key.
    pub(crate) fn key(&self) -> Option<Hash> {
        self.key
    }

    pub(crate) fn start() -> Vec<u8> {
        vec![VERSION, EAPOL_START, 0, 0]
    }

    /// One EAPOL packet from the receiver → what to send back and any new key; Err when it
    /// refused us (in words).
    pub(crate) fn handle(&mut self, p: &[u8]) -> Result<Step, String> {
        let mut step = Step::default();
        let (Some(&version), Some(&kind)) = (p.first(), p.get(1)) else { return Ok(step) };
        if kind != EAPOL_EAP || p.len() < 8 {
            return Ok(step);
        }
        let (code, id) = (p[4], p[5]);
        let len = usize::from(u16::from_be_bytes([p[6], p[7]]));
        let Some(payload) = p.get(8..4 + len) else { return Ok(step) };
        match code {
            FAILURE => return Err("The receiver refused the username or password.".into()),
            REQUEST => {}
            _ => return Ok(step),
        }
        match (payload.first(), payload.get(1)) {
            (Some(&TYPE_IDENTITY), _) => {
                // Asked who we are (first, or again for a new key): a fresh login.
                self.srp = None;
                let mut r = vec![TYPE_IDENTITY];
                r.extend_from_slice(self.user.as_bytes());
                step.replies.push(eap(RESPONSE, id, &r));
            }
            (Some(&TYPE_SRP), Some(&SRP_CHALLENGE)) => {
                if version < 3 {
                    return Err("The receiver runs a librist too old for this login (before 0.2.16).".into());
                }
                let body = &payload[2..];
                let mut at = 0;
                let _name = field(body, &mut at).ok_or("a short challenge")?;
                let salt = field(body, &mut at).ok_or("a short challenge")?;
                let g = field(body, &mut at).ok_or("a short challenge")?;
                // A generator given: the group's modulus is the rest; none: librist's default.
                let group = (!g.is_empty()).then(|| (&body[at..], g));
                let mut secret = [0u8; 32];
                getrandom::fill(&mut secret).map_err(|e| format!("no random numbers ({e})"))?;
                let srp = SrpClient::new(group, salt, &secret)?;
                let mut r = vec![TYPE_SRP, SRP_CHALLENGE];
                r.extend_from_slice(&srp.public());
                self.srp = Some(srp);
                step.replies.push(eap(RESPONSE, id, &r));
            }
            (Some(&TYPE_SRP), Some(&SRP_SERVER_KEY)) => {
                let srp = self.srp.as_mut().ok_or("a key before the challenge")?;
                let (key, m1) = srp.handle_b(&payload[2..], &self.user, &self.password)?;
                // No flags: the session key isn't the passphrase (the secret is, if any).
                let mut r = vec![TYPE_SRP, SRP_SERVER_KEY, 0, 0, 0, 0];
                r.extend_from_slice(&m1);
                step.replies.push(eap(RESPONSE, id, &r));
                self.key = Some(key);
            }
            (Some(&TYPE_SRP), Some(&SRP_SERVER_VALIDATOR)) => {
                let srp = self.srp.as_ref().ok_or("a proof before the challenge")?;
                let m2 = payload.get(6..38).ok_or("a short proof")?;
                if !srp.verify(m2) {
                    return Err(
                        "The receiver couldn't prove it knows the password: check the username and password.".into()
                    );
                }
                self.authenticated = true;
                // As librist: a success back on the server's validator.
                step.replies.push(eap(SUCCESS, id, &[0, 0]));
            }
            (Some(&TYPE_SRP), Some(&SRP_PASSPHRASE)) => {
                // Asked for our passphrase: the secret under the session key (AES-256-CTR, the
                // counter's last byte the packet's id: librist's v3 form), or none (flag U).
                if let (true, Some(key)) = (self.authenticated, self.key) {
                    let mut r = vec![TYPE_SRP, SRP_PASSPHRASE];
                    match &self.secret {
                        Some(secret) => {
                            use ctr::cipher::{KeyIvInit, StreamCipher};
                            let mut iv = [0u8; 16];
                            iv[15] = id;
                            let mut sealed = secret.clone();
                            ctr::Ctr128BE::<aes::Aes256>::new(&key.into(), &iv.into()).apply_keystream(&mut sealed);
                            r.push(0x40);
                            r.extend_from_slice(&sealed);
                        }
                        None => r.push(0x80),
                    }
                    step.replies.push(eap(RESPONSE, id, &r));
                } else {
                    step.replies.push(eap(FAILURE, id, &[]));
                }
            }
            _ => {}
        }
        Ok(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = eap(REQUEST, id, payload);
        p[0] = 3;
        p
    }

    #[test]
    fn a_whole_login_as_librist_runs_it() {
        let mut l = Login::new("alice", "pass word", Some(b"the-secret"));
        assert_eq!(Login::start(), [3, 1, 0, 0]);
        let s = l.handle(&request(7, &[TYPE_IDENTITY])).unwrap();
        assert_eq!(&s.replies[0][8..], b"\x01alice");
        // The challenge: no name, a salt, no generator (the default group).
        let salt = [9u8; 32];
        let mut ch = vec![TYPE_SRP, SRP_CHALLENGE, 0, 0, 0, 32];
        ch.extend_from_slice(&salt);
        ch.extend_from_slice(&[0, 0]);
        let s = l.handle(&request(8, &ch)).unwrap();
        let a = s.replies[0][10..].to_vec();
        let (b, server_key, _) = crate::rist_srp::tests::server("alice", "pass word", &salt, &[0x55; 32], &a);
        let mut kb = vec![TYPE_SRP, SRP_SERVER_KEY];
        kb.extend_from_slice(&b);
        let s = l.handle(&request(9, &kb)).unwrap();
        let reply = &s.replies[0];
        assert_eq!(&reply[8..14], &[TYPE_SRP, SRP_SERVER_KEY, 0, 0, 0, 0]);
        let m1 = &reply[14..46];
        // The receiver's proof, right and wrong.
        let m2: [u8; 32] = {
            use sha2::{Digest, Sha256};
            let mut d = Sha256::new();
            d.update(&a);
            d.update(m1);
            d.update(server_key);
            d.finalize().into()
        };
        let mut v = vec![TYPE_SRP, SRP_SERVER_VALIDATOR, 0, 0, 0, 1];
        v.extend_from_slice(&m2);
        let s = l.handle(&request(10, &v)).unwrap();
        assert!(l.authenticated);
        assert_eq!(s.replies[0][4], SUCCESS);
        // Asked for the passphrase: the secret, sealed with the session key.
        let s = l.handle(&request(11, &[TYPE_SRP, SRP_PASSPHRASE])).unwrap();
        let sealed = &s.replies[0][8..];
        assert_eq!(&sealed[..3], &[TYPE_SRP, SRP_PASSPHRASE, 0x40]);
        let mut open = sealed[3..].to_vec();
        {
            use ctr::cipher::{KeyIvInit, StreamCipher};
            let mut iv = [0u8; 16];
            iv[15] = 11;
            ctr::Ctr128BE::<aes::Aes256>::new(&server_key.into(), &iv.into()).apply_keystream(&mut open);
        }
        assert_eq!(open, b"the-secret");
        // A refusal is said.
        assert!(l.handle(&eap(FAILURE, 12, &[])).is_err());
    }
}
