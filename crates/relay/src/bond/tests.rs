use serde_json::Value;

use super::*;
use crate::billing::store::Synced;
use crate::{Config, Db, router};

const SECRET: &str = "a-bond-secret-of-thirty-two-chars!!";

/// The same token the Bond service's own test reads (`crates/bond`): the two agree.
#[test]
fn tokens_are_signed_as_the_bond_service_reads_them() {
    let t = sign(SECRET.as_bytes(), "acct_1", 5_000).unwrap();
    // Fixed: the Bond service's token test reads this same one.
    assert_eq!(t, "v1.acct_1.5000.7c11d94be57630efbfc3faad7cf28c0a46b84e7d2d54407d2344ddf3f7892b9d");
    assert_eq!(t, crate::bond::sign(SECRET.as_bytes(), "acct_1", 5_000).unwrap());
    assert_eq!(t.len(), "v1.acct_1.5000.".len() + 64);
    // An account is a hash of the subscription: the same each time, never the id itself.
    assert_eq!(account_of("sub_X"), account_of("sub_X"));
    assert_ne!(account_of("sub_X"), account_of("sub_Y"));
    assert!(!account_of("sub_X").contains("sub_X") && account_of("sub_X").len() == 32);
    assert!(format!("{:?}", Runtime::new(Some(SECRET.into()), None, None, None)).find(SECRET).is_none());
    assert!(Runtime::new(Some("short".into()), Some("https://b".into()), Some("h".into()), None).secret.is_none());
}

struct World {
    base: String,
    db: std::sync::Arc<Db>,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn world(bond: Runtime) -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let state = AppState::new(cfg, db).with_bond(bond);
    let db = state.db.clone();
    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, db, http: reqwest::Client::new(), _dir: dir }
}

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    fn pro(&self, id: &str, sub: &str) -> String {
        let auth = self.install(id);
        self.db.add_pro_sub(sub, "monthly", "e", "created", id).unwrap();
        let s = Synced { status: "active".into(), current_end: Some(now_ms() + 30 * 86_400_000), cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
        auth
    }

    async fn token(&self, auth: Option<&str>) -> (u16, Value) {
        let mut r = self.http.post(format!("{}/v1/pro/bond/token", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        let res = r.send().await.unwrap();
        (res.status().as_u16(), res.json().await.unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn pro_gets_a_token_and_where_bond_is() {
    let w = world(Runtime::new(
        Some(SECRET.into()),
        Some("https://bond.example/".into()),
        Some("bond-udp.example".into()),
        None,
    ))
    .await;
    assert_eq!(w.token(None).await.0, 401);
    let free = w.install("free1");
    let (s, v) = w.token(Some(&free)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")));
    let pro = w.pro("pro1", "sub_1");
    let (s, v) = w.token(Some(&pro)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["api"], "https://bond.example");
    assert_eq!((v["srtla_host"].as_str(), v["srtla_port"].as_u64()), (Some("bond-udp.example"), Some(5000)));
    let t = v["token"].as_str().unwrap();
    let exp = v["expires_ms"].as_i64().unwrap();
    assert_eq!(t, sign(SECRET.as_bytes(), &account_of("sub_1"), exp).unwrap());
    assert!(exp > now_ms() + 5 * 3_600_000);
}

#[tokio::test]
async fn without_its_settings_bond_says_it_isnt_running() {
    let w = world(Runtime::default()).await;
    let pro = w.pro("pro1", "sub_1");
    let (s, v) = w.token(Some(&pro)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("bond_off")));
}
