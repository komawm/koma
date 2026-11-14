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

use crate::{
    agent_client::KomaAgentSession,
    mcp_stdio::{builtin_mcp_tool_schemas, run_stdio_mcp_loop},
};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "--print-tools") {
        let schemas = builtin_mcp_tool_schemas(&[]);
        println!("{}", serde_json::to_string_pretty(&schemas).unwrap());
        return;
    }

    let auto_mint = args.iter().any(|a| a == "--auto-mint-admin");
    let mut session = match KomaAgentSession::from_env(auto_mint) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("koma-mcp authentication failed: {err}");
            std::process::exit(1);
        }
    };

    if let Err(err) = run_stdio_mcp_loop(&mut session) {
        eprintln!("koma-mcp stdio fatal I/O error: {err}");
        std::process::exit(1);
    }
}
