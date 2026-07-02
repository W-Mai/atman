use std::env;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let path = match env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: cargo run -p atman-dsl --example parse -- <file.at>");
            return ExitCode::from(2);
        }
    };
    let src = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::from(1);
        }
    };
    match atman_dsl::parse::parse_file(&src) {
        Ok(file) => {
            println!("=== ROUND-TRIP (parsed → printed) ===\n");
            println!("{}", atman_dsl::print::print_file(&file));
            println!("=== AST ===\n");
            println!("{file:#?}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("parse error: {e}");
            ExitCode::from(1)
        }
    }
}
