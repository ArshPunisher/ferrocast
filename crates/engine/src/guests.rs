//! Guest cameras, the engine's part: guest links (invites) made and removed through the relay,
//! and a guest's offer, which arrives down the relay link, answered by the Studio (the desktop's
//! `StudioHost::guest_join`). The relay only introduces them: the camera and mic go straight from
//! the guest's browser to this PC (crates/guest).
//!
//! Links are kept in the secret store (each one lets its holder ask to join) and the relay link is
//! kept open while there are any, so a guest can always reach this PC while Ferrocast is open.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Engine, EngineError, now};

const INVITES: &str = "guest-invites";
/// One change to the list at a time (two made at once both kept).
static CHANGING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Links kept at most (the relay holds at most 20 per install).
const MAX_INVITES: usize = 20;
/// The longest label a link may have.
const MAX_LABEL: usize = 40;

/// One guest link.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuestInvite {
    /// Short and stable, to name it in the API (never the token).
    pub id: String,
    /// The streamer's own name for it ("Co-host", "Tuesday guests").
    pub label: String,
    /// The page the guest opens.
    pub url: String,
    pub created_ms: i64,
    pub expires_ms: i64,
    /// The relay's secret for this link (the part after `#`); never sent to the dashboard.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
}

impl GuestInvite {
    /// The same without its token, for the dashboard (the URL holds it, for copying).
    fn public(&self) -> Value {
        json!({ "id": self.id, "label": self.label, "url": self.url, "created_ms": self.created_ms, "expires_ms": self.expires_ms })
    }
}

impl Engine {
    fn guest_invites(&self) -> Vec<GuestInvite> {
        let now = now().0;
        self.inner
            .secrets
            .get(INVITES)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str::<Vec<GuestInvite>>(&j).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|i| i.expires_ms > now)
            .collect()
    }

    fn save_guest_invites(&self, list: &[GuestInvite]) -> Result<(), EngineError> {
        let j = serde_json::to_string(list).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        self.inner.secrets.set(INVITES, &j).map_err(|e| EngineError::Unavailable(e.to_string()))
    }

    /// At start: the relay link opens when there are guest links, so guests can reach this PC.
    pub(crate) fn guests_at_start(&self) {
        if self.relay_url().is_ok() && !self.guest_invites().is_empty() {
            self.ensure_relay_link();
        }
    }

    /// `GET /studio/guests`: the links and who's here.
    pub async fn guests_view(&self) -> Result<Value, EngineError> {
        let invites: Vec<Value> = self.guest_invites().iter().map(GuestInvite::public).collect();
        let here = match self.studio_host() {
            Ok(h) => h.guests().await.unwrap_or_else(|_| json!([])),
            Err(_) => json!([]),
        };
        Ok(json!({ "invites": invites, "guests": here }))
    }

    /// `POST /studio/guests/invites` `{ label }`: a new link, made at the relay.
    pub async fn guest_invite_create(&self, label: &str) -> Result<Value, EngineError> {
        let label: String = label.trim().chars().filter(|c| !c.is_control()).take(MAX_LABEL).collect();
        let label = if label.is_empty() { "Guest".to_string() } else { label };
        let _one = CHANGING.lock().await;
        let mut list = self.guest_invites();
        if list.len() >= MAX_INVITES {
            return Err(EngineError::Invalid(format!(
                "You have {MAX_INVITES} guest links already. Remove one you no longer use first."
            )));
        }
        let url = self.relay_url()?;
        let auth = self.relay_auth().await?;
        let res = self
            .relay_http()
            .post(format!("{url}/v1/guests/invites"))
            .bearer_auth(&auth)
            .send()
            .await
            .map_err(|e| EngineError::Unavailable(format!("Couldn't reach the Ferrocast relay: {e}")))?;
        let ok = res.status().is_success();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        let (Some(token), Some(page), true) = (v["token"].as_str(), v["url"].as_str(), ok) else {
            return Err(EngineError::Unavailable(
                v.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("The relay couldn't make a guest link.")
                    .into(),
            ));
        };
        let invite = GuestInvite {
            // The token's first characters: short, and nothing a guest could use.
            id: token.chars().filter(char::is_ascii_alphanumeric).take(8).collect(),
            label,
            url: page.to_string(),
            created_ms: now().0,
            expires_ms: v["expires_at"].as_i64().unwrap_or(now().0 + 30 * 86_400_000),
            token: token.to_string(),
        };
        list.push(invite.clone());
        self.save_guest_invites(&list)?;
        self.ensure_relay_link();
        Ok(invite.public())
    }

    /// `DELETE /studio/guests/invites/{id}`: the link stops working, here and at the relay.
    pub async fn guest_invite_delete(&self, id: &str) -> Result<(), EngineError> {
        let _one = CHANGING.lock().await;
        let mut list = self.guest_invites();
        let Some(at) = list.iter().position(|i| i.id == id) else {
            return Err(EngineError::Missing {
                code: "guest_link_missing",
                message: "That guest link isn't there any more.",
                hint: "Refresh the list.",
            });
        };
        let gone = list.remove(at);
        // Kept out here first: the link stops working on this PC even if the relay is away.
        self.save_guest_invites(&list)?;
        if let (Ok(url), Ok(auth)) = (self.relay_url(), self.relay_auth().await) {
            let res = self
                .relay_http()
                .delete(format!("{url}/v1/guests/invites/{}", gone.token))
                .bearer_auth(&auth)
                .send()
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "the relay didn't hear that a guest link was removed; this PC refuses it anyway");
            }
        }
        Ok(())
    }

    /// `DELETE /studio/guests/{name}`: hangs up on a guest (their source stays, empty).
    pub async fn guest_hang_up(&self, name: &str) -> Result<(), EngineError> {
        self.studio_host()?.guest_leave(name.to_string()).await.map_err(EngineError::Unavailable)
    }

    /// A guest's offer from the relay (a `guest_offer` frame) → the `guest_answer` frame to send
    /// back: the Studio's answer, or why not.
    pub(crate) async fn guest_offer(&self, frame: &Value) -> Value {
        let reply = |body: Value| {
            let mut r = json!({ "type": "guest_answer", "req": frame["req"] });
            if let (Some(r), Some(b)) = (r.as_object_mut(), body.as_object()) {
                r.extend(b.clone());
            }
            r
        };
        let token = frame["token"].as_str().unwrap_or("");
        // Only links this PC still has (one removed while the relay was away stays refused).
        if token.is_empty() || !self.guest_invites().iter().any(|i| i.token == token) {
            return reply(json!({ "error": "This guest link was removed. Ask the streamer for a new one." }));
        }
        let name: String =
            frame["name"].as_str().unwrap_or("").trim().chars().filter(|c| !c.is_control()).take(MAX_LABEL).collect();
        let name = if name.is_empty() { "Guest".to_string() } else { name };
        let host = match self.studio_host() {
            Ok(h) => h,
            Err(_) => return reply(json!({ "error": "The streamer's Studio isn't open." })),
        };
        match host.guest_join(name, frame["offer"].to_string()).await {
            Ok(answer) => match serde_json::from_str::<Value>(&answer) {
                Ok(a) => reply(json!({ "answer": a })),
                Err(_) => reply(json!({ "error": "The streamer's Studio couldn't answer." })),
            },
            Err(e) => reply(json!({ "error": e })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dashboard_never_sees_a_token() {
        let i = GuestInvite {
            id: "abc".into(),
            label: "Co-host".into(),
            url: "https://x/app/guest.html#tok".into(),
            created_ms: 1,
            expires_ms: 2,
            token: "tok".into(),
        };
        let v = i.public();
        assert!(v.get("token").is_none());
        assert_eq!(v["label"], "Co-host");
    }
}
