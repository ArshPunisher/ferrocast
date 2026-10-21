//! Tips in cloud mode (docs/CLOUD.md §7) through the real router: the cloud install made on
//! the first save (once, even with two saves at once), the engine's `TipSettings` shape with
//! no secrets, the public tip page, a Ko-fi webhook to the cloud install alerting on the
//! account's bus (never queued for a PC), and taking the page down.

use reqwest::Method;
use serde_json::{Value, json};

use super::tests::{User, World, world};

const KOFI_TOKEN: &str = "kofi-verify-token-1234";

fn page() -> Value {
    json!({
        "handle": "asha-plays", "display_name": "Asha", "message": "Thanks for watching!",
        "razorpay": null, "stripe": null,
        "kofi": { "page_url": "https://ko-fi.com/asha", "secret": KOFI_TOKEN, "test_mode": false },
    })
}

impl World {
    async fn kofi(&self, install: &str, id: &str, amount: &str) -> u16 {
        let data = json!({
            "verification_token": KOFI_TOKEN, "type": "Donation", "kofi_transaction_id": id, "amount": amount,
            "currency": "USD", "from_name": "Meera", "message": "For the mic", "is_public": true,
        });
        let body: String =
            url::form_urlencoded::Serializer::new(String::new()).append_pair("data", &data.to_string()).finish();
        self.http
            .post(format!("{}/v1/tips/kofi/{install}", self.base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    /// The bus's events (they arrive through the ingest worker, so wait a little).
    async fn events_until(&self, u: &User, n: usize) -> Value {
        for _ in 0..50 {
            let (_, v) = self.web(u, Method::GET, "/c/api/v1/events", None).await;
            if v["events"].as_array().is_some_and(|a| a.len() >= n) {
                return v["events"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        panic!("no events reached the bus");
    }
}

#[tokio::test]
async fn a_cloud_install_takes_tips_without_a_pc() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;

    // Nothing yet: the engine's empty shape, and no install made just for looking.
    let (s, v) = w.web(&u, Method::GET, "/c/api/v1/tips", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["handle"], "");
    assert_eq!(v["webhooks"], Value::Null);
    assert!(w.db.cloud_account(&u.account.id).unwrap().unwrap().install_id.is_none());

    let (s, v) = w.web(&u, Method::PUT, "/c/api/v1/tips", Some(page())).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["handle"], "asha-plays");
    assert_eq!(v["kofi"]["has_secret"], true);
    assert!(!v.to_string().contains(KOFI_TOKEN), "the secret never comes back");
    let install = w.db.cloud_account(&u.account.id).unwrap().unwrap().install_id.unwrap();
    assert_eq!(v["webhooks"]["kofi"], format!("{}/v1/tips/kofi/{install}", w.base));

    // A second save keeps the same install and, with a blank secret, the saved secret.
    let mut again = page();
    again["kofi"]["secret"] = json!("");
    again["display_name"] = json!("Asha Live");
    let (s, v) = w.web(&u, Method::PUT, "/c/api/v1/tips", Some(again)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["display_name"], "Asha Live");
    assert_eq!(w.db.cloud_account(&u.account.id).unwrap().unwrap().install_id.unwrap(), install);
    let (_, v) = w.web(&u, Method::GET, "/c/api/v1/tips", None).await;
    assert_eq!(v["display_name"], "Asha Live");

    // The public page is the usual one.
    let res = w.http.get(format!("{}/t/asha-plays", w.base)).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.text().await.unwrap().contains("Asha Live"));

    // A verified Ko-fi tip alerts on the account's bus, once, and nothing is queued.
    assert_eq!(w.kofi(&install, "kofi-tx-1", "5.00").await, 200);
    assert_eq!(w.kofi(&install, "kofi-tx-1", "5.00").await, 200, "a retry is fine");
    let events = w.events_until(&u, 1).await;
    assert_eq!(events.as_array().unwrap().len(), 1, "{events}");
    assert_eq!(events[0]["kind"]["type"], "tip");
    assert_eq!(events[0]["kind"]["actor"]["display_name"], "Meera");
    assert_eq!(events[0]["kind"]["money"]["minor"], 500);
    assert!(w.db.pending(&install, 0, 10).unwrap().is_empty(), "no PC queue for a cloud install");

    // A wrong token is refused.
    let data = json!({ "verification_token": "nope", "type": "Donation", "kofi_transaction_id": "x" }).to_string();
    let body: String = url::form_urlencoded::Serializer::new(String::new()).append_pair("data", &data).finish();
    let res = w
        .http
        .post(format!("{}/v1/tips/kofi/{install}", w.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    // Down: the page goes, the empty shape again.
    let (s, _) = w.web(&u, Method::DELETE, "/c/api/v1/tips", None).await;
    assert_eq!(s, 204);
    let (_, v) = w.web(&u, Method::GET, "/c/api/v1/tips", None).await;
    assert_eq!(v["handle"], "");
    let res = w.http.get(format!("{}/t/asha-plays", w.base)).send().await.unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn bad_pages_names_taken_and_cloud_off() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    let other = w.cloud_user("ravi@example.com").await;

    let mut bad = page();
    bad["kofi"]["page_url"] = json!("https://evil.example/pay");
    let (s, v) = w.web(&u, Method::PUT, "/c/api/v1/tips", Some(bad)).await;
    assert_eq!(s, 400, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("Ko-fi"));

    let (s, v) = w.web(&u, Method::PUT, "/c/api/v1/tips", Some(json!({ "handle": "x" }))).await;
    assert_eq!(s, 422, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("display_name"));

    assert_eq!(w.web(&u, Method::PUT, "/c/api/v1/tips", Some(page())).await.0, 200);
    let (s, v) = w.web(&other, Method::PUT, "/c/api/v1/tips", Some(page())).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("taken")));
    assert!(v["error"]["hint"].is_string());

    // Each account has its own install.
    let a = w.db.cloud_account(&u.account.id).unwrap().unwrap().install_id;
    let b = w.db.cloud_account(&other.account.id).unwrap().unwrap().install_id;
    assert!(a.is_some() && b.is_some() && a != b);

    // Cloud mode off: 409 with what to do.
    let off = w.user("off@example.com");
    let (s, v) = w.web(&off, Method::GET, "/c/api/v1/tips", None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("cloud_off")));
}

#[tokio::test]
async fn two_first_saves_at_once_make_one_install() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    let ids: Vec<String> = (0..4).map(|_| super::tips::ensure_install(&w.state, &u.account.id).unwrap()).collect();
    assert!(ids.windows(2).all(|p| p[0] == p[1]));
    // Lost a race: the spare install is dropped, the account's one stays.
    let spare = "spare-install";
    w.db.add_install(spare, "h").unwrap();
    assert!(!w.db.cloud_set_install(&u.account.id, spare).unwrap());
    w.db.cloud_drop_install(spare).unwrap();
    assert!(w.db.install_hash(spare).unwrap().is_none());
    assert!(w.db.install_hash(&ids[0]).unwrap().is_some());
}

#[tokio::test]
async fn a_tip_on_a_sleeping_accounts_page_wakes_it_and_still_alerts() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    assert_eq!(w.web(&u, Method::PUT, "/c/api/v1/tips", Some(page())).await.0, 200);
    let install = w.db.cloud_account(&u.account.id).unwrap().unwrap().install_id.unwrap();
    w.db.c().execute("UPDATE cloud_accounts SET sleeping_at = 1, last_active_at = 1", []).unwrap();
    assert_eq!(w.kofi(&install, "kofi-tx-9", "3.00").await, 200);
    for _ in 0..50 {
        if w.db.cloud_account(&u.account.id).unwrap().unwrap().sleeping_at.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(w.db.cloud_account(&u.account.id).unwrap().unwrap().sleeping_at.is_none(), "money is use");
    let events = w.events_until(&u, 1).await;
    assert_eq!(events[0]["kind"]["money"]["minor"], 300);
}
