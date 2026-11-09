use crate::config::CameraConfig;
use anyhow::{Context, Result};
use flume::{Receiver, Sender};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use iced::widget::image as iced_image;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// 硬體 H.264 解碼器候選清單（依現代 GStreamer 推薦優先序排列）
pub const HW_DECODER_CANDIDATES: &[(&str, &str)] = &[
    ("vah264dec", "VA-API (gst-plugins-bad va)"),
    ("vaapih264dec", "VA-API (gstreamer1.0-vaapi)"),
    ("nvh264dec", "NVIDIA NVDEC"),
    ("v4l2slh264dec", "V4L2 Stateless HW"),
    ("v4l2h264dec", "V4L2 Stateful HW"),
];

/// 偵測系統目前可用的最佳硬體 H.264 解碼器名稱
pub fn detect_available_hw_decoder() -> Option<(&'static str, &'static str)> {
    for &(elem, desc) in HW_DECODER_CANDIDATES {
        if gst::ElementFactory::find(elem).is_some() {
            return Some((elem, desc));
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    /// 直接儲存 Iced 的引用計數影像 Handle (內部為 `bytes::Bytes`)，
    /// 傳遞至 GUI 與截圖時皆為 O(1) Zero-Copy，避免每幀重複配置與複製 3.68MB 記憶體。
    pub handle: iced_image::Handle,
}

impl PreviewFrame {
    pub fn rgba_bytes(&self) -> Option<&[u8]> {
        match &self.handle {
            iced_image::Handle::Rgba { pixels, .. } => Some(pixels.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ViewerEvent {
    Frame(PreviewFrame),
    Status {
        connected: bool,
        paused: bool,
        fps: f32,
        decoder_name: String,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub enum ViewerCommand {
    Pause,
    Resume,
    UpdateConfig(CameraConfig),
    Shutdown,
}

/// 啟動 Pipeline 2：720p 副碼流預覽專線 (獨立背景執行緒 + 支援省電暫停與自動重連)
pub fn spawn_viewer_worker(
    camera_cfg: CameraConfig,
    event_tx: Sender<ViewerEvent>,
    cmd_rx: Receiver<ViewerCommand>,
) {
    thread::Builder::new()
        .name("gst-viewer-sub".to_string())
        .spawn(move || {
            info!("啟動 720p 預覽管線執行緒 ({})", camera_cfg.name);
            run_viewer_loop(camera_cfg, event_tx, cmd_rx);
        })
        .expect("無法建立 gst-viewer-sub 執行緒");
}

fn run_viewer_loop(
    mut camera_cfg: CameraConfig,
    event_tx: Sender<ViewerEvent>,
    cmd_rx: Receiver<ViewerCommand>,
) {
    let mut preview_enabled = camera_cfg.preview_enabled;
    let mut force_sw_decoder = false;

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                ViewerCommand::Pause => {
                    preview_enabled = false;
                }
                ViewerCommand::Resume => {
                    preview_enabled = true;
                }
                ViewerCommand::Shutdown => {
                    info!("預覽管線收到 Shutdown 指令，結束執行緒。");
                    return;
                }
                ViewerCommand::UpdateConfig(new_cfg) => {
                    info!(
                        "預覽管線套用新攝影機設定: {}:{} (硬體加速={}, 最大FPS={})",
                        new_cfg.ip, new_cfg.port, new_cfg.use_hw_accel, new_cfg.preview_max_fps
                    );
                    preview_enabled = new_cfg.preview_enabled;
                    force_sw_decoder = false;
                    camera_cfg = new_cfg;
                }
            }
        }

        // 若處於「暫停預覽 (省電純錄影模式)」，完全關閉 RTSP 副碼流與解碼器，阻塞等待喚醒指令 (0% CPU)
        if !preview_enabled {
            let _ = event_tx.send(ViewerEvent::Status {
                connected: false,
                paused: true,
                fps: 0.0,
                decoder_name: "休眠省電".to_string(),
                message: "已暫停預覽 (省電純 3K 錄影模式)".to_string(),
            });

            while !preview_enabled {
                match cmd_rx.recv() {
                    Ok(ViewerCommand::Resume) => {
                        preview_enabled = true;
                        info!("使用者恢復 720p 即時預覽管線");
                    }
                    Ok(ViewerCommand::Pause) => continue,
                    Ok(ViewerCommand::UpdateConfig(new_cfg)) => {
                        preview_enabled = new_cfg.preview_enabled;
                        force_sw_decoder = false;
                        camera_cfg = new_cfg;
                    }
                    Ok(ViewerCommand::Shutdown) | Err(_) => {
                        info!("預覽管線結束執行緒");
                        return;
                    }
                }
            }
        }

        let _ = event_tx.send(ViewerEvent::Status {
            connected: false,
            paused: false,
            fps: 0.0,
            decoder_name: "...".to_string(),
            message: if camera_cfg.mock_mode {
                "連線中 (模擬訊號源 720p)...".to_string()
            } else {
                format!(
                    "連線中 ({}:{}{})...",
                    camera_cfg.ip, camera_cfg.port, camera_cfg.sub_stream_path
                )
            },
        });

        let mut immediate_reconnect = false;
        match run_single_viewer_session(&camera_cfg, force_sw_decoder, &event_tx, &cmd_rx) {
            Ok(ViewerSessionOutcome::Exit) => break,
            Ok(ViewerSessionOutcome::PausedByUser) => {
                preview_enabled = false;
                continue;
            }
            Ok(ViewerSessionOutcome::ConfigUpdated(new_cfg)) => {
                info!(
                    "預覽管線立即切換至新設定: {}:{} (硬體加速={}, 最大FPS={})",
                    new_cfg.ip, new_cfg.port, new_cfg.use_hw_accel, new_cfg.preview_max_fps
                );
                preview_enabled = new_cfg.preview_enabled;
                force_sw_decoder = false;
                camera_cfg = new_cfg;
                immediate_reconnect = true;
            }
            Ok(ViewerSessionOutcome::FallbackToSwDecoder) => {
                warn!("硬體解碼器初始化或解碼失敗，自動降級為 CPU 軟體解碼 (avdec_h264) 並立即重連...");
                force_sw_decoder = true;
                immediate_reconnect = true;
            }
            Ok(ViewerSessionOutcome::Reconnect) => {}
            Err(err) => {
                warn!("720p 預覽管線異常中斷: {:#}", err);
                if camera_cfg.use_hw_accel && !force_sw_decoder {
                    warn!("嘗試切換為 CPU 軟體解碼 (avdec_h264) 重試...");
                    force_sw_decoder = true;
                    immediate_reconnect = true;
                }
            }
        }

        if immediate_reconnect {
            continue;
        }

        // 斷線後每 5 秒自動重連 (期間可隨時回應暫停預覽、更新設定或關閉指令)
        for remaining in (1..=5).rev() {
            let _ = event_tx.send(ViewerEvent::Status {
                connected: false,
                paused: false,
                fps: 0.0,
                decoder_name: "-".to_string(),
                message: format!("預覽斷線，{} 秒後自動重連...", remaining),
            });
            match cmd_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(ViewerCommand::Pause) => {
                    preview_enabled = false;
                    break;
                }
                Ok(ViewerCommand::Resume) => {}
                Ok(ViewerCommand::Shutdown) => return,
                Ok(ViewerCommand::UpdateConfig(new_cfg)) => {
                    info!(
                        "預覽管線於倒數期間收到新設定，立即重連: {}:{}",
                        new_cfg.ip, new_cfg.port
                    );
                    preview_enabled = new_cfg.preview_enabled;
                    force_sw_decoder = false;
                    camera_cfg = new_cfg;
                    break;
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

enum ViewerSessionOutcome {
    Reconnect,
    PausedByUser,
    FallbackToSwDecoder,
    ConfigUpdated(CameraConfig),
    Exit,
}

/// 建立並執行單次預覽 Pipeline
fn run_single_viewer_session(
    camera_cfg: &CameraConfig,
    force_sw_decoder: bool,
    event_tx: &Sender<ViewerEvent>,
    cmd_rx: &Receiver<ViewerCommand>,
) -> Result<ViewerSessionOutcome> {
    let frame_counter = Arc::new(AtomicU64::new(0));
    let got_first_frame = Arc::new(AtomicBool::new(false));

    let (pipeline, appsink, decoder_label) = build_viewer_pipeline(camera_cfg, force_sw_decoder)?;
    let using_hw_decoder = decoder_label != "avdec_h264" && decoder_label != "mock";

    let tx_clone = event_tx.clone();
    let counter_clone = Arc::clone(&frame_counter);
    let first_frame_clone = Arc::clone(&got_first_frame);

    // 依 preview_max_fps 計算最小幀間距，在轉換與配置記憶體前直接節流，節省 CPU 與 GPU 頻寬
    let max_fps = camera_cfg.preview_max_fps.clamp(1, 60);
    let min_interval = if max_fps < 60 {
        // 留 5% 彈性避免因計時抖動誤丟正常速率的畫面幀
        Some(Duration::from_secs_f64(0.95 / (max_fps as f64)))
    } else {
        None
    };
    let last_emit_instant = Arc::new(Mutex::new(None::<Instant>));

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;

                // 1. 若 GUI 佇列已滿，代表 UI 尚未消化完上一幀，直接略過像素複製以節省 CPU
                if tx_clone.is_full() {
                    return Ok(gst::FlowSuccess::Ok);
                }

                // 2. 幀率上限節流檢查 (Frame Throttling)
                let now = Instant::now();
                if let Some(interval) = min_interval {
                    if let Ok(mut guard) = last_emit_instant.lock() {
                        if let Some(prev) = *guard {
                            if now.duration_since(prev) < interval {
                                return Ok(gst::FlowSuccess::Ok);
                            }
                        }
                        *guard = Some(now);
                    }
                }

                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let video_info =
                    gst_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;

                let frame =
                    gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &video_info)
                        .map_err(|_| gst::FlowError::Error)?;

                let width = frame.width();
                let height = frame.height();
                let stride = frame.plane_stride()[0] as usize;
                let plane_data = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                let row_bytes = (width as usize) * 4;

                let mut rgba = Vec::with_capacity(row_bytes * (height as usize));
                if stride == row_bytes {
                    rgba.extend_from_slice(&plane_data[..row_bytes * (height as usize)]);
                } else {
                    for y in 0..(height as usize) {
                        let start = y * stride;
                        let end = start + row_bytes;
                        if end <= plane_data.len() {
                            rgba.extend_from_slice(&plane_data[start..end]);
                        }
                    }
                }

                counter_clone.fetch_add(1, Ordering::Relaxed);
                first_frame_clone.store(true, Ordering::Relaxed);

                // 將 Vec<u8> 零複製轉為 Iced Handle (內部為 bytes::Bytes)
                let handle = iced_image::Handle::from_rgba(width, height, rgba);
                let _ = tx_clone.try_send(ViewerEvent::Frame(PreviewFrame {
                    width,
                    height,
                    handle,
                }));

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline
        .set_state(gst::State::Playing)
        .context("無法將 720p 預覽 Pipeline 切換至 Playing 狀態")?;

    let bus = pipeline.bus().context("無法取得預覽 Pipeline Bus")?;

    let mut last_fps_calc = Instant::now();
    let mut last_frame_time = Instant::now();
    let mut prev_frames = 0u64;
    let mut outcome = ViewerSessionOutcome::Reconnect;

    loop {
        if let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                ViewerCommand::Pause => {
                    info!("收到暫停預覽指令，釋放 720p 預覽解碼管線以進入省電模式...");
                    outcome = ViewerSessionOutcome::PausedByUser;
                    break;
                }
                ViewerCommand::Resume => {}
                ViewerCommand::Shutdown => {
                    outcome = ViewerSessionOutcome::Exit;
                    break;
                }
                ViewerCommand::UpdateConfig(new_cfg) => {
                    outcome = ViewerSessionOutcome::ConfigUpdated(new_cfg);
                    break;
                }
            }
        }

        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    error!(
                        "預覽管線 GStreamer 錯誤 (來自 {:?}): {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                    if using_hw_decoder && !got_first_frame.load(Ordering::Relaxed) {
                        outcome = ViewerSessionOutcome::FallbackToSwDecoder;
                    }
                    break;
                }
                MessageView::Eos(_) => {
                    warn!("預覽管線收到 EOS 訊號，準備重連...");
                    break;
                }
                _ => {}
            }
        }

        let now = Instant::now();
        let elapsed = now.duration_since(last_fps_calc).as_secs_f32();
        if elapsed >= 2.0 {
            let current_frames = frame_counter.load(Ordering::Relaxed);
            let delta = current_frames.saturating_sub(prev_frames);
            prev_frames = current_frames;
            last_fps_calc = now;

            if delta > 0 {
                last_frame_time = now;
                let fps = (delta as f32) / elapsed;
                let _ = event_tx.send(ViewerEvent::Status {
                    connected: true,
                    paused: false,
                    fps,
                    decoder_name: decoder_label.clone(),
                    message: format!("連線正常 ({:.1} FPS, {})", fps, decoder_label),
                });
            } else if got_first_frame.load(Ordering::Relaxed)
                && now.duration_since(last_frame_time) > Duration::from_secs(10)
            {
                warn!("預覽管線已超過 10 秒未收到畫面幀，觸發自動重連...");
                break;
            } else if !got_first_frame.load(Ordering::Relaxed)
                && now.duration_since(last_frame_time) > Duration::from_secs(15)
            {
                warn!("預覽管線連線逾時 (15 秒無畫面)，觸發自動重連...");
                if using_hw_decoder {
                    outcome = ViewerSessionOutcome::FallbackToSwDecoder;
                }
                break;
            }
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
    Ok(outcome)
}

fn build_viewer_pipeline(
    camera_cfg: &CameraConfig,
    force_sw_decoder: bool,
) -> Result<(gst::Pipeline, gst_app::AppSink, String)> {
    if camera_cfg.mock_mode {
        let pipeline_str = "videotestsrc is-live=true pattern=ball ! \
            video/x-raw,width=1280,height=720,framerate=15/1 ! \
            videoconvert ! \
            video/x-raw,format=RGBA,width=1280,height=720 ! \
            appsink name=preview_sink max-buffers=1 drop=true sync=false";
        let element = gst::parse::launch(pipeline_str).context("無法建立模擬預覽 Pipeline")?;
        let pipeline = element
            .dynamic_cast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("無法轉型為 gst::Pipeline"))?;
        let sink = pipeline
            .by_name("preview_sink")
            .context("找不到 preview_sink")?
            .dynamic_cast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("無法轉型為 AppSink"))?;
        return Ok((pipeline, sink, "mock".to_string()));
    }

    // 選擇硬體加速解碼器 (vah264dec / vaapih264dec / nvh264dec / v4l2) 或純 CPU 解碼器 (avdec_h264)
    let decoder_name = if camera_cfg.use_hw_accel && !force_sw_decoder {
        if let Some((hw_elem, hw_desc)) = detect_available_hw_decoder() {
            info!("預覽管線啟用硬體加速解碼器: {} ({})", hw_elem, hw_desc);
            hw_elem
        } else {
            info!("系統未偵測到可用的硬體 H.264 解碼器，使用 CPU 軟體解碼: avdec_h264");
            "avdec_h264"
        }
    } else {
        "avdec_h264"
    };

    let rtsp_url = camera_cfg.sub_rtsp_url();
    let pipeline = gst::Pipeline::with_name("foscam-sub-viewer");

    let rtspsrc = gst::ElementFactory::make("rtspsrc")
        .name("sub_rtspsrc")
        .property("location", &rtsp_url)
        .property("latency", 200u32)
        // protocols=4 代表 GST_RTSP_LOWER_TRANS_TCP (強制 TCP 避免掉包破圖)
        .property_from_str("protocols", "tcp")
        .build()
        .context("無法建立 rtspsrc (請確認已安裝 gstreamer1.0-plugins-good)")?;

    let queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 2u32)
        .property_from_str("leaky", "downstream")
        .build()
        .context("無法建立 queue")?;

    let depay = gst::ElementFactory::make("rtph264depay")
        .build()
        .context("無法建立 rtph264depay")?;
    let parse = gst::ElementFactory::make("h264parse")
        .build()
        .context("無法建立 h264parse (請確認已安裝 gstreamer1.0-plugins-bad)")?;
    let decoder = gst::ElementFactory::make(decoder_name)
        .build()
        .with_context(|| {
            format!(
                "無法建立 {} (請確認已安裝 gstreamer1.0-libav 或硬體解碼驅動)",
                decoder_name
            )
        })?;

    // 若為軟體解碼 avdec_h264，限制執行緒數為 2 以避免在多核/舊款 CPU 上產生過度執行緒切換開銷
    if decoder_name == "avdec_h264" && decoder.find_property("max-threads").is_some() {
        decoder.set_property("max-threads", 2i32);
    }

    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("無法建立 videoconvert")?;
    if convert.find_property("n-threads").is_some() {
        convert.set_property("n-threads", 2u32);
    }

    // 僅要求輸出為 RGBA，不強制執行 CPU videoscale 放大，保留副碼流原生解析度交由 GPU 顯示
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .build();

    let appsink = gst_app::AppSink::builder()
        .name("preview_sink")
        .caps(&caps)
        .max_buffers(1)
        .drop(true)
        .sync(false)
        .build();

    pipeline.add_many([
        &rtspsrc,
        &queue,
        &depay,
        &parse,
        &decoder,
        &convert,
        appsink.upcast_ref(),
    ])?;

    gst::Element::link_many([
        &queue,
        &depay,
        &parse,
        &decoder,
        &convert,
        appsink.upcast_ref(),
    ])
    .context("無法串接預覽解碼管線元件")?;

    // 動態連接 rtspsrc 的 H.264 視訊 Pad (忽略副碼流音訊軌以節省資源)
    let queue_weak = queue.downgrade();
    rtspsrc.connect_pad_added(move |_src, src_pad| {
        let Some(queue_el) = queue_weak.upgrade() else {
            return;
        };
        let Some(caps) = src_pad.current_caps().or_else(|| Some(src_pad.query_caps(None))) else {
            return;
        };
        let Some(structure) = caps.structure(0) else {
            return;
        };

        let media = structure.get::<&str>("media").unwrap_or("");
        let encoding = structure.get::<&str>("encoding-name").unwrap_or("");

        if media == "video" && encoding.eq_ignore_ascii_case("H264") {
            if let Some(sink_pad) = queue_el.static_pad("sink") {
                if !sink_pad.is_linked() {
                    if let Err(err) = src_pad.link(&sink_pad) {
                        warn!("預覽管線連接 H264 Pad 失敗: {:?}", err);
                    } else {
                        info!("預覽管線成功連接 /videoSub H.264 視訊串流");
                    }
                }
            }
        }
    });

    Ok((pipeline, appsink, decoder_name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_viewer_outputs_720p_rgba_frames() {
        gst::init().unwrap();
        let cam_cfg = CameraConfig {
            name: "TestCam".to_string(),
            ip: "127.0.0.1".to_string(),
            port: 88,
            username: "".to_string(),
            password: "".to_string(),
            main_stream_path: "/videoMain".to_string(),
            sub_stream_path: "/videoSub".to_string(),
            enable_audio: false,
            use_hw_accel: false,
            preview_max_fps: 15,
            preview_enabled: true,
            mock_mode: true,
        };

        let (event_tx, event_rx) = flume::bounded(2);
        let (cmd_tx, cmd_rx) = flume::unbounded();

        spawn_viewer_worker(cam_cfg, event_tx, cmd_rx);

        let mut got_frame = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(ViewerEvent::Frame(frame)) = event_rx.recv_timeout(Duration::from_millis(200))
            {
                got_frame = Some(frame);
                break;
            }
        }

        let _ = cmd_tx.send(ViewerCommand::Shutdown);
        let frame = got_frame.expect("應在 5 秒內從 AppSink 收到 720p RGBA 畫面幀");
        assert_eq!(frame.width, 1280);
        assert_eq!(frame.height, 720);
        assert_eq!(
            frame.rgba_bytes().expect("應包含 RGBA Bytes").len(),
            1280 * 720 * 4
        );
    }
}
