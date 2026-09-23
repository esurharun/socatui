//! Persistence of tunnel definitions as JSON.

use crate::tunnel::TunnelConfig;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Config path precedence: CLI argument, `$SOCATUI_CONFIG`, then
/// `<config dir>/socatui/tunnels.json` (e.g. `~/.config/socatui/tunnels.json`).
pub fn resolve_path(arg: Option<String>) -> PathBuf {
    if let Some(a) = arg {
        return PathBuf::from(a);
    }
    if let Ok(env) = std::env::var("SOCATUI_CONFIG") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("socatui")
        .join("tunnels.json")
}

pub fn load(path: &Path) -> Result<Vec<TunnelConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if data.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))
}

pub fn save(path: &Path, tunnels: &[TunnelConfig]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let data = serde_json::to_string_pretty(tunnels)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}
