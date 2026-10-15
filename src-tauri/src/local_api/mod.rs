//! Opt-in control API on loopback, for launchers and scripts (Raycast, Stream Deck, a shell alias).
//! Reference: `docs/local-api.md`.
//!
//! Off by default. When on, it binds `127.0.0.1:<port>` only (never a routable address) and every
//! request needs `Authorization: Bearer <token>`, a random 256-bit token kept in SQLite. Requests
//! carrying an `Origin` header, or a `Host` that isn't the loopback address, are refused before the
//! token is even looked at: that's what keeps a web page in the user's browser from reaching it,
//! through CSRF or DNS rebinding, even if the token leaked.
//!
//! Every route calls the same [`AppState`] methods the UI commands do, so the API can't drift from
//! what the buttons do (the same reasoning as media.rs).

mod auth;
mod routes;

use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::sync::watch;

use crate::db::Db;
use crate::state::AppState;

pub const DEFAULT_PORT: u16 = 26538;
/// Bumped only on a breaking change; additions keep it.
pub const API_VERSION: u32 = 1;

const KEY_ENABLED: &str = "local_api_enabled";
const KEY_PORT: &str = "local_api_port";
const KEY_TOKEN: &str = "local_api_token";

/// What the settings tab shows. The token is included: the webview already owns the switch that
/// grants it, and the user has to copy it into the client.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub enabled: bool,
    pub port: u16,
    pub token: String,
    pub running: bool,
    /// Why the server isn't up although enabled (port taken, typically).
    pub error: Option<String>,
}

/// Shared with every connection of one server instance.
#[derive(Clone)]
struct Ctx {
    port: u16,
    /// Swapped in place on "regenerate", so a new token applies without rebinding.
    token: Arc<RwLock<String>>,
    /// Set when the server is stopped; kept-alive connections answer 503 from then on.
    stopped: Arc<AtomicBool>,
}

struct Running {
    ctx: Ctx,
    stop: watch::Sender<bool>,
}

#[derive(Default)]
struct Inner {
    running: Option<Running>,
    error: Option<String>,
}

/// Managed as Tauri state; owns the server's lifecycle.
#[derive(Default)]
pub struct LocalApi {
    inner: Mutex<Inner>,
}

struct Config {
    enabled: bool,
    port: u16,
    token: String,
}

fn load(db: &Db) -> Config {
    Config {
        enabled: db.get_setting(KEY_ENABLED).as_deref() == Some("true"),
        port: db
            .get_setting(KEY_PORT)
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p >= 1024)
            .unwrap_or(DEFAULT_PORT),
        token: db.get_setting(KEY_TOKEN).unwrap_or_default(),
    }
}

/// 32 random bytes, hex.
fn new_token() -> String {
    rand::random::<[u8; 32]>().iter().map(|b| format!("{b:02x}")).collect()
}

impl LocalApi {
    pub fn status(&self, state: &AppState) -> Status {
        let cfg = load(&state.db);
        let inner = self.inner.lock().unwrap();
        Status {
            enabled: cfg.enabled,
            port: cfg.port,
            token: cfg.token,
            running: inner.running.is_some(),
            error: inner.error.clone(),
        }
    }

    /// The settings switch and port field. Turning it on for the first time mints the token.
    pub fn configure(
        &self,
        state: &Arc<AppState>,
        enabled: bool,
        port: u16,
    ) -> Result<Status, String> {
        if port < 1024 {
            return Err("port must be between 1024 and 65535".into());
        }
        if enabled && load(&state.db).token.is_empty() {
            state.db.set_setting(KEY_TOKEN, &new_token());
        }
        state.db.set_setting(KEY_ENABLED, if enabled { "true" } else { "false" });
        state.db.set_setting(KEY_PORT, &port.to_string());
        self.apply(state);
        Ok(self.status(state))
    }

    /// Invalidates every client holding the old token, effective on their next request.
    pub fn regenerate_token(&self, state: &Arc<AppState>) -> Status {
        state.db.set_setting(KEY_TOKEN, &new_token());
        self.apply(state);
        self.status(state)
    }

    /// Make the running server match the stored settings: start, stop, rebind, or retoken.
    /// Called at startup and after every settings change.
    pub fn apply(&self, state: &Arc<AppState>) {
        let cfg = load(&state.db);
        let mut inner = self.inner.lock().unwrap();
        inner.error = None;

        if let Some(r) = &inner.running {
            if cfg.enabled && r.ctx.port == cfg.port {
                *r.ctx.token.write().unwrap() = cfg.token;
                return;
            }
        }
        if let Some(r) = inner.running.take() {
            r.ctx.stopped.store(true, Ordering::SeqCst);
            let _ = r.stop.send(true);
            tracing::info!("local api stopped");
        }
        if !cfg.enabled || cfg.token.is_empty() {
            return;
        }

        let listener = match bind(cfg.port) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(port = cfg.port, error = %e, "local api: bind failed");
                inner.error = Some(if e.kind() == io::ErrorKind::AddrInUse {
                    format!("Port {} is already in use", cfg.port)
                } else {
                    format!("Couldn't listen on port {}: {e}", cfg.port)
                });
                return;
            }
        };
        let ctx = Ctx {
            port: cfg.port,
            token: Arc::new(RwLock::new(cfg.token)),
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let (stop, stop_rx) = watch::channel(false);
        serve(listener, state.clone(), ctx.clone(), stop_rx);
        tracing::info!(port = cfg.port, "local api listening on loopback");
        inner.running = Some(Running { ctx, stop });
    }
}

/// Binds synchronously so a taken port is reported to the settings tab, not just logged. A port we
/// were listening on a moment ago can still be held by the stopping accept loop, hence the retries.
fn bind(port: u16) -> io::Result<TcpListener> {
    let mut attempt = 0;
    loop {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(l) => {
                l.set_nonblocking(true)?;
                return Ok(l);
            }
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && attempt < 10 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

fn serve(listener: TcpListener, state: Arc<AppState>, ctx: Ctx, mut stop: watch::Receiver<bool>) {
    tauri::async_runtime::spawn(async move {
        let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
        loop {
            let accepted = tokio::select! {
                _ = stop.changed() => break,
                a = listener.accept() => a,
            };
            let stream = match accepted {
                Ok((s, _)) => s,
                Err(e) => {
                    // Same back-off as videoproxy.rs: EMFILE returns at once and would spin.
                    tracing::warn!(error = %e, "local api: accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let (state, ctx) = (state.clone(), ctx.clone());
            tauri::async_runtime::spawn(async move {
                let svc = service_fn(move |req| routes::serve(req, state.clone(), ctx.clone()));
                let _ = http1::Builder::new()
                    // hyper panics arming a timeout without a timer (see videoproxy.rs).
                    .timer(TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(10))
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
        // Dropping the listener here frees the port.
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_64_hex_chars_and_random() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
