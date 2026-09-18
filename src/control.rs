//! Control socket: a line-oriented request/response protocol on an abstract
//! Unix socket so scripts and agents can read stats and flip state without the TUI.
//!
//! Request: one line, e.g. `stats`, `state`, `set confine on`, `quit`, `help`.
//! Response: one JSON line, always with an `ok` field.

use crate::GlobalState;
use crate::stats::Snapshot;
use anyhow::{Context as _, Result};
use arc_swap::ArcSwap;
use serde_json::json;
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::sync::Arc;
use tracing::{info, warn};

pub const SOCKET_NAME: &str = "waygriff-0.ctl";

/// Requests that must run on the display thread, which owns input state.
#[derive(Debug, Clone, Copy)]
pub enum ControlMsg {
    SetConfine(bool),
    SetForceRelative(bool),
    SetCapture(bool),
    Quit,
}

/// Everything a connection handler needs; cheap to clone per connection.
#[derive(Clone)]
struct Ctx {
    /// Refreshed by the main thread every 100ms.
    snapshot: Arc<ArcSwap<Snapshot>>,
    /// Read live so `state` reflects a `set` immediately.
    global_state: GlobalState,
    tx: Sender<ControlMsg>,
}

pub fn spawn(
    snapshot: Arc<ArcSwap<Snapshot>>,
    global_state: GlobalState,
    tx: Sender<ControlMsg>,
) -> Result<()> {
    let addr = SocketAddr::from_abstract_name(SOCKET_NAME)?;
    let listener = UnixListener::bind_addr(&addr).context("bind control socket")?;
    info!("control socket: @{SOCKET_NAME}");
    let ctx = Ctx {
        snapshot,
        global_state,
        tx,
    };
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let ctx = ctx.clone();
                        let _ = std::thread::Builder::new()
                            .name("control-conn".into())
                            .spawn(move || serve(stream, ctx));
                    }
                    Err(e) => warn!("control accept: {e}"),
                }
            }
        })?;
    Ok(())
}

fn serve(stream: UnixStream, ctx: Ctx) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let reply = handle(line.trim(), &ctx);
        let mut text = reply.to_string();
        text.push('\n');
        if writer.write_all(text.as_bytes()).is_err() {
            break;
        }
    }
}

fn handle(line: &str, ctx: &Ctx) -> serde_json::Value {
    let words: Vec<&str> = line.split_whitespace().collect();
    let send = |msg: ControlMsg| match ctx.tx.send(msg) {
        Ok(()) => json!({"ok": true}),
        Err(_) => json!({"ok": false, "error": "display thread gone"}),
    };
    match words.as_slice() {
        ["stats"] => {
            let mut v = serde_json::to_value(&**ctx.snapshot.load()).unwrap_or_default();
            v["ok"] = json!(true);
            v
        }
        ["state"] => {
            let s = ctx.global_state.load();
            json!({
                "ok": true,
                "confine": s.confine,
                "capture": s.capture,
                "force_relative": s.force_relative,
                "cursor_visible": s.cursor_visible,
            })
        }
        ["set", flag, value] => {
            let Some(on) = parse_bool(value) else {
                return json!({"ok": false, "error": "expected on|off"});
            };
            match *flag {
                "confine" => send(ControlMsg::SetConfine(on)),
                "capture" => send(ControlMsg::SetCapture(on)),
                "relative" => send(ControlMsg::SetForceRelative(on)),
                other => json!({"ok": false, "error": format!("unknown flag: {other}")}),
            }
        }
        ["quit"] => send(ControlMsg::Quit),
        ["help"] | [] => json!({
            "ok": true,
            "commands": ["stats", "state", "set confine|capture|relative on|off", "quit", "help"],
        }),
        _ => json!({"ok": false, "error": format!("unknown command: {line}")}),
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "on" | "true" | "1" | "yes" => Some(true),
        "off" | "false" | "0" | "no" => Some(false),
        _ => None,
    }
}
