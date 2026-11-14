//! Model Context Protocol (`2024-11-05` / `2025-03-26`) JSON-RPC 2.0 `stdio` server.
//!
//! Aligns `CallToolResult` (`content` text + `image/png` blocks & `isError` semantics) with
//! `open-computer-use` while exposing Koma's isolated headless workspaces, `grid_225` zero-copy
//! captures, Zero-IME text/TSV injection, and parameterized `Phodopus` macro skills.

use std::io::{self, BufRead as _, Write as _};

use koma_lua::MacroDefinition;
use koma_proto::{Epoch, ErrorSymbol, OverlayKind, WorkspaceId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent_client::{CapturedPngObservation, KomaAgentSession, MutateOutcome};

pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub const KOMA_MCP_INSTRUCTIONS: &str = "Koma is an Agent-native Wayland tiling compositor with isolated Headless workspaces, per-workspace virtual seats (vSeat), Zero-IME text/MIME injection, and monotonic Epoch concurrency fencing. 1) Always call `koma_scene_inventory` or `koma_capture_frame` before mutating a workspace so you hold a Fresh `expected_epoch`. 2) Prefer isolated Headless workspaces (`koma_ws_create`) so the human user's physical monitor and seat0 clipboard remain completely undisturbed. 3) If a tool returns `ERR_STALE_EPOCH` (`isError: true`), the action was safely aborted BEFORE execution and the attached PNG image is ALREADY a fresh re-observed snapshot at `new_observed_epoch`—inspect the new image and recompute your next action using `new_observed_epoch`. 4) If a tool returns `ERR_HUMAN_PREEMPTED` or `ERR_PROTECTED_SURFACE`, pause and ask the user.";

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
            "name": "koma_scene_inventory",
            "description": "Lists accessible Headed and Headless workspaces, current Epoch fences, active Deck window, and background Stash windows.",
            "inputSchema": { "type": "object", "properties": {} }
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
                    "clicks": { "type": "integer", "minimum": 1, "maximum": 3, "default": 1 }
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
                    "duration_ms": { "type": "integer", "default": 110 }
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
    ];

    for m in dynamic_macros {
        tools.push(macro_to_mcp_tool_schema(m));
    }
    tools
}

pub fn run_stdio_mcp_loop(session: &mut KomaAgentSession) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break; // EOF on stdin
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(json!({ "code": -32700, "message": format!("parse error: {e}") })),
                };
                writeln!(writer, "{}", serde_json::to_string(&err_resp).unwrap())?;
                writer.flush()?;
                continue;
            }
        };

        // JSON-RPC notifications (no `id`, e.g. `notifications/initialized`) produce no reply frame
        let Some(req_id) = req.id.clone() else {
            continue;
        };

        let resp = handle_mcp_request(session, req_id, &req.method, &req.params);
        writeln!(writer, "{}", serde_json::to_string(&resp).unwrap())?;
        writer.flush()?;
    }

    Ok(())
}

pub fn handle_mcp_request(
    session: &mut KomaAgentSession,
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
                    "tools": { "listChanged": true }
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
        "tools/list" => {
            let macros = session.fetch_registered_macros();
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

fn dispatch_tool_call(session: &mut KomaAgentSession, name: &str, args: Value) -> Value {
    match name {
        "koma_scene_inventory" => match session.fetch_inventory_and_record_epochs() {
            Ok(inv) => mcp_text_result(serde_json::to_string_pretty(&inv).unwrap(), false),
            Err(e) => mcp_ipc_error_result(&e),
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
                    mcp_text_result(serde_json::to_string_pretty(&val).unwrap(), false)
                }
                Err(e) => mcp_ipc_error_result(&e),
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
                Err(e) => mcp_ipc_error_result(&e),
            }
        }

        "koma_click" | "koma_drag" | "koma_key" | "koma_inject_text" | "koma_paste_mime"
        | "koma_win_activate" => {
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
                "koma_key" => "vseat.key",
                "koma_inject_text" => "vseat.inject_text",
                "koma_paste_mime" => "vseat.paste_mime",
                "koma_win_activate" => "win.activate",
                _ => unreachable!(),
            };
            let outcome = session.execute_fenced_mutation(&ws_id, explicit_ep, ipc_method, args);
            format_mutate_outcome(outcome)
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
            format_mutate_outcome(outcome)
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
            format_mutate_outcome(outcome)
        }

        unknown => mcp_text_result(format!("unknown tool `{unknown}`"), true),
    }
}

fn format_mutate_outcome(outcome: MutateOutcome) -> Value {
    match outcome {
        MutateOutcome::Applied(val) => {
            mcp_text_result(serde_json::to_string_pretty(&val).unwrap(), false)
        }
        MutateOutcome::StaleReobserved {
            rejected_error,
            fresh_observation,
        } => {
            let notice = json!({
                "error": "ERR_STALE_EPOCH",
                "action_executed": false,
                "reason": rejected_error.message,
                "new_observed_epoch": fresh_observation.epoch,
                "has_protected_blackout": fresh_observation.has_protected_blackout,
                "recovery": "Action was NOT executed to prevent TOCTOU mis-click. Attached image is ALREADY the fresh re-observed snapshot at `new_observed_epoch`; inspect it and recompute your next action.",
            });
            format_observation_tool_result(
                &fresh_observation,
                Some(serde_json::to_string_pretty(&notice).unwrap()),
                true,
            )
        }
        MutateOutcome::Error(err) => mcp_ipc_error_result(&err),
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

fn mcp_ipc_error_result(err: &koma_proto::IpcError) -> Value {
    let guidance = match err.symbol {
        ErrorSymbol::ErrHumanPreempted => {
            "Human currently holds Mod+G takeover on this workspace. Do not retry blindly; wait or ask the user."
        }
        ErrorSymbol::ErrProtectedSurface => {
            "A credential vault, Polkit prompt, or lockscreen is focused. Ask the human to complete verification via Mod+G."
        }
        _ => "Check tool arguments and workspace permissions.",
    };
    mcp_text_result(
        serde_json::to_string_pretty(&json!({
            "code": err.code,
            "symbol": err.symbol,
            "message": err.message,
            "current_epoch": err.current_epoch,
            "retryable": err.retryable,
            "guidance": guidance,
        }))
        .unwrap(),
        true,
    )
}
