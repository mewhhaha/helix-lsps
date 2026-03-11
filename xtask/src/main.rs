use std::{
    env,
    path::PathBuf,
    process::{Command, ExitStatus},
};

use anyhow::{Context, Result, bail};

const PACKAGES: &[(&str, &str)] = &[
    ("eslint-lsp", "eslint-lsp"),
    ("prettier-lsp", "prettier-lsp"),
    ("tsgo-lsp", "tsgo-lsp"),
    ("oxc-lsp", "oxc-lsp"),
];

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_usage();
        bail!("missing xtask command");
    };

    match command.as_str() {
        "install" => install_all(args.collect()),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            bail!("unknown xtask command: {other}");
        }
    }
}

fn install_all(extra_args: Vec<String>) -> Result<()> {
    let workspace_root = workspace_root()?;

    for (package, bin) in PACKAGES {
        let manifest_path = workspace_root.join(package).join("Cargo.toml");
        let status = Command::new("cargo")
            .arg("install")
            .arg("--locked")
            .arg("--path")
            .arg(package)
            .arg("--bin")
            .arg(bin)
            .args(&extra_args)
            .current_dir(&workspace_root)
            .status()
            .with_context(|| format!("failed to spawn cargo install for {package}"))?;

        ensure_success(status, package, &manifest_path)?;
    }

    Ok(())
}

fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("xtask manifest directory has no parent"))
}

fn ensure_success(status: ExitStatus, package: &str, manifest_path: &PathBuf) -> Result<()> {
    if status.success() {
        return Ok(());
    }

    bail!(
        "cargo install failed for {package} ({}) with status {status}",
        manifest_path.display()
    );
}

fn print_usage() {
    eprintln!(
        "Usage:\n  cargo install-lsps\n  cargo run -p xtask -- install [cargo-install-args...]"
    );
}
