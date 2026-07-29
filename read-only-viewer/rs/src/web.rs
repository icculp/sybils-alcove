//! HTTP layer. Port of `alcove/web.py`.
//!
//! Read-only by construction: the only POST is /login, and all it does is set a
//! cookie.
//!
//! Static assets are compiled INTO the binary. Reading them off disk would mean
//! the "single file, no runtime" property quietly depended on a directory next
//! to it.

use std::sync::Arc;

use tiny_http::{Header, Method, Request, Response, Server};

use crate::collect::Collector;
use crate::store;
use crate::config::{Config, COOKIE};

const INDEX: &str = include_str!("../../static/index.html");
const LOGIN: &str = include_str!("../../static/login.html");
const APP_JS: &str = include_str!("../../static/app.js");
const APP_CSS: &str = include_str!("../../static/app.css");
const ACTIVITY: &str = include_str!("../../static/activity.html");
const ACTIVITY_JS: &str = include_str!("../../static/activity.js");
const ACTIVITY_CSS: &str = include_str!("../../static/activity.css");
const SPILL: &str = include_str!("../../static/spill.html");
const SPILL_JS: &str = include_str!("../../static/spill.js");
const SPILL_CSS: &str = include_str!("../../static/spill.css");

const HTML: &str = "text/html; charset=utf-8";
const JSON: &str = "application/json";

fn header(key: &str, value: &str) -> Header {
    Header::from_bytes(key.as_bytes(), value.as_bytes()).expect("static header")
}

/// Constant-time compare so a wrong guess leaks no timing signal.
fn token_ok(supplied: &str, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let (a, b) = (supplied.as_bytes(), expected.as_bytes());
    // Compare the full length of both regardless of mismatch position.
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// Bearer header for scripts, cookie for browsers.
///
/// Deliberately no `?token=` support: a secret in a URL lands in browser
/// history, screenshots, referers, and shell history.
fn supplied_token(request: &Request) -> String {
    let mut cookie_value = String::new();
    for h in request.headers() {
        let field = h.field.as_str().as_str().to_ascii_lowercase();
        if field == "authorization" {
            if let Some(rest) = h.value.as_str().strip_prefix("Bearer ") {
                return rest.trim().to_string();
            }
        } else if field == "cookie" {
            for part in h.value.as_str().split(';') {
                let part = part.trim();
                if let Some(v) = part.strip_prefix(&format!("{COOKIE}=")) {
                    cookie_value = v.to_string();
                }
            }
        }
    }
    cookie_value
}

fn send(request: Request, body: Vec<u8>, ctype: &str, status: u16, cookie: Option<&str>) {
    let mut response = Response::from_data(body).with_status_code(status);
    response.add_header(header("Content-Type", ctype));
    response.add_header(header("Cache-Control", "no-store"));
    // Nothing here loads a remote resource; say so rather than relying on it.
    response.add_header(header(
        "Content-Security-Policy",
        "default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'",
    ));
    if let Some(value) = cookie {
        // HttpOnly so page scripts cannot read it back out; SameSite=Strict so
        // another origin cannot ride the cookie.
        response.add_header(header(
            "Set-Cookie",
            &format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age=604800"),
        ));
    }
    let _ = request.respond(response);
}

fn login_page(request: Request, error: &str, status: u16) {
    let marker =
        if error.is_empty() { String::new() } else { format!("<p class=\"err\">{error}</p>") };
    let body = LOGIN.replace("__ERR__", &marker);
    send(request, body.into_bytes(), HTML, status, None);
}

fn asset(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "app.js" => Some((APP_JS, "text/javascript; charset=utf-8")),
        "app.css" => Some((APP_CSS, "text/css; charset=utf-8")),
        "activity.js" => Some((ACTIVITY_JS, "text/javascript; charset=utf-8")),
        "activity.css" => Some((ACTIVITY_CSS, "text/css; charset=utf-8")),
        "spill.js" => Some((SPILL_JS, "text/javascript; charset=utf-8")),
        "spill.css" => Some((SPILL_CSS, "text/css; charset=utf-8")),
        _ => None,
    }
}

pub fn serve(cfg: Config) -> i32 {
    if !cfg.claude_root.is_dir() && !cfg.codex_root.is_dir() {
        eprintln!(
            "no transcripts found under {} or {}",
            cfg.claude_root.display(),
            cfg.codex_root.display()
        );
        return 1;
    }
    // Fail closed: this page shows task prompts, so a non-loopback bind without
    // a token is a mistake, not a default worth honouring.
    if !cfg.is_local_bind() && cfg.token.is_empty() {
        eprintln!(
            "refusing to serve {}:{} without ALCOVE_TOKEN.\n  \
             generate one:  head -c32 /dev/urandom | base64\n  \
             or bind loopback: ALCOVE_BIND=127.0.0.1",
            cfg.bind, cfg.port
        );
        return 2;
    }

    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot bind {addr}: {e}");
            return 3;
        }
    };
    println!("alcove (rust): http://{addr}");
    println!(
        "  auth:   {}",
        if cfg.is_local_bind() { "loopback (none)" } else { "token required" }
    );

    let collector = Arc::new(Collector::new(cfg.clone()));
    let server = Arc::new(server);
    let workers = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4).min(8);

    let mut handles = Vec::new();
    for _ in 0..workers {
        let server = Arc::clone(&server);
        let collector = Arc::clone(&collector);
        let cfg = cfg.clone();
        handles.push(std::thread::spawn(move || {
            for request in server.incoming_requests() {
                handle(request, &collector, &cfg);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    0
}

fn handle(request: Request, collector: &Collector, cfg: &Config) {
    let url = request.url().to_string();
    let route = url.split('?').next().unwrap_or("/").to_string();

    if request.method() == &Method::Post {
        if route != "/login" {
            send(request, b"not found".to_vec(), "text/plain", 404, None);
            return;
        }
        let mut body = String::new();
        let mut req = request;
        if std::io::Read::read_to_string(&mut req.as_reader(), &mut body).is_err() {
            login_page(req, "bad request", 400);
            return;
        }
        let supplied = body
            .split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .map(|v| v.replace('+', " "))
            .unwrap_or_default();
        if !token_ok(&supplied, &cfg.token) {
            login_page(req, "rejected", 401);
            return;
        }
        // 303 so the browser re-requests with GET and the POST body is not
        // replayed on refresh.
        let mut response = Response::from_data(Vec::new()).with_status_code(303);
        response.add_header(header("Location", "/"));
        response.add_header(header(
            "Set-Cookie",
            &format!("{COOKIE}={supplied}; Path=/; HttpOnly; SameSite=Strict; Max-Age=604800"),
        ));
        let _ = req.respond(response);
        return;
    }

    // Loopback is trusted; anything wider requires the shared secret.
    if !cfg.is_local_bind() && !token_ok(&supplied_token(&request), &cfg.token) {
        if route.starts_with("/api/") {
            send(request, br#"{"error":"unauthorized"}"#.to_vec(), JSON, 401, None);
        } else {
            login_page(request, "", 401);
        }
        return;
    }

    match route.as_str() {
        "/" | "/index.html" | "/login" => {
            send(request, INDEX.as_bytes().to_vec(), HTML, 200, None)
        }
        "/api/sessions" => {
            let body = serde_json::to_vec(&collector.cached()).unwrap_or_default();
            send(request, body, JSON, 200, None)
        }
        "/api/activity" => {
            let body = serde_json::to_vec(&activity(&url)).unwrap_or_default();
            send(request, body, JSON, 200, None)
        }
        "/activity" => send(request, ACTIVITY.as_bytes().to_vec(), HTML, 200, None),
        "/api/spill" => {
            let q = |k: &str| -> String {
                url.split('?').nth(1).and_then(|qs| {
                    qs.split('&').find_map(|kv| {
                        kv.strip_prefix(&format!("{k}=")).map(|v| {
                            percent_decode(v)
                        })
                    })
                }).unwrap_or_default()
            };
            let num = |k: &str, d: i64, lo: i64, hi: i64| -> i64 {
                q(k).parse().unwrap_or(d).clamp(lo, hi)
            };
            let session = q("session");
            let agent = q("agent");
            let target = collector.resolve(&session, &agent);
            let body = serde_json::to_vec(&crate::spill::spill(
                target,
                num("minutes", 0, 0, 1440),
                num("limit", 300, 1, 2000) as usize,
            ))
            .unwrap_or_default();
            send(request, body, JSON, 200, None)
        }
        "/spill" => send(request, SPILL.as_bytes().to_vec(), HTML, 200, None),
        _ if route.starts_with("/static/") => {
            let name = &route["/static/".len()..];
            match asset(name) {
                Some((body, ctype)) => send(request, body.as_bytes().to_vec(), ctype, 200, None),
                None => send(request, b"not found".to_vec(), "text/plain", 404, None),
            }
        }
        _ => send(request, b"not found".to_vec(), "text/plain", 404, None),
    }
}

/// Daily history from the store — not bounded by the tail window the live view
/// reads, which is the whole reason the store exists.
///
/// A missing or unreadable store is an EXPECTED state on a fresh install, not a
/// fault: say which it is and how to fix it, rather than returning an error the
/// page cannot act on.
fn activity(url: &str) -> serde_json::Value {
    let days: i64 = url
        .split('?')
        .nth(1)
        .and_then(|q| {
            q.split('&').find_map(|kv| kv.strip_prefix("days=").map(|v| v.to_string()))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        .clamp(1, 365);
    let db = store::db_path().display().to_string();
    let conn = match store::connect(false) {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({
                "days": days, "rows": [], "totals": {}, "db": db,
                "unavailable": e.to_string(),
                "hint": "populate it with:  alcove --ingest-only",
            })
        }
    };
    match (store::daily_activity(&conn, days), store::totals(&conn)) {
        (Ok(rows), Ok(totals)) => {
            serde_json::json!({"days": days, "rows": rows, "totals": totals, "db": db})
        }
        (Err(e), _) | (_, Err(e)) => serde_json::json!({
            "days": days, "rows": [], "totals": {}, "db": db,
            "unavailable": e.to_string(),
            "hint": "populate it with:  alcove --ingest-only",
        }),
    }
}

/// Minimal percent-decoding for query values. Session ids are hex-and-dashes, so
/// this exists for the `%2F`-style escapes a browser may still send rather than
/// for general URL handling.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.replace('+', " ").into_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}
