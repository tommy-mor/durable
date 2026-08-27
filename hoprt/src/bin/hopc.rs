//! hopc — compile .hop source to Lua for the hoprt runtime.
//!
//! Usage: hopc <input.hop> [-o <output.lua>]   (default: stdout)

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input = None;
    let mut output = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                output = args.get(i).cloned();
            }
            a => input = Some(a.to_string()),
        }
        i += 1;
    }
    let Some(input) = input else {
        eprintln!("usage: hopc <input.hop> [-o <output.lua>]");
        return ExitCode::FAILURE;
    };

    let src = match std::fs::read_to_string(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hopc: read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let lua = match hoprt::compiler::compile(&src) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hopc: {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match output {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, lua) {
                eprintln!("hopc: write {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
        None => print!("{lua}"),
    }
    ExitCode::SUCCESS
}
