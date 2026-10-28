//! Request guard: checks Host and Origin, answers CORS preflights.
//!
//! - Host must be 127.0.0.1/localhost on one of our ports. This blocks DNS rebinding,
//!   where an attacker's domain is pointed at 127.0.0.1.
//! - Origin, when present, must be our own or in the allowlist. Browsers always
//!   send Origin on cross-site requests and on every POST, so a random website
//!   can never print. Requests without Origin come from local programs (curl,
//!   scripts), which could reach the printer anyway.
//! - /v1/admin/* is only for local programs (no Origin header at all).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;

pub async fn guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if !host_ok(req.headers(), &st) {
        return (StatusCode::FORBIDDEN, "bad Host header").into_response();
    }

    let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return next.run(req).await;
    };

    // Admin calls come from the settings window, never from a web page.
    if req.uri().path().starts_with("/v1/admin/") {
        return (StatusCode::FORBIDDEN, "admin API is not available to web pages").into_response();
    }

    if !origin_ok(&origin, &st) {
        tracing::warn!(%origin, "blocked request from origin that is not allowed");
        st.note_blocked(&origin);
        return (
            StatusCode::FORBIDDEN,
            format!("{origin} is not allowed to print. Allow it in Z Browser Print."),
        )
            .into_response();
    }

    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        let h = resp.headers_mut();
        add_cors(h, &origin);
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        let requested = req
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .cloned()
            .unwrap_or(HeaderValue::from_static("content-type"));
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
        h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
        // Chrome's Private/Local Network Access preflight.
        if req
            .headers()
            .contains_key("access-control-request-private-network")
        {
            h.insert(
                "access-control-allow-private-network",
                HeaderValue::from_static("true"),
            );
        }
        return resp;
    }

    let mut resp = next.run(req).await;
    add_cors(resp.headers_mut(), &origin);
    resp
}

fn host_ok(headers: &HeaderMap, st: &AppState) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    std::iter::once(st.port)
        .chain(st.zebra_port)
        .flat_map(|port| {
            [
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ]
        })
        .any(|h| h.eq_ignore_ascii_case(host))
}

fn origin_ok(origin: &str, st: &AppState) -> bool {
    let own = [
        format!("http://127.0.0.1:{}", st.port),
        format!("http://localhost:{}", st.port),
    ];
    own.iter().any(|o| o == origin) || st.printers().allowed_origins.iter().any(|o| o == origin)
}

fn add_cors(h: &mut HeaderMap, origin: &str) {
    if let Ok(v) = HeaderValue::from_str(origin) {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        h.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
}
