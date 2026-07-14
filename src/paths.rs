use std::path::PathBuf;

use anyhow::{Context, Result};

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
        Ok(home_dir()?.join("dlgt.sock"))
    }
}

pub fn database_path() -> Result<PathBuf> {
    // v1 intentionally has no compatibility surface for the pre-contract
    // schema, which exposed turns as public resources.
    Ok(home_dir()?.join("state-v1.db"))
}
