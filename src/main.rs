mod app;
mod config;
mod icon;
mod recorder;
mod storage_mgr;
mod viewer;

use anyhow::{Context, Result};
use app::NvrApp;
use config::AppConfig;
use gstreamer as gst;
use iced::{window, Font, Size};
use std::env;
use std::path::PathBuf;
use tracing::{info, warn};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    let mut config_path: Option<PathBuf> = None;
    let mut force_mock = false;
    let mut check_env_only = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => {
                if i + 1 < args.len() {
                    config_path = Some(PathBuf::from(&args[i + 1]));
                    i += 1;
                }
            }
            "--mock" | "--test-pattern" => {
                force_mock = true;
            }
            "--check-env" => {
                check_env_only = true;
            }
            "--help" | "-h" => {
                println!("Ubuntu Rust NVR 監控錄影系統 (Foscam C5M 單機雙碼流分離架構)");
                println!("用法: rnvr [選項]");
                println!("  -c, --config <PATH>   指定設定檔路徑 (預設自動搜尋 ./config.toml 或 ~/.config/rnvr/config.toml)");
                println!("  --mock                啟用測試訊號源模式 (無須連接實體攝影機即可驗證預覽與分段錄影)");
                println!("  --check-env           檢查系統 GStreamer 插件完整性並結束");
                println!("  -h, --help            顯示此說明");
                return Ok(());
            }
            _ => {}
        }
        i += 1;
    }

    // 1. 初始化 GStreamer
    gst::init().context("GStreamer 初始化失敗，請確認已安裝 GStreamer 1.0 系統套件")?;

    if check_env_only {
        return verify_gstreamer_plugins();
    }

    // 2. 載入設定檔並決定可寫入的儲存路徑 (支援 .deb 安裝後自動初始化 ~/.config/rnvr/config.toml)
    let (mut app_config, save_path) = load_or_init_config(config_path)?;

    if force_mock {
        info!("已透過命令列參數啟用模擬測試模式 (mock_mode = true)");
        app_config.camera.mock_mode = true;
    }

    // 3. 啟動 Iced 視窗 (設定專屬攝影機圖示、Wayland application_id 與 Noto Sans CJK TC 繁體中文字型)
    let win_settings = window::Settings {
        size: Size::new(1280.0, 840.0),
        icon: icon::build_window_icon(),
        platform_specific: window::settings::PlatformSpecific {
            application_id: "rnvr".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    iced::application(NvrApp::title, NvrApp::update, NvrApp::view)
        .subscription(NvrApp::subscription)
        .theme(NvrApp::theme)
        .default_font(Font::with_name("Noto Sans CJK TC"))
        .window(win_settings)
        .run_with(move || NvrApp::new(app_config, save_path))
        .context("Iced GUI 視窗執行失敗")?;

    Ok(())
}

/// 載入設定檔，並回傳 `(AppConfig, 可寫入的設定檔路徑)`
fn load_or_init_config(explicit: Option<PathBuf>) -> Result<(AppConfig, PathBuf)> {
    if let Some(p) = explicit {
        if p.exists() {
            info!("載入指定設定檔: {}", p.display());
            let cfg = AppConfig::load_from_file(&p)?;
            return Ok((cfg, p));
        } else {
            warn!("指定設定檔 {} 不存在，將使用預設值並建立檔案", p.display());
            let cfg = default_config_for_current_user();
            let _ = cfg.save_to_file(&p);
            return Ok((cfg, p));
        }
    }

    // 若當前工作目錄存在 config.toml (例如開發目錄)，優先使用
    let local = PathBuf::from("config.toml");
    if local.exists() {
        info!("載入當前目錄設定檔: {}", local.display());
        let cfg = AppConfig::load_from_file(&local)?;
        return Ok((cfg, local));
    }

    // 檢查使用者家目錄 ~/.config/rnvr/config.toml (適用於 .deb 或全系統安裝)
    if let Ok(home) = env::var("HOME") {
        let user_cfg_path = PathBuf::from(&home).join(".config/rnvr/config.toml");
        if user_cfg_path.exists() {
            info!("載入使用者設定檔: {}", user_cfg_path.display());
            let cfg = AppConfig::load_from_file(&user_cfg_path)?;
            return Ok((cfg, user_cfg_path));
        }

        // 若 ~/.config/rnvr/config.toml 尚不存在，嘗試由 /etc/rnvr/config.toml 作為範本初始化
        let etc_cfg_path = PathBuf::from("/etc/rnvr/config.toml");
        let mut cfg = if etc_cfg_path.exists() {
            info!(
                "初次啟動：以 {} 為範本建立使用者設定檔 {}",
                etc_cfg_path.display(),
                user_cfg_path.display()
            );
            AppConfig::load_from_file(&etc_cfg_path)
                .unwrap_or_else(|_| default_config_for_current_user())
        } else {
            default_config_for_current_user()
        };

        // 若範本中的錄影路徑為預設 /home/ubuntu/Foscam_Recordings，自動替換為當前使用者的家目錄
        if cfg.storage.record_dir == PathBuf::from("/home/ubuntu/Foscam_Recordings") {
            cfg.storage.record_dir = PathBuf::from(&home).join("Foscam_Recordings");
        }

        if let Err(e) = cfg.save_to_file(&user_cfg_path) {
            warn!("無法寫入初始設定檔 {}: {}", user_cfg_path.display(), e);
        }
        return Ok((cfg, user_cfg_path));
    }

    let cfg = AppConfig::default();
    Ok((cfg, local))
}

fn default_config_for_current_user() -> AppConfig {
    let mut cfg = AppConfig::default();
    if let Ok(home) = env::var("HOME") {
        cfg.storage.record_dir = PathBuf::from(home).join("Foscam_Recordings");
    }
    cfg
}

/// 檢查雙碼流管線所需的 GStreamer 插件是否皆已安裝
fn verify_gstreamer_plugins() -> Result<()> {
    let required_elements = [
        ("rtspsrc", "gstreamer1.0-plugins-good"),
        ("rtph264depay", "gstreamer1.0-plugins-good"),
        ("rtpmp4gdepay", "gstreamer1.0-plugins-good"),
        ("splitmuxsink", "gstreamer1.0-plugins-good"),
        ("h264parse", "gstreamer1.0-plugins-bad"),
        ("aacparse", "gstreamer1.0-plugins-good"),
        ("avdec_h264", "gstreamer1.0-libav"),
        ("videoconvert", "gstreamer1.0-plugins-base"),
        ("videoscale", "gstreamer1.0-plugins-base"),
        ("appsink", "gstreamer1.0-plugins-base"),
    ];

    println!("=== GStreamer 雙碼流管線環境檢查 ===");
    let mut missing = Vec::new();
    for (element, pkg) in required_elements {
        if gst::ElementFactory::find(element).is_some() {
            println!("  [OK] {:<16} (來自 {})", element, pkg);
        } else {
            println!("  [缺失] {:<14} (請安裝 {})", element, pkg);
            missing.push(pkg);
        }
    }

    if gst::ElementFactory::find("vaapih264dec").is_some() {
        println!("  [OK] vaapih264dec     (選用硬體解碼加速可用)");
    } else {
        println!("  [資訊] vaapih264dec   (未安裝，將使用純 CPU avdec_h264 解碼 720p)");
    }

    if missing.is_empty() {
        println!("=== 所有必要 GStreamer 插件皆已就緒！ ===");
        Ok(())
    } else {
        anyhow::bail!("缺少必要的 GStreamer 插件，請執行 install.sh 或安裝對應套件")
    }
}
