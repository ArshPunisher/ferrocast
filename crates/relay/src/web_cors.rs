//! Cross-site calls from the website (`RELAY_WEB_URL`, plan §9 in ferrocast-web): the
//! website serves the dashboard and the sign-in pages and calls this relay's web routes with
//! the sign-in cookie. Only that one origin gets CORS, with credentials; every route keeps its
//! own Origin, cookie and CSRF checks (`accounts::same_origin`, the phone gate).

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;

/// The web routes the website may call.
fn web_route(path: &str) -> bool {
    ["/v1/account", "/v1/login", "/v1/cloud", "/v1/pro/prices", "/r/", "/c/"].iter().any(|p| path.starts_with(p))
}

fn website<'a>(st: &AppState, headers: &'a HeaderMap) -> Option<&'a HeaderValue> {
    let o = headers.get(header::ORIGIN)?;
    let web = st.remote.cfg.web.as_deref()?;
    o.to_str().ok().filter(|o| o.eq_ignore_ascii_case(web)).map(|_| o)
}

fn allow(res: &mut Response, origin: &HeaderValue) {
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    h.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
    h.append(header::VARY, HeaderValue::from_static("Origin"));
}

pub(crate) async fn layer(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let origin = if web_route(path) { website(&st, req.headers()).cloned() } else { None };
    let Some(origin) = origin else { return next.run(req).await };
    if req.method() == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        allow(&mut res, &origin);
        let h = res.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE"));
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Accept, Content-Type, X-CSRF-Token, Authorization, X-Media-Name"),
        );
        h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
        return res;
    }
    let mut res = next.run(req).await;
    allow(&mut res, &origin);
    res
}
