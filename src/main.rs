//! cce-shortcuts-portal — the `org.freedesktop.impl.portal.GlobalShortcuts`
//! backend for the cce compositor.
//!
//! A native Wayland app cannot grab a key; it asks xdg-desktop-portal to bind
//! a trigger for it, and the portal frontend hands that to whichever backend
//! `~/.config/xdg-desktop-portal/cce-portals.conf` names for the interface.
//! This is that backend. It owns no key handling of its own: every trigger
//! is forwarded to the compositor over its control socket (`shortcut bind
//! <session> <id> <trigger>`), and every press comes back as a line on the
//! status socket's `shortcuts` topic, which is re-emitted here as the
//! portal's `Activated` / `Deactivated` signal. See `global_shortcuts.rs`
//! in cce-compositor for the other end.
//!
//! Bus-activated (`dbus/…cce-shortcuts.service`), so it starts on the first
//! request and lives as long as the compositor's status socket does: when
//! that closes the process exits, and the next request starts a fresh one.
//! A fresh one begins with `shortcut clear`, so binds left by a predecessor
//! that died can never keep eating chords nobody is listening for.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, SerializeDict, Type, Value};
use zbus::{interface, Connection, ObjectServer};

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.cce-shortcuts";
const OBJ_PATH: &str = "/org/freedesktop/portal/desktop";

/// Portal request responses.
const RESPONSE_OK: u32 = 0;
const RESPONSE_OTHER: u32 = 2;

// ── wire types ─────────────────────────────────────────────────────────────

/// One shortcut as the frontend wants it back: `(sa{sv})` with the two
/// documented keys.
#[derive(Clone, Debug, Serialize, Type)]
struct BoundShortcut(String, ShortcutProps);

#[derive(Clone, Debug, SerializeDict, Type)]
#[zvariant(signature = "dict")]
struct ShortcutProps {
    description: String,
    trigger_description: String,
}

/// The `results` vardict of BindShortcuts / ListShortcuts.
#[derive(Clone, Debug, SerializeDict, Type)]
#[zvariant(signature = "dict")]
struct ShortcutsResults {
    shortcuts: Vec<BoundShortcut>,
}

// ── state ──────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Session {
    #[allow(dead_code)]
    app_id: String,
    bound: Vec<BoundShortcut>,
}

#[derive(Default)]
struct Shared {
    sessions: HashMap<OwnedObjectPath, Session>,
}

type SharedState = Arc<Mutex<Shared>>;

// ── compositor sockets ─────────────────────────────────────────────────────

fn display() -> Option<String> {
    std::env::var("WAYLAND_DISPLAY").ok().filter(|d| !d.is_empty())
}

/// The control socket, as `ccectl` resolves it.
fn ctl_socket_path() -> String {
    match display() {
        Some(d) => format!("/tmp/cce-{d}.sock"),
        None => "/tmp/cce.sock".to_string(),
    }
}

/// The status socket (`status_server::get_status_socket_path`).
fn status_socket_path() -> String {
    match display() {
        Some(d) => format!("/tmp/cce-status-interface-{d}.sock"),
        None => "/tmp/cce-status-interface.sock".to_string(),
    }
}

/// One request/reply round on the control socket. The compositor answers
/// one line and closes, so the reply is everything up to EOF.
async fn ctl(cmd: &str) -> std::io::Result<String> {
    let mut stream = tokio::net::UnixStream::connect(ctl_socket_path()).await?;
    stream.write_all(format!("{cmd}\n").as_bytes()).await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    Ok(reply)
}

/// Shortcut ids are app-chosen and may contain anything, but the control
/// socket splits its command on whitespace: percent-encode everything
/// outside a safe set on the way out and decode on the way back. `+` is
/// safe because the trigger string is built from it (`CTRL+SHIFT+space`)
/// and the compositor parses the trigger as written; an id keeps its `+`
/// too, which round-trips just the same. The compositor stores and echoes
/// the encoded id (`ccectl shortcut list` shows `Quick%20Access`); only
/// this side ever decodes.
fn encode_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~' | b'+') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode_token(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: Option<&u8>) -> Option<u8> {
    b.and_then(|b| (*b as char).to_digit(16)).map(|d| d as u8)
}

fn str_prop(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match props.get(key).map(|v| &**v) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

// ── org.freedesktop.impl.portal.GlobalShortcuts ────────────────────────────

struct Backend {
    shared: SharedState,
}

#[interface(name = "org.freedesktop.impl.portal.GlobalShortcuts")]
impl Backend {
    /// Version 1: no `ConfigureShortcuts` (that is version 2), since there
    /// is no configuration UI — a shortcut is bound to the trigger the app
    /// preferred or not at all.
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }

    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let path = OwnedObjectPath::from(session_handle);
        log::info!("CreateSession {} for app {:?}", path, app_id);
        self.shared.lock().await.sessions.insert(path.clone(), Session { app_id, bound: Vec::new() });
        let obj = SessionObj { path: path.clone(), shared: self.shared.clone() };
        if let Err(e) = server.at(&path, obj).await {
            log::error!("exporting session object {}: {}", path, e);
            self.shared.lock().await.sessions.remove(&path);
            return (RESPONSE_OTHER, HashMap::new());
        }
        (RESPONSE_OK, HashMap::new())
    }

    async fn bind_shortcuts(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        shortcuts: Vec<(String, HashMap<String, OwnedValue>)>,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, ShortcutsResults) {
        let session = OwnedObjectPath::from(session_handle);
        if !self.shared.lock().await.sessions.contains_key(&session) {
            log::warn!("BindShortcuts for unknown session {}", session);
            return (RESPONSE_OTHER, ShortcutsResults { shortcuts: Vec::new() });
        }
        let mut bound = Vec::new();
        for (id, props) in shortcuts {
            let description = str_prop(&props, "description").unwrap_or_default();
            // Without a preferred trigger there is nothing to bind to: a
            // backend with a configuration dialog would ask the user here.
            let Some(trigger) = str_prop(&props, "preferred_trigger") else {
                log::warn!("{}: shortcut {:?} has no preferred_trigger; not bound", session, id);
                continue;
            };
            let cmd = format!("shortcut bind {} {} {}", session, encode_token(&id), encode_token(&trigger));
            match ctl(&cmd).await {
                Ok(reply) if reply.starts_with("ok") => {
                    let trigger_description = reply[2..].trim().to_string();
                    log::info!("{}: bound {:?} to {}", session, id, trigger_description);
                    bound.push(BoundShortcut(id, ShortcutProps { description, trigger_description }));
                }
                Ok(reply) => log::warn!("{}: compositor refused {:?} ({}): {}", session, id, trigger, reply.trim()),
                Err(e) => log::error!("control socket {}: {}", ctl_socket_path(), e),
            }
        }
        if let Some(s) = self.shared.lock().await.sessions.get_mut(&session) {
            s.bound = bound.clone();
        }
        (RESPONSE_OK, ShortcutsResults { shortcuts: bound })
    }

    async fn list_shortcuts(&self, _handle: ObjectPath<'_>, session_handle: ObjectPath<'_>) -> (u32, ShortcutsResults) {
        let session = OwnedObjectPath::from(session_handle);
        let shortcuts = self
            .shared
            .lock()
            .await
            .sessions
            .get(&session)
            .map(|s| s.bound.clone())
            .unwrap_or_default();
        (RESPONSE_OK, ShortcutsResults { shortcuts })
    }

    #[zbus(signal)]
    async fn activated(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        shortcut_id: &str,
        timestamp: u64,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn deactivated(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        shortcut_id: &str,
        timestamp: u64,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn shortcuts_changed(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        shortcuts: Vec<BoundShortcut>,
    ) -> zbus::Result<()>;
}

// ── org.freedesktop.impl.portal.Session ────────────────────────────────────

/// One object per session, at the path the frontend chose. `Close` is how
/// the app (or its death, via the frontend) gives its chords back.
struct SessionObj {
    path: OwnedObjectPath,
    shared: SharedState,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObj {
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }

    async fn close(&self, #[zbus(connection)] conn: &Connection) {
        log::info!("Close {}", self.path);
        if let Err(e) = ctl(&format!("shortcut unbind {}", self.path)).await {
            log::warn!("control socket {}: {}", ctl_socket_path(), e);
        }
        self.shared.lock().await.sessions.remove(&self.path);
        // The object cannot remove itself from inside its own method call
        // (the server holds it locked for the call), so it goes on the
        // next turn of the loop.
        let conn = conn.clone();
        let path = self.path.clone();
        tokio::spawn(async move {
            if let Err(e) = conn.object_server().remove::<SessionObj, _>(&path).await {
                log::warn!("removing session object {}: {}", path, e);
            }
        });
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

// ── main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(e) = run().await {
        log::error!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // A predecessor that died mid-session leaves its chords bound in the
    // compositor; nobody will ever hear them fire, so drop them before
    // taking the bus name. This also proves the compositor speaks the
    // command at all: an older one answers `error: unknown command`, and
    // then this backend must not claim to serve anything.
    let reply = ctl("shortcut clear")
        .await
        .map_err(|e| format!("control socket {}: {e}", ctl_socket_path()))?;
    if !reply.starts_with("ok") {
        return Err(format!("compositor does not support portal shortcuts: {}", reply.trim()).into());
    }

    // Subscribe to the press/release feed before serving, so a bind can
    // never fire into a gap.
    let status = tokio::net::UnixStream::connect(status_socket_path())
        .await
        .map_err(|e| format!("status socket {}: {e}", status_socket_path()))?;
    let (reader, mut writer) = status.into_split();
    writer.write_all(b"shortcuts\n").await?;

    let shared: SharedState = Arc::new(Mutex::new(Shared::default()));
    let conn = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJ_PATH, Backend { shared: shared.clone() })?
        .build()
        .await?;
    log::info!("serving {} at {}", BUS_NAME, OBJ_PATH);

    let emitter = SignalEmitter::new(&conn, OBJ_PATH)?;
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let mut it = line.split_whitespace();
        let (Some(kind), Some(session), Some(id)) = (it.next(), it.next(), it.next()) else {
            log::warn!("unparseable status line {:?}", line);
            continue;
        };
        let timestamp: u64 = it.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        let Ok(path) = ObjectPath::try_from(session) else {
            log::warn!("bad session path in {:?}", line);
            continue;
        };
        let id = decode_token(id);
        let result = match kind {
            "activated" => Backend::activated(&emitter, path, &id, timestamp, HashMap::new()).await,
            "deactivated" => Backend::deactivated(&emitter, path, &id, timestamp, HashMap::new()).await,
            other => {
                log::warn!("unknown shortcut event {:?}", other);
                Ok(())
            }
        };
        if let Err(e) = result {
            log::warn!("emitting {} for {}: {}", kind, session, e);
        }
    }
    // The compositor closed the feed: every session is void with it.
    log::info!("status socket closed; exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trip() {
        for s in ["quick-access", "Quick Access", "a/b%c", "üñî", "", "x%2", "%"] {
            let enc = encode_token(s);
            assert!(!enc.contains(char::is_whitespace));
            assert_eq!(decode_token(&enc), s);
        }
        assert_eq!(encode_token("CTRL+SHIFT+space"), "CTRL+SHIFT+space", "triggers pass through untouched");
    }
}
