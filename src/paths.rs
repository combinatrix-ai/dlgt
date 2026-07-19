use std::path::PathBuf;

use anyhow::{Context, Result};

const SOCKET_NAME: &str = "dlgt.sock";

pub fn home_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DLGT_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".dlgt"))
}

pub fn socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DLGT_SOCKET") {
        Ok(PathBuf::from(path))
    } else {
        socket_path_for_version(env!("CARGO_PKG_VERSION"))
    }
}

pub fn runtime_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join("run"))
}

pub fn socket_path_for_version(version: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(version).join(SOCKET_NAME))
}

pub fn runtime_sockets() -> Result<Vec<PathBuf>> {
    let directory = runtime_dir()?;
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", directory.display()));
        }
    };
    let mut sockets = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path().join(SOCKET_NAME))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    sockets.sort();
    Ok(sockets)
}
