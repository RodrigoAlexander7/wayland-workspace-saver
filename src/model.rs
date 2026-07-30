//! Modelo serializable de la sesión: lo que se guarda en session.json.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionSnapshot {
    pub saved_at_epoch: u64,
    pub workspaces: Vec<WorkspaceSnap>,
    pub windows: Vec<WindowSnap>,
    /// binario del navegador -> perfiles activos (--profile-directory)
    #[serde(default)]
    pub browser_profiles: BTreeMap<String, Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WorkspaceSnap {
    pub name: String,
    pub coordinates: Vec<u32>,
    /// 0 = floating, 1 = tiling
    pub tiling: Option<u32>,
    pub active: bool,
    pub pinned: bool,
    pub output: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WindowSnap {
    pub app_id: String,
    pub title: String,
    pub workspace: Option<String>,
    pub output: Option<String>,
    pub maximized: bool,
    pub minimized: bool,
    pub fullscreen: bool,
    pub sticky: bool,
    pub geometry: Option<(i32, i32, i32, i32)>,
}
