//! Stdio Model Context Protocol server (`koma mcp`).
//!
//! This module is the former standalone `koma-mcp` binary, now a subcommand of
//! `koma`. It bridges `koma-agent.sock` to AI agents with `CapabilityToken`
//! authentication, zero-copy `SCM_RIGHTS` PNG capture, and `ERR_STALE_EPOCH`
//! re-observation. Running the server never opens a display, seat or DRM device.
//!
//! Usage:
//!   `KOMA_CAPABILITY_TOKEN=koma_cap_... koma mcp`
//!   `koma mcp --auto-mint-admin`   (local developer convenience mode)
//!   `koma mcp --print-tools`       (offline schema dump for inspection)
//!
//! Documentation: <https://github.com/komawm/docs/blob/main/docs/IPC_SPEC.md>.

// The MCP surface is a protocol adapter: several typed fields and helpers are
// read only by clients or by the protocol tests, not by another in-crate caller.
#![allow(dead_code)]

pub mod agent_client;
pub mod epoch_loop;
pub mod mcp_stdio;
pub mod png_codec;

use crate::mcp::mcp_stdio::{LazyKomaSession, builtin_mcp_tool_schemas, run_stdio_mcp_loop};

/// Runs the stdio MCP server and returns the process exit status.
///
/// `--print-tools` renders the schemas and returns without touching the agent
/// socket. Time O(1) for the flag path; the server loop itself blocks on stdio.
pub fn run(print_tools: bool, auto_mint_admin: bool) -> i32 {
    if print_tools {
        let schemas = builtin_mcp_tool_schemas(&[]);
        match serde_json::to_string_pretty(&schemas) {
            Ok(rendered) => {
                println!("{rendered}");
                return 0;
            }
            Err(err) => {
                eprintln!("koma mcp: failed to render tool schemas: {err}");
                return 1;
            }
        }
    }

    let mut session = LazyKomaSession::new(auto_mint_admin);
    match run_stdio_mcp_loop(&mut session) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("koma mcp stdio fatal I/O error: {err}");
            1
        }
    }
}
