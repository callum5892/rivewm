//! The layout file: what rivewm saves so a restart puts every window back
//! where it was.
//!
//! Windows are identified by their handle (HWND), which stays valid while
//! the window exists, so this covers restarting rivewm itself, not a reboot.
//! Each one's process and class are saved too, so a handle Windows has since
//! reused for a different window isn't mistaken for the old one.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rivewm_core::{SavedWorkspace, WindowId};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedState {
    pub version: u32,
    pub workspaces: Vec<SavedWorkspace>,
    /// What each saved window was, by handle (as a decimal string).
    pub windows: HashMap<String, Identity>,
}

/// Enough to tell whether a window handle still belongs to the same window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub process: Option<String>,
    pub class: String,
}

impl SavedState {
    pub fn new(workspaces: Vec<SavedWorkspace>, windows: HashMap<WindowId, Identity>) -> Self {
        Self {
            version: VERSION,
            workspaces,
            windows: windows
                .into_iter()
                .map(|(id, identity)| (id.0.to_string(), identity))
                .collect(),
        }
    }

    pub fn identity(&self, id: WindowId) -> Option<&Identity> {
        self.windows.get(&id.0.to_string())
    }
}

/// `%LOCALAPPDATA%\rivewm\layout.json`.
pub fn path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("rivewm")
        .join("layout.json")
}

/// The saved layout, if there is a usable one.
pub fn load() -> Option<SavedState> {
    let text = std::fs::read_to_string(path()).ok()?;
    match serde_json::from_str::<SavedState>(&text) {
        Ok(state) if state.version == VERSION => Some(state),
        Ok(_) => {
            tracing::warn!("ignoring saved layout from a different rivewm version");
            None
        }
        Err(err) => {
            tracing::warn!(%err, "ignoring unreadable saved layout");
            None
        }
    }
}

/// Writes the layout, via a temporary file so a crash mid-write can't leave
/// a truncated one behind.
pub fn save(state: &SavedState) -> Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("couldn't write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("couldn't replace {}", path.display()))
}
