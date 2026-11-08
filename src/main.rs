mod app;
mod config;
mod recorder;
mod storage_mgr;
mod viewer;

use anyhow::{Context, Result};
use app::NvrApp;
use config::AppConfig;
use gstreamer as gst;
use iced::{Font, Size};
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

    // 2. 載入設定檔
    let resolved_path = resolve_config_path(config_path);
    let save_path = resolved_path
        .clone()
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let mut app_config = match resolved_path {
        Some(ref path) => {
            info!("載入設定檔: {}", path.display());
            AppConfig::load_from_file(path)?
        }
        None => {
            warn!("找不到 config.toml，使用內建預設參數");
            AppConfig::default()
        }
    };

    if force_mock {
        info!("已透過命令列參數啟用模擬測試模式 (mock_mode = true)");
        app_config.camera.mock_mode = true;
    }

    // 3. 啟動 Iced 視窗 (預設指定 Noto Sans CJK TC 中文字型，搭配 Shaping::Advanced 支援跨字型自動 Fallback)
    iced::application(NvrApp::title, NvrApp::update, NvrApp::view)
        .subscription(NvrApp::subscription)
        .theme(NvrApp::theme)
        .default_font(Font::with_name("Noto Sans CJK TC"))
        .window_size(Size::new(1280.0, 840.0))
        .run_with(move || NvrApp::new(app_config, save_path))
        .context("Iced GUI 視窗執行失敗")?;

    Ok(())
}

fn resolve_config_path(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p);
    }

    let local = PathBuf::from("config.toml");
    if local.exists() {
        return Some(local);
    }

    if let Ok(home) = env::var("HOME") {
        let user_cfg = PathBuf::from(home).join(".config/rnvr/config.toml");
        if user_cfg.exists() {
            return Some(user_cfg);
        }
    }

    let etc_cfg = PathBuf::from("/etc/rnvr/config.toml");
    if etc_cfg.exists() {
        return Some(etc_cfg);
    }

    None
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
