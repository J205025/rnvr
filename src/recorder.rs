use crate::config::{CameraConfig, StorageConfig};
use crate::storage_mgr::generate_segment_filepath;
use anyhow::{Context, Result};
use flume::{Receiver, Sender};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub enum RecorderEvent {
    Status {
        is_recording: bool,
        is_enabled: bool,
        current_file: Option<String>,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub enum RecorderCommand {
    Start,
    Stop,
    UpdateConfig(CameraConfig),
    Shutdown,
}

/// 啟動 Pipeline 1：3K 主碼流無損分段錄影專線 (Zero-Decode Pass-through)
pub fn spawn_recorder_worker(
    camera_cfg: CameraConfig,
    storage_cfg: StorageConfig,
    event_tx: Sender<RecorderEvent>,
    cmd_rx: Receiver<RecorderCommand>,
) {
    thread::Builder::new()
        .name("gst-recorder-main".to_string())
        .spawn(move || {
            info!("啟動 3K 錄影管線執行緒 ({})", camera_cfg.name);
            run_recorder_loop(camera_cfg, storage_cfg, event_tx, cmd_rx);
        })
        .expect("無法建立 gst-recorder-main 執行緒");
}

fn run_recorder_loop(
    mut camera_cfg: CameraConfig,
    storage_cfg: StorageConfig,
    event_tx: Sender<RecorderEvent>,
    cmd_rx: Receiver<RecorderCommand>,
) {
    let mut recording_enabled = storage_cfg.auto_start_recording;

    if !recording_enabled {
        let _ = event_tx.send(RecorderEvent::Status {
            is_recording: false,
            is_enabled: false,
            current_file: None,
            message: "已暫停錄影 (待命)".to_string(),
        });
    }

    loop {
        // 若目前為暫停狀態，阻塞等待使用者按下「開始錄影」、更新設定或關閉程式
        while !recording_enabled {
            match cmd_rx.recv() {
                Ok(RecorderCommand::Start) => {
                    recording_enabled = true;
                    info!("使用者啟動 3K 錄影管線");
                }
                Ok(RecorderCommand::Stop) => continue,
                Ok(RecorderCommand::UpdateConfig(new_cfg)) => {
                    info!("錄影管線已更新攝影機連線設定: {}:{}", new_cfg.ip, new_cfg.port);
                    camera_cfg = new_cfg;
                }
                Ok(RecorderCommand::Shutdown) | Err(_) => {
                    info!("錄影管線結束執行緒");
                    return;
                }
            }
        }

        let _ = event_tx.send(RecorderEvent::Status {
            is_recording: false,
            is_enabled: true,
            current_file: None,
            message: if camera_cfg.mock_mode {
                "正在初始化 3K 模擬錄影管線...".to_string()
            } else {
                format!(
                    "連線主碼流中 ({}:{}{})...",
                    camera_cfg.ip, camera_cfg.port, camera_cfg.main_stream_path
                )
            },
        });

        let mut immediate_reconnect = false;
        match run_single_recording_session(&camera_cfg, &storage_cfg, &event_tx, &cmd_rx) {
            Ok(SessionOutcome::PausedByUser) => {
                recording_enabled = false;
                let _ = event_tx.send(RecorderEvent::Status {
                    is_recording: false,
                    is_enabled: false,
                    current_file: None,
                    message: "已暫停錄影 (MP4 檔尾已安全封裝)".to_string(),
                });
                continue;
            }
            Ok(SessionOutcome::ConfigUpdated(new_cfg)) => {
                info!("錄影管線立即切換至新攝影機設定: {}:{}", new_cfg.ip, new_cfg.port);
                camera_cfg = new_cfg;
                immediate_reconnect = true;
            }
            Ok(SessionOutcome::Shutdown) => {
                info!("錄影管線正常關閉");
                return;
            }
            Ok(SessionOutcome::Disconnected) => {
                warn!("3K 錄影管線中斷，準備自動重連...");
            }
            Err(err) => {
                error!("3K 錄影管線發生錯誤: {:#}", err);
            }
        }

        if immediate_reconnect {
            continue;
        }

        // 斷線後每 5 秒自動重試連線，期間仍可隨時回應暫停、更新設定或關閉指令
        for remaining in (1..=5).rev() {
            let _ = event_tx.send(RecorderEvent::Status {
                is_recording: false,
                is_enabled: true,
                current_file: None,
                message: format!("主碼流斷線，{} 秒後自動重連...", remaining),
            });

            match cmd_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(RecorderCommand::Stop) => {
                    recording_enabled = false;
                    let _ = event_tx.send(RecorderEvent::Status {
                        is_recording: false,
                        is_enabled: false,
                        current_file: None,
                        message: "已暫停錄影 (待命)".to_string(),
                    });
                    break;
                }
                Ok(RecorderCommand::Start) => {}
                Ok(RecorderCommand::UpdateConfig(new_cfg)) => {
                    info!("錄影管線於倒數期間收到新設定，立即重連: {}:{}", new_cfg.ip, new_cfg.port);
                    camera_cfg = new_cfg;
                    break;
                }
                Ok(RecorderCommand::Shutdown) => return,
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

enum SessionOutcome {
    PausedByUser,
    ConfigUpdated(CameraConfig),
    Disconnected,
    Shutdown,
}

fn run_single_recording_session(
    camera_cfg: &CameraConfig,
    storage_cfg: &StorageConfig,
    event_tx: &Sender<RecorderEvent>,
    cmd_rx: &Receiver<RecorderCommand>,
) -> Result<SessionOutcome> {
    let stream_linked = Arc::new(AtomicBool::new(false));
    let current_filename = Arc::new(Mutex::new(None::<String>));

    let pipeline = build_recorder_pipeline(
        camera_cfg,
        storage_cfg,
        event_tx.clone(),
        Arc::clone(&stream_linked),
        Arc::clone(&current_filename),
    )?;

    pipeline
        .set_state(gst::State::Playing)
        .context("無法將 3K 錄影 Pipeline 切換至 Playing 狀態")?;

    let bus = pipeline.bus().context("無法取得錄影 Pipeline Bus")?;
    let session_start = Instant::now();
    let outcome;

    loop {
        // 檢查來自 GUI 的控制指令
        if let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                RecorderCommand::Stop => {
                    info!("收到暫停錄影指令，正在送出 EOS 以安全寫入 MP4 moov 檔尾...");
                    outcome = SessionOutcome::PausedByUser;
                    break;
                }
                RecorderCommand::UpdateConfig(new_cfg) => {
                    info!("收到更新攝影機設定指令，正在安全寫入當前 MP4 檔尾並切換連線...");
                    outcome = SessionOutcome::ConfigUpdated(new_cfg);
                    break;
                }
                RecorderCommand::Shutdown => {
                    info!("收到系統關閉指令，正在送出 EOS 以安全寫入 MP4 moov 檔尾...");
                    outcome = SessionOutcome::Shutdown;
                    break;
                }
                RecorderCommand::Start => {}
            }
        }

        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    error!(
                        "3K 錄影管線 GStreamer 錯誤 (來自 {:?}): {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                    outcome = SessionOutcome::Disconnected;
                    break;
                }
                MessageView::Eos(_) => {
                    warn!("3K 錄影管線收到串流端 EOS，準備結束當前切片並重連...");
                    outcome = SessionOutcome::Disconnected;
                    break;
                }
                _ => {}
            }
        }

        // 若超過 15 秒仍未成功連接視訊 Pad，判定連線逾時並重試
        if !stream_linked.load(Ordering::Relaxed)
            && session_start.elapsed() > Duration::from_secs(15)
        {
            warn!("3K 錄影管線連線逾時 (15 秒未收到視訊串流)，觸發重連...");
            outcome = SessionOutcome::Disconnected;
            break;
        }
    }

    // 優雅關閉：若曾連接串流，先發送 EOS 確保 splitmuxsink / mp4mux 完整寫入 moov atom
    graceful_stop_pipeline(&pipeline, &bus, stream_linked.load(Ordering::Relaxed));

    Ok(outcome)
}

/// 確保正在錄製的 3K MP4 片段正常寫入檔尾 (`moov` atom)，絕不損毀已錄好的畫面
fn graceful_stop_pipeline(pipeline: &gst::Pipeline, bus: &gst::Bus, had_data: bool) {
    if had_data {
        if pipeline.send_event(gst::event::Eos::new()) {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::Eos(_) => {
                            info!("3K MP4 切片檔尾 (moov atom) 已完整寫入硬碟。");
                            break;
                        }
                        MessageView::Error(_) => break,
                        _ => {}
                    }
                }
            }
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
}

fn build_recorder_pipeline(
    camera_cfg: &CameraConfig,
    storage_cfg: &StorageConfig,
    event_tx: Sender<RecorderEvent>,
    stream_linked: Arc<AtomicBool>,
    current_filename: Arc<Mutex<Option<String>>>,
) -> Result<gst::Pipeline> {
    std::fs::create_dir_all(&storage_cfg.record_dir)
        .with_context(|| format!("無法建立錄影資料夾: {}", storage_cfg.record_dir.display()))?;

    let max_size_time_ns = storage_cfg
        .segment_duration_secs
        .saturating_mul(1_000_000_000);

    let pipeline = gst::Pipeline::with_name("foscam-main-recorder");

    let splitmux = gst::ElementFactory::make("splitmuxsink")
        .name("recorder_splitmux")
        .property("max-size-time", max_size_time_ns)
        .property("async-finalize", true)
        .build()
        .context("無法建立 splitmuxsink (請確認已安裝 gstreamer1.0-plugins-good)")?;

    // 設定 format-location Callback：依日期自動分資料夾存放
    // 例如: /home/user/Foscam_Recordings/2026-11-08/foscam_20261108_155000.mp4
    let record_dir_clone: PathBuf = storage_cfg.record_dir.clone();
    let tx_for_loc = event_tx.clone();
    let fname_store = Arc::clone(&current_filename);

    splitmux.connect("format-location", false, move |_values| {
        let filepath = match generate_segment_filepath(&record_dir_clone) {
            Ok(p) => p,
            Err(e) => {
                error!("建立分段錄影路徑失敗: {}", e);
                record_dir_clone.join("foscam_fallback.mp4")
            }
        };

        let short_name = filepath
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("foscam.mp4")
            .to_string();

        if let Ok(mut guard) = fname_store.lock() {
            *guard = Some(short_name.clone());
        }

        info!("開始寫入新 3K 錄影切片: {}", filepath.display());
        let _ = tx_for_loc.send(RecorderEvent::Status {
            is_recording: true,
            is_enabled: true,
            current_file: Some(short_name.clone()),
            message: format!("寫入中 ({})", short_name),
        });

        Some(filepath.to_string_lossy().into_owned().to_value())
    });

    if camera_cfg.mock_mode {
        // 模擬模式：產生 H.264 串流直入 splitmuxsink 測試分段存檔
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property_from_str("pattern", "smpte")
            .build()
            .context("無法建立 videotestsrc")?;
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 1280i32)
                    .field("height", 720i32)
                    .field("framerate", gst::Fraction::new(25, 1))
                    .build(),
            )
            .build()?;
        let enc = gst::ElementFactory::make("x264enc")
            .property_from_str("tune", "zerolatency")
            .property("key-int-max", 25u32)
            .property("bitrate", 2000u32)
            .build()
            .context("無法建立 x264enc (請確認已安裝 gstreamer1.0-plugins-ugly)")?;
        let parse = gst::ElementFactory::make("h264parse")
            .property("config-interval", -1i32)
            .build()?;

        pipeline.add_many([&src, &capsfilter, &enc, &parse, &splitmux])?;
        gst::Element::link_many([&src, &capsfilter, &enc, &parse])?;

        let src_pad = parse.static_pad("src").context("找不到 h264parse src pad")?;
        let video_sink_pad = splitmux
            .request_pad_simple("video")
            .context("無法向 splitmuxsink 請求 video pad")?;
        src_pad.link(&video_sink_pad)?;

        stream_linked.store(true, Ordering::Relaxed);
        return Ok(pipeline);
    }

    // 實體攝影機 RTSP 3K 主碼流 Zero-Decode Pass-through 管線
    let rtsp_url = camera_cfg.main_rtsp_url();
    let rtspsrc = gst::ElementFactory::make("rtspsrc")
        .name("main_rtspsrc")
        .property("location", &rtsp_url)
        .property("latency", 500u32)
        .property_from_str("protocols", "tcp")
        .build()
        .context("無法建立 rtspsrc")?;

    let video_queue = gst::ElementFactory::make("queue")
        .name("video_queue")
        .build()?;
    let depay = gst::ElementFactory::make("rtph264depay")
        .name("video_depay")
        .build()?;
    let parse = gst::ElementFactory::make("h264parse")
        .name("video_parse")
        // 在每個 IDR 關鍵影格插入 SPS/PPS，確保 splitmuxsink 切分後每個 MP4 都能獨立播放
        .property("config-interval", -1i32)
        .build()?;

    pipeline.add_many([&rtspsrc, &video_queue, &depay, &parse, &splitmux])?;
    gst::Element::link_many([&video_queue, &depay, &parse])?;

    let parse_src_pad = parse.static_pad("src").context("找不到 h264parse src pad")?;
    let splitmux_video_pad = splitmux
        .request_pad_simple("video")
        .context("無法向 splitmuxsink 請求 video pad")?;
    parse_src_pad
        .link(&splitmux_video_pad)
        .context("無法將 h264parse 連接至 splitmuxsink video pad")?;

    let enable_audio = camera_cfg.enable_audio;
    let pipeline_weak = pipeline.downgrade();
    let video_queue_weak = video_queue.downgrade();
    let splitmux_weak = splitmux.downgrade();

    rtspsrc.connect_pad_added(move |_src, src_pad| {
        let Some(caps) = src_pad.current_caps().or_else(|| Some(src_pad.query_caps(None))) else {
            return;
        };
        let Some(structure) = caps.structure(0) else {
            return;
        };

        let media = structure.get::<&str>("media").unwrap_or("");
        let encoding = structure.get::<&str>("encoding-name").unwrap_or("");

        if media == "video" && encoding.eq_ignore_ascii_case("H264") {
            if let Some(vq) = video_queue_weak.upgrade() {
                if let Some(sink_pad) = vq.static_pad("sink") {
                    if !sink_pad.is_linked() {
                        if let Err(err) = src_pad.link(&sink_pad) {
                            error!("3K 錄影管線連接 H.264 Pad 失敗: {:?}", err);
                        } else {
                            info!("3K 錄影管線已連接 /videoMain H.264 視訊軌 (Zero-Decode)");
                            stream_linked.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        } else if media == "audio" && enable_audio {
            // 支援 Foscam C5M 設定為 AAC 時的無損音軌封裝 (MPEG4-GENERIC 或 MP4A-LATM)
            let depay_factory = if encoding.eq_ignore_ascii_case("MPEG4-GENERIC") {
                Some("rtpmp4gdepay")
            } else if encoding.eq_ignore_ascii_case("MP4A-LATM") {
                Some("rtpmp4adepay")
            } else {
                warn!(
                    "偵測到非 AAC 音訊編碼 ({})，略過音軌封裝（請於攝影機後台將音訊設為 AAC）",
                    encoding
                );
                None
            };

            if let (Some(factory_name), Some(pipe), Some(mux)) = (
                depay_factory,
                pipeline_weak.upgrade(),
                splitmux_weak.upgrade(),
            ) {
                if let (Ok(aq), Ok(adepay), Ok(aparse)) = (
                    gst::ElementFactory::make("queue").build(),
                    gst::ElementFactory::make(factory_name).build(),
                    gst::ElementFactory::make("aacparse").build(),
                ) {
                    if pipe.add_many([&aq, &adepay, &aparse]).is_ok()
                        && gst::Element::link_many([&aq, &adepay, &aparse]).is_ok()
                    {
                        if let (Some(aparse_src), Some(mux_audio_pad)) =
                            (aparse.static_pad("src"), mux.request_pad_simple("audio_%u"))
                        {
                            if aparse_src.link(&mux_audio_pad).is_ok() {
                                let _ = aq.sync_state_with_parent();
                                let _ = adepay.sync_state_with_parent();
                                let _ = aparse.sync_state_with_parent();
                                if let Some(aq_sink) = aq.static_pad("sink") {
                                    if src_pad.link(&aq_sink).is_ok() {
                                        info!("3K 錄影管線已同步掛載 AAC 音訊軌無損封裝");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_mock_recorder_writes_valid_mp4_segment() {
        gst::init().unwrap();
        let temp_dir =
            std::env::temp_dir().join(format!("rnvr_rec_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);

        let mut cam_cfg = CameraConfig {
            name: "TestCam".to_string(),
            ip: "127.0.0.1".to_string(),
            port: 88,
            username: "".to_string(),
            password: "".to_string(),
            main_stream_path: "/videoMain".to_string(),
            sub_stream_path: "/videoSub".to_string(),
            enable_audio: false,
            use_hw_accel: false,
            mock_mode: true,
        };
        cam_cfg.mock_mode = true;

        let storage_cfg = StorageConfig {
            record_dir: temp_dir.clone(),
            segment_duration_secs: 60,
            max_storage_gb: 10.0,
            cleanup_interval_secs: 60,
            auto_start_recording: true,
        };

        let (event_tx, event_rx) = flume::unbounded();
        let (cmd_tx, cmd_rx) = flume::unbounded();

        spawn_recorder_worker(cam_cfg, storage_cfg, event_tx, cmd_rx);

        // 等待約 1.5 秒讓 splitmuxsink 寫入模擬 H.264 影格
        thread::sleep(Duration::from_millis(1500));

        // 發送 Shutdown 觸發 EOS 與 moov atom 寫入
        cmd_tx.send(RecorderCommand::Shutdown).unwrap();
        thread::sleep(Duration::from_millis(800));

        // 確認有收到寫入狀態通知且 MP4 檔案已產生且大於 0 bytes
        let mut recorded_file = None;
        while let Ok(ev) = event_rx.try_recv() {
            let RecorderEvent::Status {
                is_recording,
                current_file,
                ..
            } = ev;
            if is_recording && current_file.is_some() {
                recorded_file = current_file;
            }
        }
        assert!(recorded_file.is_some(), "應該產生錄影切片檔名");

        let stats = crate::storage_mgr::enforce_retention_policy(&temp_dir, 10_000_000, false);
        assert!(stats.file_count >= 1, "應該至少有 1 個 MP4 檔案");
        assert!(stats.used_bytes > 1000, "MP4 檔案大小應大於 1KB");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
