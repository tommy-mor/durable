//! hopc — compile .hop source to Hop IR and print the listing.
//!
//! Usage: hopc <input.hop> [-o <output.txt>] [--explain]
//! --explain appends the hop-shipping table: what crosses each hop, per
//! hop id — the lamp over the compiler's liveness analysis.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input = None;
    let mut output = None;
    let mut explain = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                output = args.get(i).cloned();
            }
            "--explain" => explain = true,
            a => input = Some(a.to_string()),
        }
        i += 1;
    }
    let Some(input) = input else {
        eprintln!("usage: hopc <input.hop> [-o <output.txt>] [--explain]");
        return ExitCode::FAILURE;
    };

    let src = match std::fs::read_to_string(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hopc: read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let prog = match hoprt::compiler::compile(&src) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hopc: {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    for w in &prog.warnings {
        eprintln!("hopc: warning: {w}");
    }
    let mut listing = prog.listing();
    if explain {
        listing.push_str(&prog.explain_listing());
    }
    match output {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, listing) {
                eprintln!("hopc: write {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
        None => print!("{listing}"),
    }
    ExitCode::SUCCESS
}
