//! hopd — serve a .hop application to real browsers.
//!
//! Usage: hopd <app.hop> [http_port] [ws_port] [--data <dir>]
//!
//! Compiles the app with hopc, serves the page + Lua to browsers over HTTP,
//! runs the server VM, and routes hop packets over WebSockets. If the app
//! declares `schema` and `fn reduce`, the server VM opens a durable store
//! (JSONL log + RocksDB projection) at `--data` (default `./hop-data`).

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input = None;
    let mut ports: Vec<u16> = Vec::new();
    let mut data_dir = PathBuf::from("hop-data");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data" => {
                i += 1;
                match args.get(i) {
                    Some(p) => data_dir = PathBuf::from(p),
                    None => {
                        eprintln!("hopd: --data needs a directory");
                        return ExitCode::FAILURE;
                    }
                }
            }
            a if a.starts_with('-') => {
                eprintln!("hopd: unknown flag {a}");
                return ExitCode::FAILURE;
            }
            a if input.is_none() => input = Some(a.to_string()),
            a => match a.parse::<u16>() {
                Ok(p) => ports.push(p),
                Err(_) => {
                    eprintln!("hopd: unexpected argument {a}");
                    return ExitCode::FAILURE;
                }
            },
        }
        i += 1;
    }
    let Some(path) = input else {
        eprintln!("usage: hopd <app.hop> [http_port] [ws_port] [--data <dir>]");
        return ExitCode::FAILURE;
    };
    let http_port = ports.first().copied().unwrap_or(9000);
    let ws_port = ports.get(1).copied().unwrap_or(9001);

    let src = match std::fs::read_to_string(&path) {
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

    match hoprt::serve::serve(lua, http_port, ws_port, data_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hopd: {e}");
            ExitCode::FAILURE
        }
    }
}
