//! Session auth (SPEC v1 §2): cookie session + CSRF double-submit.
//!
//! Replaces the previous single-password `X-Password` gate. A single admin
//! account (`web.username` / `web.password` in the config — plaintext file
//! storage is an accepted device dialect, SPEC appendix A3) logs in via
//! `POST /api/auth/login` and receives:
//!
//! - `session=<token>` cookie — `HttpOnly; Path=/; SameSite=Strict` (24 h;
//!   in-memory store, so a process restart signs everyone out);
//! - `csrf-token=<token>` cookie — readable by JS for the double-submit
//!   pattern: every state-changing `/api/*` request must echo it in the
//!   `X-CSRF-Token` header (login/setup/logout are exempt).
//!
//! First boot (`web.password` empty): every gated endpoint answers
//! `401 unauthorized`; `GET /api/auth/me` reports `503 setup_required` and
//! `POST /api/auth/setup` creates the admin and signs in.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::api::{err_env, ok_env, AppState};

/// Cookie / header names (SPEC §2).
pub const SESSION_COOKIE: &str = "session";
pub const CSRF_COOKIE: &str = "csrf-token";
pub const CSRF_HEADER: &str = "x-csrf-token";

/// Session lifetime (SPEC §2: 24 h).
const SESSION_TTL: Duration = Duration::from_secs(24 * 3600);
/// Cleanup interval for expired sessions.
const SESSION_SWEEP: Duration = Duration::from_secs(300);
/// Failed-login lockout: 5 failures → 60 s, doubling (SPEC §2 recommendation).
const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_LOCK_BASE_SECS: u64 = 60;

/// `/api/*` paths reachable without a session (SPEC §1–2).
const PUBLIC_API_PATHS: &[&str] = &[
    "/api/health",
    "/api/auth/me",
    "/api/auth/login",
    "/api/auth/setup",
    "/api/auth/logout",
];
/// State-changing auth endpoints exempt from CSRF (they establish the session).
const CSRF_EXEMPT: &[&str] = &["/api/auth/login", "/api/auth/setup", "/api/auth/logout"];

/// Mask shown in `GET /api/config` for any set password (`""` when unset).
pub const PASSWORD_MASK: &str = "****";

// ---------------------------------------------------------------------------
// Session store
// ---------------------------------------------------------------------------

/// One live session: who, its CSRF token, and when it expires.
struct Session {
    username: String,
    csrf: String,
    expires: Instant,
}

/// Session store. In-memory by default; `with_persistence` round-trips it
/// through a JSON file so deliberate restarts (§5.1) keep browsers signed
/// in (SPEC 附录A #10) — logout and password reset still clear the file.
pub struct SessionStore {
    inner: Mutex<SessionsInner>,
    path: Option<PathBuf>,
}

struct SessionsInner {
    sessions: HashMap<String, Session>,
    /// Per-username login failures: (count, locked_until)
    failures: HashMap<String, (u32, Option<Instant>)>,
}

/// On-disk shape (web-sessions.json). `Instant` is process-local, so the
/// expiry crosses the boundary as wall-clock unix seconds.
#[derive(Serialize, Deserialize)]
struct SessionRecord {
    username: String,
    csrf: String,
    expires_at: u64,
}

#[derive(Serialize, Deserialize)]
struct SessionsDoc {
    version: u32,
    sessions: HashMap<String, SessionRecord>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SessionsInner {
                sessions: HashMap::new(),
                failures: HashMap::new(),
            }),
            path: None,
        }
    }

    /// Load (or create) a store backed by `path`. Expired entries are
    /// pruned at load; a missing or corrupt file degrades to an empty
    /// store — sessions are a cache, never worth failing startup over.
    #[must_use]
    pub fn with_persistence(path: impl AsRef<Path>) -> Self {
        let mut store = Self::new();
        if let Ok(raw) = std::fs::read(path.as_ref()) {
            if let Ok(doc) = serde_json::from_slice::<SessionsDoc>(&raw) {
                let now = unix_now();
                let sessions = doc
                    .sessions
                    .into_iter()
                    .filter_map(|(token, rec)| {
                        if rec.expires_at <= now {
                            return None;
                        }
                        Some((
                            token,
                            Session {
                                username: rec.username,
                                csrf: rec.csrf,
                                expires: Instant::now() + Duration::from_secs(rec.expires_at - now),
                            },
                        ))
                    })
                    .collect();
                store.lock().sessions = sessions;
            }
        }
        store.path = Some(path.as_ref().to_path_buf());
        store
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionsInner> {
        // std Mutex over a tiny critical section; a panic while holding it
        // would abort the process anyway (poisoning is not a concern here).
        self.inner.lock().expect("session store lock")
    }

    /// Snapshot the store to disk (best effort; caller holds the lock).
    fn persist(inner: &SessionsInner, path: &Path) {
        let doc = SessionsDoc {
            version: 1,
            sessions: inner
                .sessions
                .iter()
                .map(|(token, s)| {
                    (
                        token.clone(),
                        SessionRecord {
                            username: s.username.clone(),
                            csrf: s.csrf.clone(),
                            expires_at: unix_now().saturating_add(
                                s.expires
                                    .saturating_duration_since(Instant::now())
                                    .as_secs(),
                            ),
                        },
                    )
                })
                .collect(),
        };
        let Ok(data) = serde_json::to_vec(&doc) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            #[cfg(unix)]
            let mut f = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)?
            };
            #[cfg(not(unix))]
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)
        };
        let _ = write();
    }

    /// Create a session, returning `(session_token, csrf_token)`.
    pub fn create(&self, username: &str) -> (String, String) {
        let session_token = token();
        let csrf = token();
        let mut inner = self.lock();
        inner.sessions.insert(
            session_token.clone(),
            Session {
                username: username.to_string(),
                csrf: csrf.clone(),
                expires: Instant::now() + SESSION_TTL,
            },
        );
        if let Some(p) = &self.path {
            Self::persist(&inner, p);
        }
        (session_token, csrf)
    }

    /// Validate a session token, returning `(username, csrf)` when alive.
    pub fn validate(&self, token: &str) -> Option<(String, String)> {
        let mut inner = self.lock();
        let s = inner.sessions.get(token)?;
        if s.expires <= Instant::now() {
            inner.sessions.remove(token);
            return None;
        }
        Some((s.username.clone(), s.csrf.clone()))
    }

    /// Drop one session (logout).
    pub fn remove(&self, token: &str) {
        let mut inner = self.lock();
        inner.sessions.remove(token);
        if let Some(p) = &self.path {
            Self::persist(&inner, p);
        }
    }

    /// Drop every session (password reset).
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.sessions.clear();
        if let Some(p) = &self.path {
            Self::persist(&inner, p);
        }
    }

    /// Number of live sessions (tests / observability).
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().sessions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove expired sessions (periodic sweep).
    pub fn sweep(&self) {
        let now = Instant::now();
        self.lock().sessions.retain(|_, s| s.expires > now);
    }

    /// `true` while a username is locked out after repeated failures.
    /// An expired lock resets the counter (fresh start).
    fn is_locked(&self, username: &str) -> bool {
        let mut inner = self.lock();
        let Some((count, until)) = inner.failures.get(username).cloned() else {
            return false;
        };
        match until {
            Some(t) if t > Instant::now() => true,
            Some(_) => {
                inner.failures.remove(username);
                false
            }
            None => count >= LOGIN_MAX_FAILURES,
        }
    }

    /// Record a failed login for the username, engaging the lock when due.
    fn record_failure(&self, username: &str) {
        let mut inner = self.lock();
        let entry = inner
            .failures
            .entry(username.to_string())
            .or_insert((0, None));
        entry.0 += 1;
        if entry.0 >= LOGIN_MAX_FAILURES {
            let secs = LOGIN_LOCK_BASE_SECS * (1 << (entry.0 - LOGIN_MAX_FAILURES)).min(8);
            entry.1 = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

    fn clear_failures(&self, username: &str) {
        self.lock().failures.remove(username);
    }
}

fn token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison so credential checks do not leak value length or
/// content through response timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

/// Build the `Set-Cookie` pair issued on login/setup/reset.
fn session_cookies(token: &str, csrf: &str) -> [(String, String); 2] {
    [
        (
            header::SET_COOKIE.to_string(),
            format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400"),
        ),
        (
            header::SET_COOKIE.to_string(),
            format!("{CSRF_COOKIE}={csrf}; Path=/; SameSite=Strict"),
        ),
    ]
}

/// Parse the `session` cookie out of a request's Cookie header.
fn session_cookie(req: &Request<Body>) -> Option<String> {
    let raw = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(SESSION_COOKIE) {
            let value = value.strip_prefix('=')?;
            return Some(value.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Auth gate middleware
// ---------------------------------------------------------------------------

fn is_write(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    )
}

/// Session gate + CSRF check for `/api/*` (SPEC §2).
///
/// Public paths pass through. Everything else needs a live session cookie;
/// writes additionally need the CSRF double-submit pair to match. The ONVIF
/// proxy (`/onvif/*`) keeps its own WS-Security auth and is not gated.
pub async fn auth_gate(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if !path.starts_with("/api/") || PUBLIC_API_PATHS.contains(&path) {
        return next.run(req).await;
    }

    let Some(token) = session_cookie(&req) else {
        return err_env(StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };
    let Some((_user, csrf)) = state.sessions.validate(&token) else {
        return err_env(StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };

    if is_write(req.method()) && !CSRF_EXEMPT.contains(&path) {
        let supplied = req
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !constant_time_eq(supplied, &csrf) {
            return err_env(StatusCode::UNAUTHORIZED, "csrf mismatch").into_response();
        }
    }

    next.run(req).await
}

/// Append the session + CSRF cookies onto a response (append, not insert —
/// both share the `Set-Cookie` header name).
fn issue_cookies(resp: &mut Response, token: &str, csrf: &str) {
    for (name, value) in session_cookies(token, csrf) {
        resp.headers_mut().append(
            name.parse::<header::HeaderName>().unwrap(),
            value.parse().unwrap(),
        );
    }
}

// ---------------------------------------------------------------------------
// Auth handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct Credentials {
    pub username: Option<String>,
    pub password: String,
}

/// `GET /api/auth/me` — auth state probe: 200 signed in / 401 login /
/// 503 setup_required (SPEC §2). Public (not session-gated) so the UI can
/// resolve its initial state before any credential exists.
pub async fn me_handler(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let cfg = state.config.read().await;
    if cfg.web.password.is_empty() {
        return err_env(StatusCode::SERVICE_UNAVAILABLE, "setup_required").into_response();
    }
    drop(cfg);

    if let Some(token) = session_cookie(&req) {
        if let Some((user, _csrf)) = state.sessions.validate(&token) {
            return ok_env(serde_json::json!({"username": user, "role": "admin"})).into_response();
        }
    }
    err_env(StatusCode::UNAUTHORIZED, "not signed in").into_response()
}

/// `POST /api/auth/setup` — first-boot admin creation (SPEC §2).
pub async fn setup(State(state): State<Arc<AppState>>, Json(creds): Json<Credentials>) -> Response {
    let username = creds.username.unwrap_or_else(|| "admin".to_string());
    if username.is_empty() || creds.password.len() < 8 {
        return err_env(
            StatusCode::BAD_REQUEST,
            "username required and password >= 8 chars",
        )
        .into_response();
    }

    let mut cfg = state.config.write().await;
    if !cfg.web.password.is_empty() {
        return err_env(StatusCode::BAD_REQUEST, "already configured").into_response();
    }
    cfg.web.username = username.clone();
    cfg.web.password = creds.password;
    let snapshot = cfg.clone();
    drop(cfg);
    if let Err(e) = super::api::persist_config(&state, &snapshot).await {
        return e.into_response();
    }

    let (token, csrf) = state.sessions.create(&username);
    let mut resp = ok_env(serde_json::json!({"username": username})).into_response();
    issue_cookies(&mut resp, &token, &csrf);
    resp
}

/// `POST /api/auth/login` (SPEC §2). Accepts any username when the stored one
/// is empty (migration from the pre-SPEC single-password model).
pub async fn login(State(state): State<Arc<AppState>>, Json(creds): Json<Credentials>) -> Response {
    let cfg = state.config.read().await;
    if cfg.web.password.is_empty() {
        return err_env(StatusCode::SERVICE_UNAVAILABLE, "setup_required").into_response();
    }
    let stored_user = if cfg.web.username.is_empty() {
        "admin".to_string()
    } else {
        cfg.web.username.clone()
    };
    let stored_pass = cfg.web.password.clone();
    drop(cfg);

    if state.sessions.is_locked(&stored_user) {
        return err_env(StatusCode::TOO_MANY_REQUESTS, "locked, try again later").into_response();
    }

    // SPEC §2: empty/omitted username defaults to "admin" (single-admin login form).
    let sent_user = creds
        .username
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or("admin");
    let user_ok = constant_time_eq(sent_user, &stored_user);
    if !user_ok || !constant_time_eq(&creds.password, &stored_pass) {
        state.sessions.record_failure(&stored_user);
        return err_env(StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
    }
    state.sessions.clear_failures(&stored_user);

    let (token, csrf) = state.sessions.create(&stored_user);
    let mut resp = ok_env(serde_json::json!({"username": stored_user})).into_response();
    issue_cookies(&mut resp, &token, &csrf);
    resp
}

/// `POST /api/auth/logout` — drop the session, clear the cookie.
pub async fn logout(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    if let Some(token) = session_cookie(&req) {
        state.sessions.remove(&token);
    }
    let mut resp = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        format!("{SESSION_COOKIE}=; Path=/; Max-Age=0")
            .parse()
            .unwrap(),
    );
    resp
}

#[derive(Deserialize)]
pub struct ResetRequest {
    pub old_password: String,
    pub new_password: String,
}

/// `POST /api/auth/reset` — change the admin password, invalidating every
/// session (SPEC §2).
pub async fn reset(State(state): State<Arc<AppState>>, Json(req): Json<ResetRequest>) -> Response {
    if req.new_password.len() < 8 {
        return err_env(StatusCode::BAD_REQUEST, "password >= 8 chars").into_response();
    }
    let mut cfg = state.config.write().await;
    if !constant_time_eq(&req.old_password, &cfg.web.password) {
        drop(cfg);
        return err_env(StatusCode::UNAUTHORIZED, "wrong password").into_response();
    }
    cfg.web.password = req.new_password;
    let snapshot = cfg.clone();
    let username = if cfg.web.username.is_empty() {
        "admin".to_string()
    } else {
        cfg.web.username.clone()
    };
    drop(cfg);
    if let Err(e) = super::api::persist_config(&state, &snapshot).await {
        return e.into_response();
    }
    state.sessions.clear();

    let (token, csrf) = state.sessions.create(&username);
    let mut resp = ok_env(serde_json::json!({"username": username})).into_response();
    issue_cookies(&mut resp, &token, &csrf);
    resp
}

/// Spawn the periodic expired-session sweeper.
pub fn spawn_sweeper(sessions: Arc<SessionStore>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SESSION_SWEEP).await;
            sessions.sweep();
        }
    });
}

// ---------------------------------------------------------------------------
// Config masking (SPEC §5: passwords are masked, never echoed)
// ---------------------------------------------------------------------------

/// Sections of `Config` that carry a `password` field — kept in one place so
/// mask/unmask can never drift.
pub const PASSWORD_SECTIONS: &[&str] = &["web", "rtsp", "onvif", "gb28181"];

/// Mask every set password in a serialized config value; empty passwords stay
/// `""` so the UI can distinguish "unset" from "set".
#[must_use]
pub fn mask_passwords(mut value: serde_json::Value) -> serde_json::Value {
    for section in PASSWORD_SECTIONS {
        let Some(sec) = value.get_mut(*section) else {
            continue;
        };
        let Some(p) = sec.get_mut("password") else {
            continue;
        };
        if p.as_str().is_some_and(|s| !s.is_empty()) {
            *p = serde_json::Value::String(PASSWORD_MASK.to_string());
        }
    }
    value
}

/// Restore `"****"`-masked password fields in an incoming config update from
/// the currently stored values, so a client that GETs (masked) and PUTs back
/// the same document does not wipe the stored secrets.
pub fn unmask_passwords(value: &mut serde_json::Value, current: &serde_json::Value) {
    for section in PASSWORD_SECTIONS {
        let Some(masked) = value
            .get(section)
            .and_then(|s| s.get("password"))
            .and_then(|p| p.as_str())
        else {
            continue;
        };
        if masked == PASSWORD_MASK {
            let stored = current
                .get(section)
                .and_then(|s| s.get("password"))
                .cloned()
                .unwrap_or(serde_json::Value::String(String::new()));
            if let Some(p) = value.get_mut(section).and_then(|s| s.get_mut("password")) {
                *p = stored;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq("abc123", "abc123"));
        assert!(!constant_time_eq("abc123", "abc124"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn session_lifecycle() {
        let store = SessionStore::new();
        let (token, csrf) = store.create("admin");
        assert_eq!(store.len(), 1);
        let (user, c) = store.validate(&token).unwrap();
        assert_eq!(user, "admin");
        assert_eq!(c, csrf);
        store.remove(&token);
        assert!(store.validate(&token).is_none());
        assert!(store.is_empty());
    }

    /// A session created in one store validates in a store reloaded from the
    /// same path (process-restart simulation) — restarts must not sign out
    /// every browser (SPEC 附录A #10).
    #[test]
    fn sessions_survive_store_reload() {
        let path = std::env::temp_dir().join(format!(
            "rs-sessions-reload-{}-{}.json",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);
        let a = SessionStore::with_persistence(&path);
        let (token, csrf) = a.create("admin");

        let b = SessionStore::with_persistence(&path);
        let (user, c) = b.validate(&token).expect("session survives reload");
        assert_eq!(user, "admin");
        assert_eq!(c, csrf);
        let _ = std::fs::remove_file(&path);
    }

    /// Logout and clear write through — a restart must not resurrect
    /// signed-out sessions.
    #[test]
    fn logout_and_clear_persist() {
        let path = std::env::temp_dir().join(format!(
            "rs-sessions-clear-{}-{}.json",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);
        let a = SessionStore::with_persistence(&path);
        let (t1, _) = a.create("admin");
        a.remove(&t1);
        assert!(SessionStore::with_persistence(&path)
            .validate(&t1)
            .is_none());

        let (_, t2) = a.create("admin");
        a.clear();
        assert!(SessionStore::with_persistence(&path)
            .validate(&t2)
            .is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// Expired sessions are pruned at load; a corrupt file degrades to an
    /// empty store instead of breaking startup; the file stays owner-only.
    #[test]
    fn load_prunes_expired_tolerates_corrupt_and_keeps_file_private() {
        let path = std::env::temp_dir().join(format!(
            "rs-sessions-corrupt-{}-{}.json",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);
        let a = SessionStore::with_persistence(&path);
        let (t1, _) = a.create("admin");

        // Backdate the expiry directly in the file, then reload.
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        doc["sessions"][&t1]["expires_at"] = serde_json::json!(1_u64);
        std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
        assert!(SessionStore::with_persistence(&path)
            .validate(&t1)
            .is_none());

        // Corrupt file → empty store, not a panic.
        std::fs::write(&path, b"{not json").unwrap();
        let st = SessionStore::with_persistence(&path);
        assert!(st.is_empty());
        let _ = std::fs::remove_file(&path);

        // Persisted file permissions: owner-only.
        let p2 = path.with_extension("priv.json");
        let _ = std::fs::remove_file(&p2);
        let b_store = SessionStore::with_persistence(&p2);
        b_store.create("admin");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p2).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "session file must be owner-only");
        }
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn login_lockout_engages_after_failures() {
        let store = SessionStore::new();
        for _ in 0..LOGIN_MAX_FAILURES {
            assert!(!store.is_locked("admin"));
            store.record_failure("admin");
        }
        assert!(store.is_locked("admin"));
        // A successful login clears the counter.
        store.clear_failures("admin");
        assert!(!store.is_locked("admin"));
    }

    #[test]
    fn mask_roundtrip_preserves_stored_secret() {
        let stored = serde_json::json!({
            "web": {"password": "s3cret", "port": 8088},
            "rtsp": {"password": ""},
            "onvif": {"password": "onvif-pw"},
            "camera": {"fps": 15}
        });

        let masked = mask_passwords(stored.clone());
        assert_eq!(masked["web"]["password"], PASSWORD_MASK);
        assert_eq!(masked["onvif"]["password"], PASSWORD_MASK);
        assert_eq!(masked["rtsp"]["password"], "");
        assert_eq!(masked["web"]["port"], 8088);

        let mut roundtrip = masked;
        unmask_passwords(&mut roundtrip, &stored);
        assert_eq!(roundtrip["web"]["password"], "s3cret");
        assert_eq!(roundtrip["onvif"]["password"], "onvif-pw");
        assert_eq!(roundtrip["rtsp"]["password"], "");
    }

    #[test]
    fn unmask_ignores_absent_sections() {
        let current = serde_json::json!({"web": {"password": "s3cret"}});
        let mut value = serde_json::json!({"camera": {"fps": 30}});
        unmask_passwords(&mut value, &current);
        assert_eq!(value["camera"]["fps"], 30);
        assert!(value.get("web").is_none());
    }
}
