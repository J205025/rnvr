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
    #[serde(default)]
    pub use_hw_accel: bool,
    #[serde(default)]
    pub mock_mode: bool,
}

fn default_enable_audio() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub record_dir: PathBuf,
    pub segment_duration_secs: u64,
    pub max_storage_gb: f64,
    pub cleanup_interval_secs: u64,
    pub auto_start_recording: bool,
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
                use_hw_accel: false,
                mock_mode: false,
            },
            storage: StorageConfig {
                record_dir: PathBuf::from("/home/ubuntu/Foscam_Recordings"),
                segment_duration_secs: 600,
                max_storage_gb: 100.0,
                cleanup_interval_secs: 300,
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

        // 確保分段秒數與清理間隔有合理下限
        if cfg.storage.segment_duration_secs < 10 {
            cfg.storage.segment_duration_secs = 10;
        }
        if cfg.storage.cleanup_interval_secs < 10 {
            cfg.storage.cleanup_interval_secs = 10;
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
