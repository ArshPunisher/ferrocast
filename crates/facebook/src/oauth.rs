//! What a Facebook sign-in gives. The sign-in itself runs on the Ferrocast relay (the Meta app
//! secret lives only there, R-46): the person approves in their browser, the relay trades the
//! code and the long-lived token and hands the Pages they manage to this install. This is that
//! hand-over, read.

use serde_json::Value;

use crate::graph::Page;

/// What a finished sign-in gives: who signed in, and the Pages they manage (each with its token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserGrant {
    pub user_id: String,
    pub user_name: String,
    pub pages: Vec<Page>,
}

impl UserGrant {
    /// Reads the relay's `facebook_connected` item: `{ user: { id, name }, pages: [{ id, name,
    /// access_token, avatar_url? }] }`. `None` when the shape is wrong. A Page without an id or a
    /// token is left out (it could not act as the Page), and a picture that is not an `https`
    /// address is dropped.
    pub fn from_relay(v: &Value) -> Option<UserGrant> {
        let user_id = v.pointer("/user/id")?.as_str()?.to_string();
        let user_name = v.pointer("/user/name").and_then(Value::as_str).unwrap_or("Facebook").to_string();
        let pages = v
            .get("pages")?
            .as_array()?
            .iter()
            .filter_map(|p| {
                let id = p.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
                let access_token = p.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty())?;
                Some(Page {
                    id: id.to_string(),
                    name: p.get("name").and_then(Value::as_str).unwrap_or("Page").to_string(),
                    access_token: access_token.to_string(),
                    avatar_url: p
                        .get("avatar_url")
                        .and_then(Value::as_str)
                        .filter(|u| u.starts_with("https://"))
                        .map(String::from),
                })
            })
            .collect();
        Some(UserGrant { user_id, user_name, pages })
    }
}
