//! Persistent user settings (backend URL, session token) in the OS config dir.
//! Mirrors the Svelte app's localStorage keys (`lumen.backendUrl`,
//! `lumen.token`). A plain JSON file keeps parity without pulling in a
//! keyring backend; OS-keychain storage is a documented future hardening.

use std::path::PathBuf;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

pub const DEFAULT_BACKEND_URL: &str = "http://localhost:8787";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SettingsData {
    #[serde(default)]
    pub backend_url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    /// Voice noise-suppression model, as `SuppressorModel::as_str()` —
    /// "fastenhancer" | "ns-only". Parsed by the app (lumen-core doesn't
    /// depend on lumen-voice). `None` = the client's default.
    #[serde(default)]
    pub suppressor_model: Option<String>,
    /// Feed the playback reference into AEC3 (echo cancellation). Speakers
    /// users need it; headphones users should leave it off (this webrtc build
    /// corrupts the send when render is fed). `None` = the client's default.
    #[serde(default)]
    pub aec_enabled: Option<bool>,
}

pub struct Settings {
    dir: PathBuf,
    data: RwLock<SettingsData>,
}

impl Settings {
    /// Load from `<config_dir>/lumen/settings.json`; missing/corrupt → defaults.
    pub fn load() -> Self {
        let dir = directories::ProjectDirs::from("io", "lumen", "Lumen")
            .map(|p| p.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let path = dir.join("settings.json");
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { dir, data: RwLock::new(data) }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    fn persist(&self) {
        let data = self.data.read();
        if let Some(json) = serde_json::to_string_pretty(&*data).ok() {
            let _ = std::fs::create_dir_all(&self.dir);
            let _ = std::fs::write(self.path(), json);
        }
    }

    pub fn backend_url(&self) -> String {
        self.data
            .read()
            .backend_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BACKEND_URL.to_string())
    }

    pub fn set_backend_url(&self, url: String) {
        self.data.write().backend_url = Some(url.trim_end_matches('/').to_string());
        self.persist();
    }

    pub fn token(&self) -> Option<String> {
        self.data.read().token.clone()
    }

    pub fn set_token(&self, token: Option<String>) {
        self.data.write().token = token;
        self.persist();
    }

    pub fn suppressor_model(&self) -> Option<String> {
        self.data.read().suppressor_model.clone()
    }

    pub fn set_suppressor_model(&self, model: String) {
        self.data.write().suppressor_model = Some(model);
        self.persist();
    }

    pub fn aec_enabled(&self) -> Option<bool> {
        self.data.read().aec_enabled
    }

    pub fn set_aec_enabled(&self, enabled: bool) {
        self.data.write().aec_enabled = Some(enabled);
        self.persist();
    }
}
