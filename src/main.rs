#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
    // 限制 Tokio 非同步執行緒池為 2 個工作執行緒 (預設會依 Lenovo M900 的 8 邏輯核心建立 8 條執行緒)，
    // 減少背景執行緒數量與 Context Switch 功耗
    if env::var_os("TOKIO_WORKER_THREADS").is_none() {
        env::set_var("TOKIO_WORKER_THREADS", "2");
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "info,iced_winit=warn,iced_wgpu=warn,wgpu_core=warn,wgpu_hal=warn,sctk_adwaita=error",
                )
            }),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    let mut config_path: Option<PathBuf> = None;
    let mut renderer_override: Option<String> = None;
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
            "--renderer" | "-r" => {
                if i + 1 < args.len() {
                    renderer_override = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            "--mock" | "--test-pattern" => {
                force_mock = true;
            }
            "--check-env" => {
                check_env_only = true;
            }
            "--version" | "-V" | "-v" => {
                println!("rnvr v{}", env!("CARGO_PKG_VERSION"));
                println!("作者 (Author): Lin Ming-Chi <j205025@gmail.com>");
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "Ubuntu Rust NVR 監控錄影系統 v{} (Foscam C5M 單機雙碼流分離架構)",
                    env!("CARGO_PKG_VERSION")
                );
                println!("作者: Lin Ming-Chi <j205025@gmail.com>");
                println!("用法: rnvr [選項]");
                println!("  -c, --config <PATH>       指定設定檔路徑 (預設自動搜尋 ./config.toml 或 ~/.config/rnvr/config.toml)");
                println!("  -r, --renderer <BACKEND>  指定圖形渲染後端: auto (預設), gl (硬體 OpenGL), vulkan, tiny-skia (2D 軟體)");
                println!("  --mock                    啟用測試訊號源模式 (無須連接實體攝影機即可驗證預覽與分段錄影)");
                println!("  --check-env               檢查系統 GStreamer 插件與硬體解碼支援並結束");
                println!("  -V, --version             顯示版本與作者資訊");
                println!("  -h, --help                顯示此說明");
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

    // 2. 智慧選擇最佳圖形渲染後端 (避免 wgpu 在舊款 Intel 內顯或無 Vulkan 主機上誤選 llvmpipe CPU 軟體模擬)
    configure_optimal_graphics_backend(renderer_override.as_deref());

    // 3. 載入設定檔並決定可寫入的儲存路徑 (支援 .deb 安裝後自動初始化 ~/.config/rnvr/config.toml)
    let (mut app_config, save_path) = load_or_init_config(config_path)?;

    if force_mock {
        info!("已透過命令列參數啟用模擬測試模式 (mock_mode = true)");
        app_config.camera.mock_mode = true;
    }

    // 4. 啟動 Iced 視窗 (設定專屬攝影機圖示、Wayland application_id 與繁體中文字型)
    #[cfg(target_os = "linux")]
    let platform_specific = window::settings::PlatformSpecific {
        application_id: "rnvr".to_string(),
        ..Default::default()
    };
    #[cfg(not(target_os = "linux"))]
    let platform_specific = window::settings::PlatformSpecific::default();

    #[cfg(target_os = "windows")]
    let default_cjk_font = Font::with_name("Microsoft JhengHei");
    #[cfg(not(target_os = "windows"))]
    let default_cjk_font = Font::with_name("Noto Sans CJK TC");

    let win_settings = window::Settings {
        size: Size::new(1280.0, 840.0),
        icon: icon::build_window_icon(),
        platform_specific,
        ..Default::default()
    };

    iced::application(NvrApp::title, NvrApp::update, NvrApp::view)
        .subscription(NvrApp::subscription)
        .theme(NvrApp::theme)
        .default_font(default_cjk_font)
        .window(win_settings)
        .run_with(move || NvrApp::new(app_config, save_path))
        .context("Iced GUI 視窗執行失敗")?;

    Ok(())
}

/// 智慧設定圖形渲染後端，徹底避開 Linux Mesa `llvmpipe` (CPU 軟體模擬 Vulkan) 造成的高 CPU 負載
fn configure_optimal_graphics_backend(explicit_renderer: Option<&str>) {
    if let Some(mode) = explicit_renderer {
        match mode.trim().to_ascii_lowercase().as_str() {
            "tiny-skia" | "skia" | "software" | "cpu" | "2d" => {
                env::set_var("ICED_BACKEND", "tiny-skia");
                info!("已啟用輕量 2D 渲染後端 (ICED_BACKEND=tiny-skia)");
                return;
            }
            "wgpu" | "vulkan" | "vk" => {
                env::set_var("ICED_BACKEND", "wgpu");
                env::set_var("WGPU_BACKEND", "vulkan");
                info!("已指定使用 Vulkan 渲染後端 (ICED_BACKEND=wgpu, WGPU_BACKEND=vulkan)");
                return;
            }
            "gl" | "opengl" | "gles" => {
                env::set_var("ICED_BACKEND", "wgpu");
                env::set_var("WGPU_BACKEND", "gl");
                info!("已指定使用 OpenGL 渲染後端 (ICED_BACKEND=wgpu, WGPU_BACKEND=gl)");
                return;
            }
            _ => {}
        }
    }

    // 若使用者已透過環境變數自行指定，尊重使用者設定
    if env::var_os("WGPU_BACKEND").is_some() || env::var_os("ICED_BACKEND").is_some() {
        return;
    }

    // 預設採用輕量 2D 渲染後端 (tiny-skia)，避免在舊款 Intel 內顯、迷你電腦、工控機或無獨立顯卡主機上
    // 觸發 Mesa llvmpipe (LLVM CPU 軟體模擬 Vulkan 3D 管線) 導致 CPU 飆升與記憶體暴增。
    env::set_var("ICED_BACKEND", "tiny-skia");
    info!("自動啟用低耗能 2D 圖形渲染後端 (ICED_BACKEND=tiny-skia)，避免觸發 llvmpipe CPU 3D 軟體模擬");
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

    // 若當前工作目錄存在 config.toml (例如開發目錄)，檢查是否仍為未修改的預設範本 (192.168.1.101 / your_password)；
    // 若使用者已在 ~/.config/rnvr/config.toml 設定真實攝影機連線，則自動同步並採用使用者設定，避免誤讀範本導致無法連線。
    let local = PathBuf::from("config.toml");
    let user_cfg_opt = env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .ok()
        .map(|h| PathBuf::from(h).join(".config/rnvr/config.toml"));

    if local.exists() {
        let local_cfg = AppConfig::load_from_file(&local)?;
        let is_unmodified_template =
            local_cfg.camera.ip == "192.168.1.101" && local_cfg.camera.password == "your_password";
        if is_unmodified_template {
            if let Some(ref user_cfg_path) = user_cfg_opt {
                if user_cfg_path.exists() {
                    if let Ok(user_cfg) = AppConfig::load_from_file(user_cfg_path) {
                        if user_cfg.camera.ip != "192.168.1.101"
                            || user_cfg.camera.password != "your_password"
                        {
                            info!(
                                "偵測到當前目錄 config.toml 為預設範本，自動同步並載入使用者設定檔: {}",
                                user_cfg_path.display()
                            );
                            let _ = user_cfg.save_to_file(&local);
                            return Ok((user_cfg, local));
                        }
                    }
                }
            }
        }
        info!("載入當前目錄設定檔: {}", local.display());
        return Ok((local_cfg, local));
    }

    // 檢查使用者家目錄 ~/.config/rnvr/config.toml (Linux HOME 或 Windows USERPROFILE)
    if let Ok(home) = env::var("HOME").or_else(|_| env::var("USERPROFILE")) {
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
    if let Ok(home) = env::var("HOME").or_else(|_| env::var("USERPROFILE")) {
        cfg.storage.record_dir = PathBuf::from(home).join("Foscam_Recordings");
    }
    cfg
}

/// 檢查雙碼流管線所需的 GStreamer 插件與硬體解碼支援是否就緒
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

    println!("\n=== 硬體 H.264 解碼加速支援檢查 ===");
    if let Some((hw_elem, hw_desc)) = viewer::detect_available_hw_decoder() {
        println!(
            "  [OK] {:<16} (已就緒！預覽管線將優先使用 {} 硬體解碼)",
            hw_elem, hw_desc
        );
    } else {
        println!("  [資訊] 未偵測到硬體解碼器 (建議安裝 i965-va-driver 或 intel-media-va-driver)，將使用 avdec_h264 (CPU)");
    }

    if missing.is_empty() {
        println!("\n=== 所有必要 GStreamer 插件皆已就緒！ ===");
        Ok(())
    } else {
        anyhow::bail!("缺少必要的 GStreamer 插件，請執行 install.sh 或安裝對應套件")
    }
}
