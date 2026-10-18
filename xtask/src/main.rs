//! Repository checks. Run as `cargo xtask <command>`.

// Developer tool: a failed unwrap is a failed check.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod fold_table;
mod layers;

use std::path::Path;
use std::process::ExitCode;

use cargo_metadata::MetadataCommand;

const USAGE: &str = "usage: cargo xtask layers | fold-table [--check]";

/// `cargo metadata --no-deps` for the workspace that contains this crate.
fn metadata_command() -> MetadataCommand {
    let mut command = MetadataCommand::new();
    command
        .manifest_path(Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.toml"))
        .no_deps();
    command
}

fn run_layers() -> ExitCode {
    let metadata = match metadata_command().exec() {
        Ok(metadata) => metadata,
        Err(err) => {
            eprintln!("layers: cargo metadata failed: {err}");
            return ExitCode::FAILURE;
        }
    };
    let violations = layers::check(&metadata);
    for violation in &violations {
        eprintln!("{violation}");
    }
    if violations.is_empty() {
        println!("layers: {} crates, ok", metadata.workspace_packages().len());
        ExitCode::SUCCESS
    } else {
        eprintln!("layers: {} violation(s)", violations.len());
        ExitCode::FAILURE
    }
}

fn run_fold_table(check: bool) -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    match fold_table::run(&root, check) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("fold-table: {err}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["layers"] => run_layers(),
        ["fold-table"] => run_fold_table(false),
        ["fold-table", "--check"] => run_fold_table(true),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
