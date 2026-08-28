//! hopd — serve a .hop application over CBOR-binary WebSockets.
//!
//! Usage: hopd <app.hop> [http_port] [ws_port] [--data <dir>] [--web <pkg_dir>] [--log]
//!
//! Compiles the app with hopc, runs the server VM, and routes hop packets
//! over WebSockets. Real tabs get the shell page + glue.js and the wasm
//! interpreter from `--web` (default `./hop-web/pkg`, a
//! `wasm-pack build hop-web --target web` output). If the app declares
//! `schema` and `fn reduce`, the server VM opens a durable store (JSONL
//! log + RocksDB projection) at `--data` (default `./hop-data`). `--log`
//! dumps every packet in diagnostic notation.

use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input = None;
    let mut ports: Vec<u16> = Vec::new();
    let mut data_dir = PathBuf::from("hop-data");
    let mut pkg_dir = PathBuf::from("hop-web/pkg");
    let mut log_packets = false;
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
            "--web" => {
                i += 1;
                match args.get(i) {
                    Some(p) => pkg_dir = PathBuf::from(p),
                    None => {
                        eprintln!("hopd: --web needs a directory");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--log" => log_packets = true,
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
        eprintln!("usage: hopd <app.hop> [http_port] [ws_port] [--data <dir>] [--web <pkg_dir>] [--log]");
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
    let prog = match hoprt::compiler::compile(&src) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hopd: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "[hopd] compiled {path} ({} functions, {} hops)",
        prog.fns.len(),
        prog.hops.len()
    );

    match hoprt::serve::serve(Rc::new(prog), src, http_port, ws_port, data_dir, pkg_dir, log_packets) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hopd: {e}");
            ExitCode::FAILURE
        }
    }
}
