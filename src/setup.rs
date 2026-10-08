use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn find_codewhale_binary(configured: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = configured {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        bail!(
            "configured codewhale binary not found at {}",
            path.display()
        );
    }

    if let Ok(path) = which("codewhale") {
        return Ok(path);
    }

    if let Ok(path) = which("codewhale-cli") {
        return Ok(path);
    }

    let home = dirs::home_dir().context("could not resolve home directory")?;
    let candidates = [
        home.join(".local/bin/codewhale"),
        home.join(".cargo/bin/codewhale"),
        home.join(".cargo/bin/codewhale-cli"),
        PathBuf::from("/usr/local/bin/codewhale"),
        PathBuf::from("/usr/bin/codewhale"),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    bail!(
        "codewhale binary not found. Install it with: cargo install codewhale-cli\n\
         Or set --codewhale-bin /path/to/codewhale"
    )
}

/// Resolve a codewhale executable from a name or a path: an existing file is
/// used directly, otherwise the value is looked up on `PATH`.
pub fn resolve_codewhale_binary(spec: &str) -> Result<PathBuf> {
    let as_path = PathBuf::from(spec);
    if as_path.is_file() {
        return Ok(as_path);
    }
    if let Ok(path) = which(spec) {
        return Ok(path);
    }
    bail!("could not find a codewhale executable named or located at {spec}")
}

pub fn install_codewhale() -> Result<()> {
    println!("Installing codewhale-cli via cargo...");
    let status = Command::new("cargo")
        .args(["install", "codewhale-cli", "--locked"])
        .status()
        .context("failed to run cargo install")?;
    if !status.success() {
        bail!("cargo install codewhale-cli failed with {status}");
    }
    println!("codewhale-cli installed successfully.");
    Ok(())
}

fn which(name: &str) -> Result<PathBuf> {
    let path_var = std::env::var_os("PATH").context("PATH not set")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("{name} not found in PATH")
}
