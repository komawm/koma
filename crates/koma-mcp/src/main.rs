#![forbid(unsafe_code)]

//! Standard `stdio` Model Context Protocol server bridging `koma-agent.sock` to AI agents (`Tier 3`).
//!
//! Usage:
//!   `KOMA_CAPABILITY_TOKEN=koma_cap_... koma-mcp`
//!   `koma-mcp --auto-mint-admin`   (local developer convenience mode)
//!   `koma-mcp --print-tools`       (offline schema dump for inspection)

pub mod agent_client;
pub mod epoch_loop;
pub mod mcp_stdio;
pub mod png_codec;

use std::env;

use crate::mcp_stdio::{LazyKomaSession, builtin_mcp_tool_schemas, run_stdio_mcp_loop};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "--print-tools") {
        let schemas = builtin_mcp_tool_schemas(&[]);
        println!("{}", serde_json::to_string_pretty(&schemas).unwrap());
        return;
    }

    let auto_mint = args.iter().any(|a| a == "--auto-mint-admin");
    let mut session = LazyKomaSession::new(auto_mint);

    if let Err(err) = run_stdio_mcp_loop(&mut session) {
        eprintln!("koma-mcp stdio fatal I/O error: {err}");
        std::process::exit(1);
    }
}
