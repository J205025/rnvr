use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub camera: CameraConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub main_stream_path: String,
    pub sub_stream_path: String,
    #[serde(default = "default_enable_audio")]
    pub enable_audio: bool,
    #[serde(default = "default_use_hw_accel")]
    pub use_hw_accel: bool,
    #[serde(default = "default_preview_max_fps")]
    pub preview_max_fps: u32,
    #[serde(default = "default_preview_enabled")]
    pub preview_enabled: bool,
    #[serde(default)]
    pub mock_mode: bool,
}

fn default_enable_audio() -> bool {
    true
}

fn default_use_hw_accel() -> bool {
    true
}

fn default_preview_max_fps() -> u32 {
    12
}

fn default_preview_enabled() -> bool {
    true
}

fn default_segment_duration_mins() -> u64 {
    30
}

fn default_retention_days() -> u32 {
    30
}

fn default_cleanup_cron() -> String {
    "0 3 * * 2".to_string()
}

fn default_max_storage_gb() -> f64 {
    100.0
}

fn default_auto_start_recording() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub record_dir: PathBuf,
    #[serde(default = "default_segment_duration_mins")]
    pub segment_duration_mins: u64,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_cleanup_cron")]
    pub cleanup_cron: String,
    #[serde(default = "default_max_storage_gb")]
    pub max_storage_gb: f64,
    #[serde(default = "default_auto_start_recording")]
    pub auto_start_recording: bool,
}

impl StorageConfig {
    /// 取得單一錄影切片秒數 (分鐘 * 60)
    pub fn segment_duration_secs(&self) -> u64 {
        self.segment_duration_mins.max(1).saturating_mul(60)
    }
}

impl CameraConfig {
    /// 建構完整的 RTSP URL，並對帳號密碼進行 URL Encode 以支援特殊字元。
    /// 若使用者於 stream_path 直接填入完整 `rtsp://...` 網址亦能直接支援。
    pub fn build_rtsp_url(&self, stream_path: &str) -> String {
        let trimmed_path = stream_path.trim();
        if trimmed_path.starts_with("rtsp://") || trimmed_path.starts_with("rtsps://") {
            return trimmed_path.to_string();
        }

        let trimmed_ip = self.ip.trim();
        if trimmed_ip.starts_with("rtsp://") || trimmed_ip.starts_with("rtsps://") {
            return trimmed_ip.to_string();
        }

        let path = if trimmed_path.starts_with('/') {
            trimmed_path.to_string()
        } else {
            format!("/{}", trimmed_path)
        };

        if self.username.trim().is_empty() {
            format!("rtsp://{}:{}{}", trimmed_ip, self.port, path)
        } else {
            let user = urlencoding::encode(self.username.trim());
            let pass = urlencoding::encode(&self.password);
            format!(
                "rtsp://{}:{}@{}:{}{}",
                user, pass, trimmed_ip, self.port, path
            )
        }
    }

    /// 取得 3K 主碼流 RTSP URL (錄影專用)
    pub fn main_rtsp_url(&self) -> String {
        self.build_rtsp_url(&self.main_stream_path)
    }

    /// 取得 720p 副碼流 RTSP URL (預覽專用)
    pub fn sub_rtsp_url(&self) -> String {
        self.build_rtsp_url(&self.sub_stream_path)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            camera: CameraConfig {
                name: "大門監控 (Foscam C5M)".to_string(),
                ip: "192.168.1.101".to_string(),
                port: 88,
                username: "nvr_admin".to_string(),
                password: "your_password".to_string(),
                main_stream_path: "/videoMain".to_string(),
                sub_stream_path: "/videoSub".to_string(),
                enable_audio: true,
                use_hw_accel: true,
                preview_max_fps: 12,
                preview_enabled: true,
                mock_mode: false,
            },
            storage: StorageConfig {
                record_dir: PathBuf::from("/home/ubuntu/Foscam_Recordings"),
                segment_duration_mins: 30,
                retention_days: 30,
                cleanup_cron: "0 3 * * 2".to_string(),
                max_storage_gb: 100.0,
                auto_start_recording: true,
            },
        }
    }
}

impl AppConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let content = fs::read_to_string(path_ref)
            .with_context(|| format!("無法讀取設定檔: {}", path_ref.display()))?;
        let mut cfg: AppConfig = toml::from_str(&content)
            .with_context(|| format!("設定檔 TOML 格式解析失敗: {}", path_ref.display()))?;

        if cfg.camera.preview_max_fps == 0 || cfg.camera.preview_max_fps > 60 {
            cfg.camera.preview_max_fps = default_preview_max_fps();
        }
        if cfg.storage.segment_duration_mins < 1 {
            cfg.storage.segment_duration_mins = 1;
        }
        if cfg.storage.retention_days < 1 {
            cfg.storage.retention_days = 1;
        }
        if cfg.storage.cleanup_cron.trim().is_empty() {
            cfg.storage.cleanup_cron = default_cleanup_cron();
        }
        if cfg.storage.max_storage_gb <= 0.1 {
            cfg.storage.max_storage_gb = 1.0;
        }

        Ok(cfg)
    }

    /// 將設定寫回 TOML 設定檔
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path_ref = path.as_ref();
        if let Some(parent) = path_ref.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        let toml_str =
            toml::to_string_pretty(self).context("序列化設定檔為 TOML 格式失敗")?;
        fs::write(path_ref, toml_str)
            .with_context(|| format!("無法寫入設定檔: {}", path_ref.display()))?;
        Ok(())
    }
}
