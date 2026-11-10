use crate::config::AppConfig;
use crate::recorder::{spawn_recorder_worker, RecorderCommand, RecorderEvent};
use crate::storage_mgr::{
    spawn_storage_manager, CronSchedule, StorageCommand, StorageStats,
};
use crate::viewer::{spawn_viewer_worker, PreviewFrame, ViewerCommand, ViewerEvent};
use chrono::Local;
use flume::{Receiver, Sender};
use iced::widget::{
    button, column, container, horizontal_space, image as iced_image, mouse_area, progress_bar,
    row, text, text_input,
};
use iced::{
    keyboard, window, Alignment, Color, ContentFit, Element, Length, Subscription, Task, Theme,
};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{error, info};

pub struct NvrApp {
    config: AppConfig,
    config_file_path: PathBuf,
    current_time_str: String,
    app_icon_handle: iced_image::Handle,

    // 預覽管線狀態與畫面
    latest_frame: Option<PreviewFrame>,
    frame_handle: Option<iced_image::Handle>,
    viewer_connected: bool,
    preview_paused: bool,
    viewer_fps: f32,
    viewer_decoder_name: String,
    viewer_status_text: String,

    // 錄影管線狀態
    is_recording: bool,
    recording_enabled: bool,
    current_record_file: Option<String>,
    recorder_status_text: String,

    // 磁碟空間狀態
    storage_stats: StorageStats,

    // 系統與連線設定面板狀態與編輯欄位
    show_settings: bool,
    show_password: bool,
    edit_name: String,
    edit_ip: String,
    edit_port: String,
    edit_username: String,
    edit_password: String,
    edit_main_path: String,
    edit_sub_path: String,
    edit_use_hw_accel: bool,
    edit_preview_max_fps: String,
    edit_segment_mins: String,
    edit_retention_days: String,
    edit_cleanup_cron: String,
    edit_max_storage_gb: String,

    // 底部操作回饋訊息 (如截圖或儲存設定通知)
    toast_message: Option<String>,

    // 全螢幕狀態與滑鼠左鍵連點兩下偵測
    is_fullscreen: bool,
    last_video_click: Option<Instant>,

    // 跨執行緒通訊 Channels
    viewer_rx: Receiver<ViewerEvent>,
    viewer_cmd_tx: Sender<ViewerCommand>,
    recorder_rx: Receiver<RecorderEvent>,
    recorder_cmd_tx: Sender<RecorderCommand>,
    storage_rx: Receiver<StorageStats>,
    storage_cmd_tx: Sender<StorageCommand>,
}

impl Drop for NvrApp {
    fn drop(&mut self) {
        info!("視窗關閉中，正在通知 3K 錄影管線安全封裝 MP4 檔尾 (moov atom)...");
        let _ = self.recorder_cmd_tx.send(RecorderCommand::Shutdown);
        let _ = self.viewer_cmd_tx.send(ViewerCommand::Shutdown);
        let _ = self.storage_cmd_tx.send(StorageCommand::Shutdown);
        std::thread::sleep(Duration::from_millis(600));
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    // 事件驅動訊息 (僅在收到新畫面、狀態變更或每秒時鐘跳動時觸發更新，避免 30Hz 空轉重繪)
    ClockTick,
    ViewerEvent(ViewerEvent),
    RecorderEvent(RecorderEvent),
    StorageEvent(StorageStats),
    // 操作控制事件
    ToggleRecording,
    TogglePreview,
    TakeScreenshot,
    OpenRecordFolder,
    TriggerStorageCleanup,
    // 全螢幕切換與畫面滑鼠點擊事件
    VideoAreaClicked,
    ToggleFullscreen,
    ExitFullscreen,
    // 系統與連線設定相關事件
    ToggleSettingsPanel,
    ToggleShowPassword,
    ToggleEditHwAccel,
    EditNameChanged(String),
    EditIpChanged(String),
    EditPortChanged(String),
    EditUsernameChanged(String),
    EditPasswordChanged(String),
    EditMainPathChanged(String),
    EditSubPathChanged(String),
    EditPreviewMaxFpsChanged(String),
    EditSegmentMinsChanged(String),
    EditRetentionDaysChanged(String),
    EditCleanupCronChanged(String),
    EditMaxStorageGbChanged(String),
    SaveAndApplySettings,
}

impl NvrApp {
    pub fn new(config: AppConfig, config_file_path: PathBuf) -> (Self, Task<Message>) {
        // 建立預覽管線 Channel (容量設為 2，避免堆積舊畫面與浪費記憶體複製)
        let (viewer_tx, viewer_rx) = flume::bounded(2);
        let (viewer_cmd_tx, viewer_cmd_rx) = flume::unbounded();
        spawn_viewer_worker(config.camera.clone(), viewer_tx, viewer_cmd_rx);

        // 建立 3K 錄影管線 Channel
        let (recorder_tx, recorder_rx) = flume::unbounded();
        let (recorder_cmd_tx, recorder_cmd_rx) = flume::unbounded();
        spawn_recorder_worker(
            config.camera.clone(),
            config.storage.clone(),
            recorder_tx,
            recorder_cmd_rx,
        );

        // 建立循環錄影空間管理員 Channel
        let (storage_tx, storage_rx) = flume::unbounded();
        let (storage_cmd_tx, storage_cmd_rx) = flume::unbounded();
        spawn_storage_manager(config.storage.clone(), storage_tx, storage_cmd_rx);

        let max_bytes = (config.storage.max_storage_gb * 1024.0 * 1024.0 * 1024.0) as u64;
        let auto_rec = config.storage.auto_start_recording;
        let preview_paused = !config.camera.preview_enabled;

        let edit_name = config.camera.name.clone();
        let edit_ip = config.camera.ip.clone();
        let edit_port = config.camera.port.to_string();
        let edit_username = config.camera.username.clone();
        let edit_password = config.camera.password.clone();
        let edit_main_path = config.camera.main_stream_path.clone();
        let edit_sub_path = config.camera.sub_stream_path.clone();
        let edit_use_hw_accel = config.camera.use_hw_accel;
        let edit_preview_max_fps = config.camera.preview_max_fps.to_string();
        let edit_segment_mins = config.storage.segment_duration_mins.to_string();
        let edit_retention_days = config.storage.retention_days.to_string();
        let edit_cleanup_cron = config.storage.cleanup_cron.clone();
        let edit_max_storage_gb = format!("{}", config.storage.max_storage_gb);

        let app = Self {
            config,
            config_file_path,
            current_time_str: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            app_icon_handle: iced_image::Handle::from_rgba(
                36,
                36,
                crate::icon::render_nvr_icon_rgba(36),
            ),
            latest_frame: None,
            frame_handle: None,
            viewer_connected: false,
            preview_paused,
            viewer_fps: 0.0,
            viewer_decoder_name: String::new(),
            viewer_status_text: if preview_paused {
                "已暫停預覽 (省電純 3K 錄影模式)".to_string()
            } else {
                "初始化中...".to_string()
            },
            is_recording: false,
            recording_enabled: auto_rec,
            current_record_file: None,
            recorder_status_text: if auto_rec {
                "準備啟動 3K 錄影...".to_string()
            } else {
                "已暫停錄影 (待命)".to_string()
            },
            storage_stats: StorageStats {
                used_bytes: 0,
                max_bytes,
                file_count: 0,
                deleted_in_last_run: 0,
                last_check_time: "--:--:--".to_string(),
                next_cron_time: "計算中...".to_string(),
            },
            show_settings: false,
            show_password: false,
            edit_name,
            edit_ip,
            edit_port,
            edit_username,
            edit_password,
            edit_main_path,
            edit_sub_path,
            edit_use_hw_accel,
            edit_preview_max_fps,
            edit_segment_mins,
            edit_retention_days,
            edit_cleanup_cron,
            edit_max_storage_gb,
            toast_message: None,
            is_fullscreen: false,
            last_video_click: None,
            viewer_rx,
            viewer_cmd_tx,
            recorder_rx,
            recorder_cmd_tx,
            storage_rx,
            storage_cmd_tx,
        };

        (app, Task::none())
    }

    pub fn title(&self) -> String {
        format!(
            "Ubuntu Rust NVR v{} - Foscam C5M ({})",
            env!("CARGO_PKG_VERSION"),
            self.config.camera.ip
        )
    }

    pub fn theme(&self) -> Theme {
        Theme::Dark
    }

    pub fn subscription(&self) -> Subscription<Message> {
        // 採用事件驅動 Subscription：僅在背景執行緒送來新畫面/狀態或每秒時鐘跳動 (1Hz) 時才喚醒 GUI，徹底消除無效的 30Hz 空轉重繪
        let clock_sub = iced::time::every(Duration::from_secs(1)).map(|_| Message::ClockTick);
        let viewer_sub =
            Subscription::run_with_id("viewer-events", self.viewer_rx.clone().into_stream())
                .map(Message::ViewerEvent);
        let recorder_sub =
            Subscription::run_with_id("recorder-events", self.recorder_rx.clone().into_stream())
                .map(Message::RecorderEvent);
        let storage_sub =
            Subscription::run_with_id("storage-events", self.storage_rx.clone().into_stream())
                .map(Message::StorageEvent);

        let key_sub = keyboard::on_key_press(|key, _modifiers| match key {
            keyboard::Key::Named(keyboard::key::Named::F11) => Some(Message::ToggleFullscreen),
            keyboard::Key::Named(keyboard::key::Named::Escape) => Some(Message::ExitFullscreen),
            _ => None,
        });

        Subscription::batch([clock_sub, viewer_sub, recorder_sub, storage_sub, key_sub])
    }

    fn apply_viewer_event(&mut self, ev: ViewerEvent) {
        match ev {
            ViewerEvent::Frame(frame) => {
                if !self.preview_paused {
                    // O(1) Zero-Copy 引用計數複製，不複製 3.68MB 像素資料
                    self.frame_handle = Some(frame.handle.clone());
                    self.latest_frame = Some(frame);
                }
            }
            ViewerEvent::Status {
                connected,
                paused,
                fps,
                decoder_name,
                message,
            } => {
                self.viewer_connected = connected;
                self.preview_paused = paused;
                self.viewer_fps = fps;
                self.viewer_decoder_name = decoder_name;
                self.viewer_status_text = message;
            }
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ClockTick => {
                self.current_time_str = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            }

            Message::ViewerEvent(first_ev) => {
                self.apply_viewer_event(first_ev);
                // 若佇列中同時有多筆事件，一次汲取至最新一幀，避免連續重繪
                while let Ok(ev) = self.viewer_rx.try_recv() {
                    self.apply_viewer_event(ev);
                }
            }

            Message::RecorderEvent(ev) => {
                let RecorderEvent::Status {
                    is_recording,
                    is_enabled,
                    current_file,
                    message,
                } = ev;
                self.is_recording = is_recording;
                self.recording_enabled = is_enabled;
                self.current_record_file = current_file;
                self.recorder_status_text = message;
            }

            Message::StorageEvent(stats) => {
                self.storage_stats = stats;
            }

            Message::ToggleRecording => {
                if self.recording_enabled {
                    let _ = self.recorder_cmd_tx.send(RecorderCommand::Stop);
                } else {
                    let _ = self.recorder_cmd_tx.send(RecorderCommand::Start);
                }
                let _ = self.storage_cmd_tx.send(StorageCommand::RefreshNow);
            }

            Message::TogglePreview => {
                if self.preview_paused {
                    self.preview_paused = false;
                    self.config.camera.preview_enabled = true;
                    let _ = self.viewer_cmd_tx.send(ViewerCommand::Resume);
                    self.toast_message = Some("已恢復 720p 即時預覽畫面".to_string());
                } else {
                    self.preview_paused = true;
                    self.viewer_connected = false;
                    self.viewer_fps = 0.0;
                    self.frame_handle = None;
                    self.config.camera.preview_enabled = false;
                    let _ = self.viewer_cmd_tx.send(ViewerCommand::Pause);
                    self.toast_message = Some(
                        "已進入省電模式：暫停 720p 預覽解碼，背景 3K 錄影持續無損運作中"
                            .to_string(),
                    );
                }
                let _ = self.config.save_to_file(&self.config_file_path);
            }

            Message::TakeScreenshot => {
                if let Some(ref frame) = self.latest_frame {
                    let Some(rgba_bytes) = frame.rgba_bytes() else {
                        self.toast_message = Some("無法讀取畫面像素資料".to_string());
                        return Task::none();
                    };

                    let screenshots_dir = self.config.storage.record_dir.join("screenshots");
                    if let Err(e) = std::fs::create_dir_all(&screenshots_dir) {
                        self.toast_message = Some(format!("建立截圖資料夾失敗: {}", e));
                        return Task::none();
                    }

                    let filename = format!(
                        "snapshot_{}.png",
                        Local::now().format("%Y%m%d_%H%M%S")
                    );
                    let filepath = screenshots_dir.join(&filename);

                    match save_rgba_as_png(&filepath, frame.width, frame.height, rgba_bytes) {
                        Ok(_) => {
                            info!("畫面截圖已儲存: {}", filepath.display());
                            self.toast_message =
                                Some(format!("已儲存截圖: screenshots/{}", filename));
                        }
                        Err(e) => {
                            error!("儲存截圖失敗: {}", e);
                            self.toast_message = Some(format!("截圖失敗: {}", e));
                        }
                    }
                } else {
                    self.toast_message = Some("目前尚無預覽畫面可供截圖".to_string());
                }
            }

            Message::OpenRecordFolder => {
                let dir = &self.config.storage.record_dir;
                let _ = std::fs::create_dir_all(dir);
                let open_cmd = if cfg!(target_os = "windows") {
                    "explorer"
                } else if cfg!(target_os = "macos") {
                    "open"
                } else {
                    "xdg-open"
                };
                match Command::new(open_cmd).arg(dir).spawn() {
                    Ok(_) => {
                        self.toast_message = Some(format!("已開啟資料夾: {}", dir.display()));
                    }
                    Err(e) => {
                        error!("無法開啟錄影資料夾: {}", e);
                        self.toast_message =
                            Some(format!("開啟資料夾失敗 ({}): {}", dir.display(), e));
                    }
                }
            }

            Message::TriggerStorageCleanup => {
                let _ = self.storage_cmd_tx.send(StorageCommand::RunCleanupNow);
                self.toast_message =
                    Some("已手動觸發硬碟空間巡檢與過期/超量檔案清理".to_string());
            }

            Message::VideoAreaClicked => {
                let now = Instant::now();
                if let Some(prev) = self.last_video_click {
                    if now.duration_since(prev) <= Duration::from_millis(400) {
                        self.last_video_click = None;
                        return self.update(Message::ToggleFullscreen);
                    }
                }
                self.last_video_click = Some(now);
            }

            Message::ToggleFullscreen => {
                self.is_fullscreen = !self.is_fullscreen;
                self.last_video_click = None;
                let target_mode = if self.is_fullscreen {
                    self.toast_message = Some(
                        "已進入全螢幕模式（滑鼠左鍵連點兩下畫面或按 Esc / F11 即可返回）"
                            .to_string(),
                    );
                    window::Mode::Fullscreen
                } else {
                    self.toast_message = Some("已退出全螢幕模式".to_string());
                    window::Mode::Windowed
                };
                return window::get_latest()
                    .and_then(move |id| window::change_mode(id, target_mode));
            }

            Message::ExitFullscreen => {
                if self.is_fullscreen {
                    self.is_fullscreen = false;
                    self.last_video_click = None;
                    self.toast_message = Some("已退出全螢幕模式".to_string());
                    return window::get_latest()
                        .and_then(|id| window::change_mode(id, window::Mode::Windowed));
                }
            }

            Message::ToggleSettingsPanel => {
                self.show_settings = !self.show_settings;
                if self.show_settings {
                    // 開啟設定面板時，同步載入當前生效的攝影機與錄影排程設定
                    self.edit_name = self.config.camera.name.clone();
                    self.edit_ip = self.config.camera.ip.clone();
                    self.edit_port = self.config.camera.port.to_string();
                    self.edit_username = self.config.camera.username.clone();
                    self.edit_password = self.config.camera.password.clone();
                    self.edit_main_path = self.config.camera.main_stream_path.clone();
                    self.edit_sub_path = self.config.camera.sub_stream_path.clone();
                    self.edit_use_hw_accel = self.config.camera.use_hw_accel;
                    self.edit_preview_max_fps = self.config.camera.preview_max_fps.to_string();
                    self.edit_segment_mins = self.config.storage.segment_duration_mins.to_string();
                    self.edit_retention_days = self.config.storage.retention_days.to_string();
                    self.edit_cleanup_cron = self.config.storage.cleanup_cron.clone();
                    self.edit_max_storage_gb = format!("{}", self.config.storage.max_storage_gb);
                }
            }

            Message::ToggleShowPassword => {
                self.show_password = !self.show_password;
            }

            Message::ToggleEditHwAccel => {
                self.edit_use_hw_accel = !self.edit_use_hw_accel;
            }

            Message::EditNameChanged(val) => self.edit_name = val,
            Message::EditIpChanged(val) => self.edit_ip = val,
            Message::EditPortChanged(val) => self.edit_port = val,
            Message::EditUsernameChanged(val) => self.edit_username = val,
            Message::EditPasswordChanged(val) => self.edit_password = val,
            Message::EditMainPathChanged(val) => self.edit_main_path = val,
            Message::EditSubPathChanged(val) => self.edit_sub_path = val,
            Message::EditPreviewMaxFpsChanged(val) => self.edit_preview_max_fps = val,
            Message::EditSegmentMinsChanged(val) => self.edit_segment_mins = val,
            Message::EditRetentionDaysChanged(val) => self.edit_retention_days = val,
            Message::EditCleanupCronChanged(val) => self.edit_cleanup_cron = val,
            Message::EditMaxStorageGbChanged(val) => self.edit_max_storage_gb = val,

            Message::SaveAndApplySettings => {
                let trimmed_ip = self.edit_ip.trim().to_string();
                if trimmed_ip.is_empty() {
                    self.toast_message = Some("錯誤：IP 位址或網址不可為空！".to_string());
                    return Task::none();
                }

                let parsed_port = match self.edit_port.trim().parse::<u16>() {
                    Ok(p) if p > 0 => p,
                    _ => {
                        self.toast_message =
                            Some("錯誤：RTSP 連接埠必須為 1 ~ 65535 之間的數字！".to_string());
                        return Task::none();
                    }
                };

                let parsed_preview_fps = match self.edit_preview_max_fps.trim().parse::<u32>() {
                    Ok(fps) if (1..=60).contains(&fps) => fps,
                    _ => {
                        self.toast_message =
                            Some("錯誤：預覽最大 FPS 請輸入 1 ~ 60 之間的整數！".to_string());
                        return Task::none();
                    }
                };

                let parsed_seg_mins = match self.edit_segment_mins.trim().parse::<u64>() {
                    Ok(m) if (1..=1440).contains(&m) => m,
                    _ => {
                        self.toast_message =
                            Some("錯誤：單一檔案錄影時間請輸入 1 ~ 1440 分鐘之間的整數！".to_string());
                        return Task::none();
                    }
                };

                let parsed_ret_days = match self.edit_retention_days.trim().parse::<u32>() {
                    Ok(d) if (1..=3650).contains(&d) => d,
                    _ => {
                        self.toast_message =
                            Some("錯誤：錄影保留天數請輸入 1 ~ 3650 天之間的整數！".to_string());
                        return Task::none();
                    }
                };

                let trimmed_cron = self.edit_cleanup_cron.trim().to_string();
                if let Err(err) = CronSchedule::parse(&trimmed_cron) {
                    self.toast_message = Some(format!("錯誤：{}", err));
                    return Task::none();
                }

                let parsed_max_gb = match self.edit_max_storage_gb.trim().parse::<f64>() {
                    Ok(gb) if gb >= 1.0 => gb,
                    _ => {
                        self.toast_message =
                            Some("錯誤：硬碟容量上限請輸入大於等於 1.0 (GB) 的數值！".to_string());
                        return Task::none();
                    }
                };

                let new_main_path = if self.edit_main_path.trim().is_empty() {
                    "/videoMain".to_string()
                } else {
                    self.edit_main_path.trim().to_string()
                };
                let new_sub_path = if self.edit_sub_path.trim().is_empty() {
                    "/videoSub".to_string()
                } else {
                    self.edit_sub_path.trim().to_string()
                };

                // 判斷攝影機連線、預覽設定或錄影分段時間是否有變更
                let camera_conn_changed = self.config.camera.ip != trimmed_ip
                    || self.config.camera.port != parsed_port
                    || self.config.camera.username != self.edit_username.trim()
                    || self.config.camera.password != self.edit_password
                    || self.config.camera.main_stream_path != new_main_path
                    || self.config.camera.sub_stream_path != new_sub_path
                    || self.config.camera.mock_mode;

                let viewer_changed = camera_conn_changed
                    || self.config.camera.use_hw_accel != self.edit_use_hw_accel
                    || self.config.camera.preview_max_fps != parsed_preview_fps;

                let recorder_changed = camera_conn_changed
                    || self.config.storage.segment_duration_mins != parsed_seg_mins;

                self.config.camera.name = if self.edit_name.trim().is_empty() {
                    "Foscam C5M".to_string()
                } else {
                    self.edit_name.trim().to_string()
                };
                self.config.camera.ip = trimmed_ip;
                self.config.camera.port = parsed_port;
                self.config.camera.username = self.edit_username.trim().to_string();
                self.config.camera.password = self.edit_password.clone();
                self.config.camera.main_stream_path = new_main_path;
                self.config.camera.sub_stream_path = new_sub_path;
                self.config.camera.use_hw_accel = self.edit_use_hw_accel;
                self.config.camera.preview_max_fps = parsed_preview_fps;
                self.config.camera.mock_mode = false;

                self.config.storage.segment_duration_mins = parsed_seg_mins;
                self.config.storage.retention_days = parsed_ret_days;
                self.config.storage.cleanup_cron = trimmed_cron;
                self.config.storage.max_storage_gb = parsed_max_gb;

                // 1. 寫入 config.toml 持久化保存
                let save_status = match self.config.save_to_file(&self.config_file_path) {
                    Ok(_) => format!("已儲存至 {}", self.config_file_path.display()),
                    Err(e) => {
                        error!("寫入設定檔失敗: {}", e);
                        format!("設定已套用 (但寫入檔案失敗: {})", e)
                    }
                };

                // 2. 通知空間管理員立即更新 Cron 排程與保留天數
                let _ = self
                    .storage_cmd_tx
                    .send(StorageCommand::UpdateConfig(self.config.storage.clone()));

                // 3. 若預覽或錄影設定有變更，通知對應管線切換
                if viewer_changed {
                    let _ = self
                        .viewer_cmd_tx
                        .send(ViewerCommand::UpdateConfig(self.config.camera.clone()));
                }
                if recorder_changed {
                    let _ = self.recorder_cmd_tx.send(RecorderCommand::UpdateConfig(
                        self.config.camera.clone(),
                        self.config.storage.clone(),
                    ));
                }

                self.show_settings = false;
                self.toast_message = Some(format!(
                    "已套用設定：預覽上限 {} FPS、硬體加速={}、單檔 {} 分 ({})",
                    self.config.camera.preview_max_fps,
                    if self.config.camera.use_hw_accel { "開" } else { "關" },
                    self.config.storage.segment_duration_mins,
                    save_status
                ));
            }
        }

        Task::none()
    }

    pub fn view(&self) -> Element<'_, Message> {
        // =========================================================
        // 1. 頂部標題與狀態列：[● REC 3K] 大門監控 (Foscam C5M)    2026-11-08 15:57
        // =========================================================
        let (rec_badge_text, rec_badge_color) = if self.is_recording {
            ("● REC 3K", Color::from_rgb(0.95, 0.22, 0.22))
        } else if self.recording_enabled {
            ("● 連線錄影中", Color::from_rgb(0.95, 0.70, 0.15))
        } else {
            ("○ 錄影待命", Color::from_rgb(0.55, 0.58, 0.62))
        };

        let header_bar = container(
            row![
                iced_image(self.app_icon_handle.clone())
                    .width(Length::Fixed(28.0))
                    .height(Length::Fixed(28.0)),
                container(cjk_text(rec_badge_text).size(15).color(rec_badge_color)).padding([4, 10]),
                cjk_text(&self.config.camera.name)
                    .size(18)
                    .color(Color::WHITE),
                cjk_text(format!(
                    "({}:{})",
                    self.config.camera.ip, self.config.camera.port
                ))
                .size(14)
                .color(Color::from_rgb(0.60, 0.68, 0.78)),
                cjk_text(format!(
                    "v{} | 作者: Lin Ming-Chi <j205025@gmail.com>",
                    env!("CARGO_PKG_VERSION")
                ))
                .size(13)
                .color(Color::from_rgb(0.55, 0.62, 0.70)),
                horizontal_space(),
                cjk_text(&self.current_time_str)
                    .size(16)
                    .color(Color::from_rgb(0.80, 0.84, 0.88)),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([10, 16])
        .width(Length::Fill);

        // =========================================================
        // 2. 中央 1280 x 720 即時預覽畫面區 (使用 FilterMethod::Nearest 降低縮放負載)
        // =========================================================
        let video_inner: Element<'_, Message> = if let Some(ref handle) = self.frame_handle {
            container(
                iced_image(handle.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(ContentFit::Contain)
                    .filter_method(iced_image::FilterMethod::Nearest),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
        } else if self.preview_paused {
            container(
                column![
                    cjk_text("省電純錄影模式 — 已暫停 720p 即時預覽畫面")
                        .size(22)
                        .color(Color::from_rgb(0.45, 0.85, 0.55)),
                    cjk_text(&self.recorder_status_text)
                        .size(16)
                        .color(Color::from_rgb(0.75, 0.80, 0.85)),
                    cjk_text("提示：背景 3K 無損分段錄影仍持續運作中（CPU 極低耗能狀態）；點擊下方「恢復預覽畫面」即可隨時監看")
                        .size(14)
                        .color(Color::from_rgb(0.55, 0.70, 0.85)),
                ]
                .spacing(10)
                .align_x(Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
        } else {
            container(
                column![
                    cjk_text("1280 x 720 即時預覽畫面區")
                        .size(22)
                        .color(Color::from_rgb(0.65, 0.70, 0.75)),
                    cjk_text(&self.viewer_status_text)
                        .size(16)
                        .color(Color::from_rgb(0.50, 0.55, 0.60)),
                    cjk_text("提示：於畫面處「滑鼠左鍵連點兩下」（或按 F11）可切換全螢幕；不需看畫面時可點「暫停預覽(省電)」極大化降低 CPU 負載")
                        .size(14)
                        .color(Color::from_rgb(0.45, 0.65, 0.85)),
                ]
                .spacing(10)
                .align_x(Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
        };

        let video_viewport: Element<'_, Message> =
            mouse_area(video_inner).on_press(Message::VideoAreaClicked).into();

        // 若處於全螢幕模式，隱藏頂部與底部工具列，讓監控畫面完整填滿螢幕
        if self.is_fullscreen {
            return container(video_viewport)
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }

        // =========================================================
        // 3. 選用展開：攝影機連線、硬體加速、預覽幀率、錄影分段時間與 Cron 清理排程設定面板
        // =========================================================
        let settings_section: Option<Element<'_, Message>> = if self.show_settings {
            let row1 = row![
                cjk_text("攝影機名稱:").size(14).width(Length::Fixed(95.0)),
                text_input("例如: 大門監控", &self.edit_name)
                    .on_input(Message::EditNameChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(190.0)),
                cjk_text("IP 位址 / 網址:")
                    .size(14)
                    .width(Length::Fixed(105.0)),
                text_input("例如: 192.168.1.101", &self.edit_ip)
                    .on_input(Message::EditIpChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(200.0)),
                cjk_text("RTSP 埠號:")
                    .size(14)
                    .width(Length::Fixed(80.0)),
                text_input("88 或 554", &self.edit_port)
                    .on_input(Message::EditPortChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(80.0)),
            ]
            .spacing(10)
            .align_y(Alignment::Center);

            let pwd_toggle_label = if self.show_password {
                "隱藏密碼"
            } else {
                "顯示密碼"
            };

            let row2 = row![
                cjk_text("登入帳號:").size(14).width(Length::Fixed(95.0)),
                text_input("帳號", &self.edit_username)
                    .on_input(Message::EditUsernameChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(190.0)),
                cjk_text("登入密碼:")
                    .size(14)
                    .width(Length::Fixed(105.0)),
                text_input("密碼", &self.edit_password)
                    .on_input(Message::EditPasswordChanged)
                    .secure(!self.show_password)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(200.0)),
                button(cjk_text(pwd_toggle_label).size(13))
                    .on_press(Message::ToggleShowPassword)
                    .padding([6, 12]),
            ]
            .spacing(10)
            .align_y(Alignment::Center);

            let hw_accel_label = if self.edit_use_hw_accel {
                "硬體解碼加速: 開啟 (VA-API 優先)"
            } else {
                "硬體解碼加速: 關閉 (純 CPU)"
            };

            let row3 = row![
                cjk_text("3K 主碼流:").size(14).width(Length::Fixed(95.0)),
                text_input("/videoMain 或完整 rtsp://", &self.edit_main_path)
                    .on_input(Message::EditMainPathChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(190.0)),
                cjk_text("720p 副碼流:")
                    .size(14)
                    .width(Length::Fixed(105.0)),
                text_input("/videoSub 或完整 rtsp://", &self.edit_sub_path)
                    .on_input(Message::EditSubPathChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(200.0)),
                cjk_text("預覽最大FPS:")
                    .size(14)
                    .width(Length::Fixed(90.0)),
                text_input("預設 12", &self.edit_preview_max_fps)
                    .on_input(Message::EditPreviewMaxFpsChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(65.0)),
                button(cjk_text(hw_accel_label).size(13))
                    .on_press(Message::ToggleEditHwAccel)
                    .padding([6, 12]),
            ]
            .spacing(10)
            .align_y(Alignment::Center);

            let row4 = row![
                cjk_text("單檔時間(分):")
                    .size(14)
                    .width(Length::Fixed(95.0)),
                text_input("預設 30", &self.edit_segment_mins)
                    .on_input(Message::EditSegmentMinsChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(70.0)),
                cjk_text("保留天數(天):")
                    .size(14)
                    .width(Length::Fixed(95.0)),
                text_input("預設 30", &self.edit_retention_days)
                    .on_input(Message::EditRetentionDaysChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(70.0)),
                cjk_text("巡檢 Cron:")
                    .size(14)
                    .width(Length::Fixed(75.0)),
                text_input("分 時 日 月 週 (如 0 3 * * 2)", &self.edit_cleanup_cron)
                    .on_input(Message::EditCleanupCronChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(140.0)),
                cjk_text("容量上限(GB):")
                    .size(14)
                    .width(Length::Fixed(95.0)),
                text_input("100", &self.edit_max_storage_gb)
                    .on_input(Message::EditMaxStorageGbChanged)
                    .padding(6)
                    .size(14)
                    .width(Length::Fixed(75.0)),
                button(cjk_text("儲存並套用設定").size(14))
                    .on_press(Message::SaveAndApplySettings)
                    .padding([7, 16]),
                button(cjk_text("取消").size(14))
                    .on_press(Message::ToggleSettingsPanel)
                    .padding([7, 14]),
            ]
            .spacing(8)
            .align_y(Alignment::Center);

            Some(
                container(
                    column![
                        cjk_text("系統與連線設定 — 攝影機 IP、硬體解碼加速、預覽最大 FPS、單檔錄影分鐘數與 Cron 自動清理排程")
                            .size(14)
                            .color(Color::from_rgb(0.95, 0.82, 0.35)),
                        row1,
                        row2,
                        row3,
                        row4,
                    ]
                    .spacing(8),
                )
                .padding([12, 16])
                .width(Length::Fill)
                .into(),
            )
        } else {
            None
        };

        // =========================================================
        // 4. 底部狀態與控制面板
        // =========================================================
        let viewer_color = if self.viewer_connected {
            Color::from_rgb(0.30, 0.85, 0.45)
        } else if self.preview_paused {
            Color::from_rgb(0.45, 0.78, 0.95)
        } else {
            Color::from_rgb(0.95, 0.65, 0.25)
        };

        let recorder_color = if self.is_recording {
            Color::from_rgb(0.30, 0.85, 0.45)
        } else if self.recording_enabled {
            Color::from_rgb(0.95, 0.65, 0.25)
        } else {
            Color::from_rgb(0.65, 0.68, 0.72)
        };

        let status_row = row![
            cjk_text("預覽狀態:")
                .size(14)
                .color(Color::from_rgb(0.75, 0.78, 0.82)),
            cjk_text(&self.viewer_status_text)
                .size(14)
                .color(viewer_color),
            cjk_text("  |  ")
                .size(14)
                .color(Color::from_rgb(0.45, 0.48, 0.52)),
            cjk_text("錄影狀態:")
                .size(14)
                .color(Color::from_rgb(0.75, 0.78, 0.82)),
            cjk_text(&self.recorder_status_text)
                .size(14)
                .color(recorder_color),
            horizontal_space(),
            cjk_text(self.toast_message.as_deref().unwrap_or(""))
                .size(13)
                .color(Color::from_rgb(0.45, 0.80, 0.95)),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let used_gb = self.storage_stats.used_gb();
        let max_gb = self.storage_stats.max_gb();
        let usage_pct = (self.storage_stats.usage_ratio() * 100.0).clamp(0.0, 100.0);

        let storage_row = row![
            cjk_text("磁碟空間:")
                .size(14)
                .color(Color::from_rgb(0.75, 0.78, 0.82)),
            container(progress_bar(0.0..=100.0, usage_pct).height(14))
                .width(Length::Fixed(220.0)),
            cjk_text(format!(
                "{:.2} / {:.1} GB ({:.1}%) | 共 {} 個切片 | 單檔: {}分 | 保留: {}天 | 下次 Cron 巡檢: {} (上次清理: {} 個)",
                used_gb,
                max_gb,
                usage_pct,
                self.storage_stats.file_count,
                self.config.storage.segment_duration_mins,
                self.config.storage.retention_days,
                self.storage_stats.next_cron_time,
                self.storage_stats.deleted_in_last_run
            ))
            .size(13)
            .color(Color::from_rgb(0.85, 0.88, 0.92)),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let toggle_rec_label = if self.recording_enabled {
            "暫停 3K 錄影"
        } else {
            "開始 3K 錄影"
        };

        let toggle_preview_label = if self.preview_paused {
            "恢復預覽畫面"
        } else {
            "暫停預覽(省電)"
        };

        let settings_btn_label = if self.show_settings {
            "收起系統設定"
        } else {
            "系統與連線設定"
        };

        let controls_row = row![
            button(cjk_text(toggle_rec_label).size(14))
                .on_press(Message::ToggleRecording)
                .padding([8, 16]),
            button(cjk_text(toggle_preview_label).size(14))
                .on_press(Message::TogglePreview)
                .padding([8, 16]),
            button(cjk_text("畫面截圖").size(14))
                .on_press(Message::TakeScreenshot)
                .padding([8, 16]),
            button(cjk_text("全螢幕預覽").size(14))
                .on_press(Message::ToggleFullscreen)
                .padding([8, 16]),
            button(cjk_text("開啟錄影資料夾").size(14))
                .on_press(Message::OpenRecordFolder)
                .padding([8, 16]),
            button(cjk_text("立即巡檢清理").size(14))
                .on_press(Message::TriggerStorageCleanup)
                .padding([8, 16]),
            button(cjk_text(settings_btn_label).size(14))
                .on_press(Message::ToggleSettingsPanel)
                .padding([8, 16]),
            horizontal_space(),
            cjk_text(format!(
                "存檔路徑: {}",
                self.config.storage.record_dir.display()
            ))
            .size(12)
            .color(Color::from_rgb(0.55, 0.58, 0.62)),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let bottom_panel = container(
            column![status_row, storage_row, controls_row]
                .spacing(10)
                .width(Length::Fill),
        )
        .padding([12, 16])
        .width(Length::Fill);

        let mut main_col = column![header_bar, video_viewport];
        if let Some(panel) = settings_section {
            main_col = main_col.push(panel);
        }
        main_col = main_col.push(bottom_panel);

        main_col.width(Length::Fill).height(Length::Fill).into()
    }
}

/// 建立啟用 `Shaping::Advanced` 的文字元件，確保繁體中文字元與特殊符號能正確進行系統字型 Fallback 渲染，不會出現「⍰」豆腐塊
fn cjk_text<'a>(content: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(content).shaping(text::Shaping::Advanced)
}

fn save_rgba_as_png(
    path: &std::path::Path,
    width: u32,
    height: u32,
    rgba_data: &[u8],
) -> anyhow::Result<()> {
    let file = File::create(path)?;
    let w = BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba_data)?;
    Ok(())
}
