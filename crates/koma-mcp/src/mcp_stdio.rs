//! Model Context Protocol (`2024-11-05` / `2025-03-26`) JSON-RPC 2.0 `stdio` server.
//!
//! Aligns `CallToolResult` (`content` text + `image/png` blocks & `isError` semantics) with
//! `open-computer-use` while exposing Koma's isolated headless workspaces, `grid_225` zero-copy
//! captures, Zero-IME text/TSV injection, and parameterized `Phodopus` macro skills.

use std::io::{self, BufRead, Write};

use koma_lua::MacroDefinition;
use koma_proto::{Epoch, ErrorSymbol, OverlayKind, WorkspaceId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent_client::{CapturedPngObservation, KomaAgentSession, MutateOutcome};

pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub const KOMA_MCP_INSTRUCTIONS: &str = "Koma is an Agent-native Wayland tiling compositor with isolated Headless workspaces, per-workspace virtual seats (vSeat), Zero-IME text/MIME injection, and monotonic Epoch concurrency fencing. 1) Always call `koma_scene_inventory` or `koma_capture_frame` before mutating a workspace so you hold a Fresh `expected_epoch`. 2) Prefer isolated Headless workspaces (`koma_ws_create`) so the human user's physical monitor and seat0 clipboard remain completely undisturbed. 3) If a tool returns `ERR_STALE_EPOCH` (`isError: true`), the action was safely aborted BEFORE execution and the attached PNG image is ALREADY a fresh re-observed snapshot at `new_observed_epoch`—inspect the new image and recompute your next action using `new_observed_epoch`. 4) If a tool returns `ERR_HUMAN_PREEMPTED` or `ERR_PROTECTED_SURFACE`, pause and ask the user.";

/// Lazy connection wrapper so `koma-mcp` can respond to `initialize`, `tools/list`,
/// `resources/list`, and `prompts/list` before a Koma compositor session is running.
pub struct LazyKomaSession {
    auto_mint: bool,
    inner: Option<KomaAgentSession>,
}

impl LazyKomaSession {
    #[must_use]
    pub fn new(auto_mint: bool) -> Self {
        Self {
            auto_mint,
            inner: None,
        }
    }

    pub fn try_connected(&mut self) -> Option<&mut KomaAgentSession> {
        if self.inner.is_none()
            && let Ok(session) = KomaAgentSession::from_env(self.auto_mint)
        {
            self.inner = Some(session);
        }
        self.inner.as_mut()
    }

    pub fn ensure_connected(&mut self) -> Result<&mut KomaAgentSession, koma_proto::IpcError> {
        if self.inner.is_none() {
            self.inner = Some(KomaAgentSession::from_env(self.auto_mint)?);
        }
        Ok(self.inner.as_mut().expect("session initialized above"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFraming {
    Ndjson,
    ContentLength,
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// Converts a registered `Phodopus` `MacroDefinition` into an MCP `Tool` JSON schema.
#[must_use]
pub fn macro_to_mcp_tool_schema(def: &MacroDefinition) -> Value {
    let mut arg_props = serde_json::Map::new();
    let mut required_args = Vec::new();
    for (name, spec) in &def.params {
        let json_ty = match spec.param_type.as_str() {
            "int" => "integer",
            "float" => "number",
            "bool" => "boolean",
            _ => "string",
        };
        let mut p = json!({ "type": json_ty });
        if let Some(min) = spec.min {
            p["minimum"] = json!(min);
        }
        if let Some(max) = spec.max {
            p["maximum"] = json!(max);
        }
        if let Some(def_val) = &spec.default_value {
            p["default"] = def_val.clone();
        }
        if spec.required {
            required_args.push(Value::String(name.clone()));
        }
        arg_props.insert(name.clone(), p);
    }

    json!({
        "name": format!("macro__{}", def.id.as_str().replace('.', "__")),
        "description": format!("[Phodopus Zero-Token Macro for `{}`] {}", def.app_id_regex, def.description),
        "inputSchema": {
            "type": "object",
            "required": ["ws_id"],
            "properties": {
                "ws_id": { "type": "string", "description": "Target workspace ID (e.g. ws_02)" },
                "expected_epoch": { "type": "integer", "minimum": 1, "description": "Optional; defaults to latest observed epoch" },
                "args": {
                    "type": "object",
                    "required": required_args,
                    "properties": arg_props,
                }
            }
        }
    })
}

#[must_use]
pub fn builtin_mcp_tool_schemas(dynamic_macros: &[MacroDefinition]) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "koma_ping",
            "description": "Reports whether `koma-mcp` can currently reach a running Koma compositor session (works offline).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "koma_scene_inventory",
            "description": "Lists accessible Headed and Headless workspaces, current Epoch fences, active Deck window, and background Stash windows.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "koma_ws_create",
            "description": "Creates an isolated Headless workspace (1920x1080 Deck monocle stack, 0% idle GPU) and acquires its initial write lease.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "width": { "type": "integer", "default": 1920 },
                    "height": { "type": "integer", "default": 1080 }
                }
            }
        }),
        json!({
            "name": "koma_capture_frame",
            "description": "Captures a zero-copy Render-on-Read PNG screenshot of `ws_id` (with optional `grid_225` 15x15 cell labels A1..O15) and records a Fresh Epoch.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "overlay": { "type": "string", "enum": ["grid_225", "som", "none"], "default": "grid_225" },
                    "max_edge": { "type": "integer", "default": 1920 }
                }
            }
        }),
        json!({
            "name": "koma_click",
            "description": "Clicks a `grid_225` cell (`A1`..`O15`) or `(x, y)` coordinate on `ws_id`'s isolated vSeat under Epoch fencing.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "grid_225": { "type": "string", "description": "15x15 cell label e.g. G8" },
                    "x": { "type": "number" },
                    "y": { "type": "number" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" },
                    "clicks": { "type": "integer", "minimum": 1, "maximum": 3, "default": 1 },
                    "modifiers": { "type": "array", "items": { "type": "string" } },
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "koma_drag",
            "description": "Drags smoothly (`minimum_jerk`) between two `grid_225` cells or `(x, y)` points on `ws_id`'s isolated vSeat.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id", "from", "to"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "from": { "type": "object", "properties": { "grid_225": { "type": "string" }, "x": { "type": "number" }, "y": { "type": "number" } } },
                    "to":   { "type": "object", "properties": { "grid_225": { "type": "string" }, "x": { "type": "number" }, "y": { "type": "number" } } },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" },
                    "duration_ms": { "type": "integer", "default": 110 },
                    "modifiers": { "type": "array", "items": { "type": "string" } },
                    "label": { "type": "string" },
                    "bow": { "type": "number" },
                    "overshoot": { "type": "number" },
                    "tremor": { "type": "number" },
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "koma_key",
            "description": "Taps a keyboard shortcut or chord (e.g. `Ctrl+S`, `Alt+H`, `Return`, `Tab`) on `ws_id`'s vSeat.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id", "chord"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "chord": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "koma_inject_text",
            "description": "Injects UTF-8 text directly into the focused window on `ws_id` bypassing Fcitx5/IBus IME candidate popups.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id", "text"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "text": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "koma_paste_mime",
            "description": "Pastes structured MIME content (`text/tab-separated-values` for spreadsheets or `text/html` for rich docs) via `ws_id`'s isolated vSeat clipboard.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id", "mime_type", "data_utf8"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "mime_type": { "type": "string", "default": "text/tab-separated-values" },
                    "data_utf8": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "koma_win_activate",
            "description": "Promotes a background Stash window (`win_slot`) onto the active 100%-viewport Deck.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id", "win_slot"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "win_slot": { "type": "integer" },
                    "win_generation": { "type": "integer", "default": 1 }
                }
            }
        }),
        json!({
            "name": "koma_exec_macro",
            "description": "Executes a parameterized Phodopus Lua 5.4 macro (`macro_id`) atomically on `ws_id` with zero LLM token overhead.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id", "macro_id"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "macro_id": { "type": "string" },
                    "args": { "type": "object" }
                }
            }
        }),
        json!({
            "name": "koma_path",
            "description": "Moves or strokes the workspace agent cursor along a smooth multi-waypoint spline (`catmull_rom` or `bezier`) on `ws_id`'s isolated vSeat under Epoch fencing.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id", "waypoints"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "waypoints": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 128,
                        "items": {
                            "type": "object",
                            "properties": {
                                "grid_225": { "type": "string" },
                                "x": { "type": "number" },
                                "y": { "type": "number" },
                                "anchor": { "type": "string" },
                                "dx": { "type": "number" },
                                "dy": { "type": "number" }
                            }
                        }
                    },
                    "spline": { "type": "string", "enum": ["auto", "catmull_rom", "bezier"], "default": "auto" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"] },
                    "hold_start_ms": { "type": "integer", "minimum": 0, "default": 0 },
                    "hold_end_ms": { "type": "integer", "minimum": 0, "default": 0 },
                    "duration_ms": { "type": "integer", "minimum": 0 },
                    "kinematics": { "type": "object" },
                    "modifiers": { "type": "array", "items": { "type": "string" } },
                    "label": { "type": "string" },
                    "bow": { "type": "number" },
                    "overshoot": { "type": "number" },
                    "tremor": { "type": "number" },
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "koma_cursor_settle",
            "description": "Transitions the AI agent cursor on `ws_id` into settle fade-out (`280 ms` smooth fade or immediate hide).",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "immediate": { "type": "boolean", "default": false },
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "koma_wheel",
            "description": "Dispatches an `AxisFrame` scroll wheel event (optionally aimed at `target`) on `ws_id`'s isolated vSeat under Epoch fencing.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "target": {
                        "type": "object",
                        "properties": {
                            "grid_225": { "type": "string" },
                            "x": { "type": "number" },
                            "y": { "type": "number" }
                        }
                    },
                    "dx": { "type": "number", "default": 0.0 },
                    "dy": { "type": "number", "default": 0.0 },
                    "discrete_x": { "type": "integer", "default": 0 },
                    "discrete_y": { "type": "integer", "default": 0 },
                    "modifiers": { "type": "array", "items": { "type": "string" } },
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "koma_relative_move",
            "description": "Sends relative pointer motion (`dx`, `dy`) aware of `zwp_relative_pointer_v1` / `PointerConstraint::Locked` for camera/3D/game control.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id", "dx", "dy"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "expected_epoch": { "type": "integer", "minimum": 1 },
                    "dx": { "type": "number" },
                    "dy": { "type": "number" },
                    "dx_unaccel": { "type": "number" },
                    "dy_unaccel": { "type": "number" },
                    "modifiers": { "type": "array", "items": { "type": "string" } },
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "koma_hit_test",
            "description": "Probes 5 clamped sample points around `target` on `ws_id` to detect occluding windows and active layout animations without mutating input.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id", "target"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "target": {
                        "type": "object",
                        "properties": {
                            "grid_225": { "type": "string" },
                            "x": { "type": "number" },
                            "y": { "type": "number" },
                            "anchor": { "type": "string" },
                            "dx": { "type": "number" },
                            "dy": { "type": "number" }
                        }
                    },
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "koma_cursor_label",
            "description": "Sets or clears the Koma companion `LabelPill` caption and optional `[x, y, w, h]` `caret_rect` highlight on `ws_id`.",
            "inputSchema": {
                "type": "object",
                "required": ["ws_id"],
                "properties": {
                    "ws_id": { "type": "string" },
                    "label": { "type": "string", "description": "Short caption (<=8 words / <=48 chars); omit to clear" },
                    "caret_rect": {
                        "type": "array",
                        "minItems": 4,
                        "maxItems": 4,
                        "items": { "type": "number" }
                    },
                    "compact": { "type": "boolean", "default": false }
                }
            }
        }),
    ];

    for m in dynamic_macros {
        tools.push(macro_to_mcp_tool_schema(m));
    }
    tools
}

pub fn run_stdio_mcp_loop(session: &mut LazyKomaSession) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    run_stdio_mcp_loop_with_rw(session, &mut reader, &mut writer)
}

/// Reads an MCP frame from `reader`, auto-detecting `Content-Length` vs NDJSON per message.
fn read_stdio_frame(reader: &mut impl BufRead) -> io::Result<Option<(McpFraming, String)>> {
    let mut first = String::new();
    loop {
        first.clear();
        let n = reader.read_line(&mut first)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = first.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            return Ok(Some((McpFraming::Ndjson, trimmed.to_owned())));
        }
        if let Some((name, val)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let content_len: usize = val.trim().parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid Content-Length header: {e}"),
                )
            })?;
            let mut hdr_line = String::new();
            loop {
                hdr_line.clear();
                let hn = reader.read_line(&mut hdr_line)?;
                if hn == 0 || hdr_line.trim().is_empty() {
                    break;
                }
            }
            let mut body = vec![0_u8; content_len];
            reader.read_exact(&mut body)?;
            let body_str = String::from_utf8(body).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("invalid UTF-8: {e}"))
            })?;
            return Ok(Some((McpFraming::ContentLength, body_str)));
        }
        return Ok(Some((McpFraming::Ndjson, trimmed.to_owned())));
    }
}

fn write_stdio_frame(
    writer: &mut impl Write,
    framing: McpFraming,
    payload: &str,
) -> io::Result<()> {
    match framing {
        McpFraming::Ndjson => writeln!(writer, "{payload}")?,
        McpFraming::ContentLength => {
            write!(
                writer,
                "Content-Length: {}\r\n\r\n{}",
                payload.len(),
                payload
            )?;
        }
    }
    writer.flush()
}

pub fn run_stdio_mcp_loop_with_rw(
    session: &mut LazyKomaSession,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> io::Result<()> {
    while let Some((framing, payload)) = read_stdio_frame(reader)? {
        let req: JsonRpcRequest = match serde_json::from_str(&payload) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(json!({ "code": -32700, "message": format!("parse error: {e}") })),
                };
                write_stdio_frame(writer, framing, &serde_json::to_string(&err_resp).unwrap())?;
                continue;
            }
        };

        // JSON-RPC notifications (no `id`, e.g. `notifications/initialized`) produce no reply frame
        let Some(req_id) = req.id.clone() else {
            continue;
        };

        let resp = handle_mcp_request(session, req_id, &req.method, &req.params);
        write_stdio_frame(writer, framing, &serde_json::to_string(&resp).unwrap())?;
    }

    Ok(())
}

pub fn handle_mcp_request(
    session: &mut LazyKomaSession,
    id: Value,
    method: &str,
    params: &Value,
) -> JsonRpcResponse {
    match method {
        "initialize" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {
                    "tools": { "listChanged": true },
                    "resources": { "subscribe": false, "listChanged": false },
                    "prompts": { "listChanged": false }
                },
                "serverInfo": {
                    "name": "koma-mcp",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": KOMA_MCP_INSTRUCTIONS,
            })),
            error: None,
        },
        "ping" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({})),
            error: None,
        },
        "resources/list" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({ "resources": [] })),
            error: None,
        },
        "resources/templates/list" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({ "resourceTemplates": [] })),
            error: None,
        },
        "prompts/list" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({ "prompts": [] })),
            error: None,
        },
        "tools/list" => {
            let macros = session
                .try_connected()
                .map(|s| s.fetch_registered_macros())
                .unwrap_or_default();
            let tools = builtin_mcp_tool_schemas(&macros);
            JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(json!({ "tools": tools })),
                error: None,
            }
        }
        "tools/call" => {
            let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let call_res = dispatch_tool_call(session, tool_name, args);
            JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(call_res),
                error: None,
            }
        }
        other => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(json!({
                "code": -32601,
                "message": format!("method not found: {other}"),
            })),
        },
    }
}

fn format_json_text(val: &Value, compact: bool) -> String {
    if compact {
        serde_json::to_string(val).unwrap()
    } else {
        serde_json::to_string_pretty(val).unwrap()
    }
}

fn dispatch_tool_call(lazy: &mut LazyKomaSession, name: &str, mut args: Value) -> Value {
    let compact = args
        .get("compact")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(obj) = args.as_object_mut() {
        obj.remove("compact");
    }

    if name == "koma_ping" {
        let payload = match lazy.try_connected() {
            Some(sess) => json!({
                "ok": true,
                "compositor_connected": true,
                "session_id": sess.session_id,
            }),
            None => json!({
                "ok": true,
                "compositor_connected": false,
                "note": "Koma compositor IPC session is not running yet; discovery works offline.",
            }),
        };
        return mcp_text_result(format_json_text(&payload, compact), false);
    }

    let session = match lazy.ensure_connected() {
        Ok(s) => s,
        Err(e) => return mcp_ipc_error_result(&e, compact),
    };

    match name {
        "koma_scene_inventory" => match session.fetch_inventory_and_record_epochs() {
            Ok(inv) => mcp_text_result(format_json_text(&json!(inv), compact), false),
            Err(e) => mcp_ipc_error_result(&e, compact),
        },

        "koma_ws_create" => {
            let ws_name = args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("agent-work");
            let w = args.get("width").and_then(Value::as_u64).unwrap_or(1920);
            let h = args.get("height").and_then(Value::as_u64).unwrap_or(1080);
            let res = session.raw_rpc(
                "ws.create",
                json!({
                    "name": ws_name,
                    "mode": "headless",
                    "viewport": { "width": w, "height": h, "scale": 1.0 },
                    "default_layout": "deck",
                    "acquire_lease": true,
                }),
            );
            match res {
                Ok(val) => {
                    if let (Some(ws_s), Some(ep_n)) = (
                        val.get("ws_id").and_then(Value::as_str),
                        val.get("epoch").and_then(Value::as_u64),
                    ) {
                        session
                            .epochs
                            .record_observed(WorkspaceId::new(ws_s), Epoch(ep_n), false);
                    }
                    mcp_text_result(format_json_text(&val, compact), false)
                }
                Err(e) => mcp_ipc_error_result(&e, compact),
            }
        }

        "koma_capture_frame" => {
            let Some(ws_s) = args.get("ws_id").and_then(Value::as_str) else {
                return mcp_text_result("missing required argument `ws_id`".into(), true);
            };
            let overlay = match args.get("overlay").and_then(Value::as_str) {
                Some("none") => OverlayKind::None,
                Some("som") => OverlayKind::Som,
                _ => OverlayKind::Grid225,
            };
            let max_edge = args.get("max_edge").and_then(Value::as_u64).unwrap_or(1920) as u32;
            match session.capture_png_observation(&WorkspaceId::new(ws_s), overlay, max_edge) {
                Ok(obs) => format_observation_tool_result(&obs, None, false),
                Err(e) => mcp_ipc_error_result(&e, compact),
            }
        }

        "koma_click" | "koma_drag" | "koma_path" | "koma_wheel" | "koma_relative_move"
        | "koma_key" | "koma_inject_text" | "koma_paste_mime" | "koma_win_activate" => {
            let Some(ws_s) = args.get("ws_id").and_then(Value::as_str) else {
                return mcp_text_result("missing required argument `ws_id`".into(), true);
            };
            let ws_id = WorkspaceId::new(ws_s);
            let explicit_ep = args
                .get("expected_epoch")
                .and_then(Value::as_u64)
                .map(Epoch);
            let ipc_method = match name {
                "koma_click" => "vseat.click",
                "koma_drag" => "vseat.drag",
                "koma_path" => "vseat.path",
                "koma_wheel" => "vseat.wheel",
                "koma_relative_move" => "vseat.relative_move",
                "koma_key" => "vseat.key",
                "koma_inject_text" => "vseat.inject_text",
                "koma_paste_mime" => "vseat.paste_mime",
                "koma_win_activate" => "win.activate",
                _ => unreachable!(),
            };
            let outcome = session.execute_fenced_mutation(&ws_id, explicit_ep, ipc_method, args);
            format_mutate_outcome(outcome, compact)
        }

        "koma_hit_test" => {
            let Some(ws_s) = args.get("ws_id").and_then(Value::as_str) else {
                return mcp_text_result("missing required argument `ws_id`".into(), true);
            };
            let target = args.get("target").cloned().unwrap_or(Value::Null);
            match session.raw_rpc("vseat.hit_test", json!({ "ws_id": ws_s, "target": target })) {
                Ok(val) => mcp_text_result(format_json_text(&val, compact), false),
                Err(err) => mcp_ipc_error_result(&err, compact),
            }
        }

        "koma_cursor_label" => {
            let Some(ws_s) = args.get("ws_id").and_then(Value::as_str) else {
                return mcp_text_result("missing required argument `ws_id`".into(), true);
            };
            let mut payload = serde_json::Map::new();
            payload.insert("ws_id".to_owned(), json!(ws_s));
            if let Some(label) = args.get("label") {
                payload.insert("label".to_owned(), label.clone());
            }
            if let Some(caret_rect) = args.get("caret_rect") {
                payload.insert("caret_rect".to_owned(), caret_rect.clone());
            }
            match session.raw_rpc("vseat.cursor_label", Value::Object(payload)) {
                Ok(val) => mcp_text_result(format_json_text(&val, compact), false),
                Err(err) => mcp_ipc_error_result(&err, compact),
            }
        }

        "koma_cursor_settle" => {
            let Some(ws_s) = args.get("ws_id").and_then(Value::as_str) else {
                return mcp_text_result("missing required argument `ws_id`".into(), true);
            };
            let immediate = args
                .get("immediate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match session.raw_rpc(
                "vseat.cursor_settle",
                json!({ "ws_id": ws_s, "immediate": immediate }),
            ) {
                Ok(val) => mcp_text_result(format_json_text(&val, compact), false),
                Err(err) => mcp_ipc_error_result(&err, compact),
            }
        }

        "koma_exec_macro" => {
            let Some(ws_s) = args.get("ws_id").and_then(Value::as_str) else {
                return mcp_text_result("missing required argument `ws_id`".into(), true);
            };
            let Some(macro_id) = args.get("macro_id").and_then(Value::as_str) else {
                return mcp_text_result("missing required argument `macro_id`".into(), true);
            };
            let ws_id = WorkspaceId::new(ws_s);
            let explicit_ep = args
                .get("expected_epoch")
                .and_then(Value::as_u64)
                .map(Epoch);
            let macro_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
            let payload = json!({
                "macro_id": macro_id,
                "args": macro_args,
                "fuel_budget": 100_000,
                "memory_limit_bytes": 4_194_304,
                "timeout_ms": 2_500,
            });
            let outcome =
                session.execute_fenced_mutation(&ws_id, explicit_ep, "lua.exec_tx", payload);
            format_mutate_outcome(outcome, compact)
        }

        dyn_name if dyn_name.starts_with("macro__") => {
            let Some(ws_s) = args.get("ws_id").and_then(Value::as_str) else {
                return mcp_text_result("missing required argument `ws_id`".into(), true);
            };
            let macro_id = dyn_name.trim_start_matches("macro__").replace("__", ".");
            let ws_id = WorkspaceId::new(ws_s);
            let explicit_ep = args
                .get("expected_epoch")
                .and_then(Value::as_u64)
                .map(Epoch);
            let macro_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
            let payload = json!({
                "macro_id": macro_id,
                "args": macro_args,
                "fuel_budget": 100_000,
                "memory_limit_bytes": 4_194_304,
                "timeout_ms": 2_500,
            });
            let outcome =
                session.execute_fenced_mutation(&ws_id, explicit_ep, "lua.exec_tx", payload);
            format_mutate_outcome(outcome, compact)
        }

        unknown => mcp_text_result(format!("unknown tool `{unknown}`"), true),
    }
}

fn format_mutate_outcome(outcome: MutateOutcome, compact: bool) -> Value {
    match outcome {
        MutateOutcome::Applied(val) => mcp_text_result(format_json_text(&val, compact), false),
        MutateOutcome::StaleReobserved {
            rejected_error,
            fresh_observation,
        } => {
            let notice = json!({
                "error": "ERR_STALE_EPOCH",
                "action_executed": false,
                "reason": rejected_error.message,
                "disposition": rejected_error.disposition,
                "new_observed_epoch": fresh_observation.epoch,
                "has_protected_blackout": fresh_observation.has_protected_blackout,
                "recovery": "Action was NOT executed to prevent TOCTOU mis-click. Attached image is ALREADY the fresh re-observed snapshot at `new_observed_epoch`; inspect it and recompute your next action.",
            });
            format_observation_tool_result(
                &fresh_observation,
                Some(format_json_text(&notice, compact)),
                true,
            )
        }
        MutateOutcome::Error(err) => mcp_ipc_error_result(&err, compact),
    }
}

fn format_observation_tool_result(
    obs: &CapturedPngObservation,
    override_text: Option<String>,
    is_error: bool,
) -> Value {
    let summary = override_text.unwrap_or_else(|| {
        serde_json::to_string_pretty(&json!({
            "ws_id": obs.ws_id,
            "epoch": obs.epoch,
            "width": obs.width,
            "height": obs.height,
            "has_protected_blackout": obs.has_protected_blackout,
            "grid_cells": obs.grid_cells,
        }))
        .unwrap()
    });

    json!({
        "isError": is_error,
        "content": [
            {
                "type": "text",
                "text": summary,
            },
            {
                "type": "image",
                "data": obs.png_base64,
                "mimeType": "image/png",
            }
        ]
    })
}

fn mcp_text_result(text: String, is_error: bool) -> Value {
    json!({
        "isError": is_error,
        "content": [
            {
                "type": "text",
                "text": text,
            }
        ]
    })
}

fn mcp_ipc_error_result(err: &koma_proto::IpcError, compact: bool) -> Value {
    let guidance = match err.symbol {
        ErrorSymbol::ErrHumanPreempted => {
            "Human currently holds Mod+G takeover on this workspace. Do not retry blindly; wait or ask the user."
        }
        ErrorSymbol::ErrProtectedSurface => {
            "A credential vault, Polkit prompt, or lockscreen is focused. Ask the human to complete verification via Mod+G."
        }
        _ => "Check tool arguments and workspace permissions.",
    };
    let payload = json!({
        "code": err.code,
        "symbol": err.symbol,
        "message": err.message,
        "current_epoch": err.current_epoch,
        "retryable": err.retryable,
        "disposition": err.disposition,
        "guidance": guidance,
    });
    mcp_text_result(format_json_text(&payload, compact), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn builtin_mcp_tool_schemas_expose_koma_path_and_vseat_primitives() {
        let schemas = builtin_mcp_tool_schemas(&[]);
        let path_tool = schemas
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("koma_path"))
            .expect("koma_path tool schema must exist");
        let req = path_tool["inputSchema"]["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v.as_str() == Some("ws_id")));
        assert!(req.iter().any(|v| v.as_str() == Some("waypoints")));
        let props = path_tool["inputSchema"]["properties"].as_object().unwrap();
        for key in [
            "ws_id",
            "expected_epoch",
            "waypoints",
            "spline",
            "button",
            "hold_start_ms",
            "hold_end_ms",
            "kinematics",
            "modifiers",
            "label",
            "bow",
            "overshoot",
            "tremor",
            "compact",
        ] {
            assert!(
                props.contains_key(key),
                "missing property {key} in koma_path"
            );
        }

        for expected_tool in [
            "koma_ping",
            "koma_cursor_settle",
            "koma_wheel",
            "koma_relative_move",
            "koma_hit_test",
            "koma_cursor_label",
        ] {
            assert!(
                schemas
                    .iter()
                    .any(|s| s.get("name").and_then(Value::as_str) == Some(expected_tool)),
                "missing builtin tool {expected_tool}"
            );
        }
    }

    #[test]
    fn offline_mcp_discovery_and_dual_stdio_framing_work() {
        let mut session = LazyKomaSession::new(false);

        // 1) NDJSON roundtrip including notification (no reply), resources/list, prompts/list, tools/list, and koma_ping
        let ndjson_input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/list","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"prompts/list","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"koma_ping","arguments":{"compact":true}}}"#,
            "\n",
        );
        let mut reader = Cursor::new(ndjson_input.as_bytes());
        let mut out = Vec::new();
        run_stdio_mcp_loop_with_rw(&mut session, &mut reader, &mut out).unwrap();
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 5, "notification must not produce a reply");
        assert_eq!(lines[0]["id"], json!(1));
        assert!(lines[0]["result"]["capabilities"]["resources"].is_object());
        assert!(lines[0]["result"]["capabilities"]["prompts"].is_object());
        assert_eq!(lines[1]["result"]["resources"], json!([]));
        assert_eq!(lines[2]["result"]["prompts"], json!([]));
        assert!(
            lines[3]["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "koma_hit_test")
        );
        assert_eq!(lines[4]["result"]["isError"], json!(false));

        // 2) Content-Length LSP framing roundtrip
        let ping_body = r#"{"jsonrpc":"2.0","id":9,"method":"ping","params":{}}"#;
        let framed = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}",
            ping_body.len(),
            ping_body
        );
        let mut cl_reader = Cursor::new(framed.as_bytes());
        let mut cl_out = Vec::new();
        run_stdio_mcp_loop_with_rw(&mut session, &mut cl_reader, &mut cl_out).unwrap();
        let cl_text = String::from_utf8(cl_out).unwrap();
        assert!(
            cl_text.starts_with("Content-Length: "),
            "expected Content-Length response header, got: {cl_text}"
        );
        let (_, json_part) = cl_text.split_once("\r\n\r\n").unwrap();
        let parsed: Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(parsed["id"], json!(9));
        assert_eq!(parsed["result"], json!({}));
    }
}
