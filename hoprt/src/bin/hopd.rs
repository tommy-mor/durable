//! hopd — serve a .hop application to real browsers.
//!
//! Usage: hopd <app.hop> [http_port] [ws_port]   (defaults 9000, 9001)
//!
//! Compiles the app with hopc, serves the page + Lua to browsers over HTTP,
//! runs the server VM, and routes hop packets over WebSockets. Open the
//! page in two tabs; they are two sessions of one cluster.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: hopd <app.hop> [http_port] [ws_port]");
        return ExitCode::FAILURE;
    };
    let http_port: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(9000);
    let ws_port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(9001);

    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hopd: read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let lua = match hoprt::compiler::compile(&src) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hopd: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("[hopd] compiled {path} ({} lines of Lua)", lua.lines().count());

    match hoprt::serve::serve(lua, http_port, ws_port) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hopd: {e}");
            ExitCode::FAILURE
        }
    }
}
