use crate::config::CameraConfig;
use anyhow::{Context, Result};
use flume::{Receiver, Sender};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum ViewerEvent {
    Frame(PreviewFrame),
    Status {
        connected: bool,
        fps: f32,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub enum ViewerCommand {
    UpdateConfig(CameraConfig),
    Shutdown,
}

/// 啟動 Pipeline 2：720p 副碼流預覽專線 (獨立背景執行緒 + 5 秒自動重連迴圈)
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
    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                ViewerCommand::Shutdown => {
                    info!("預覽管線收到 Shutdown 指令，結束執行緒。");
                    return;
                }
                ViewerCommand::UpdateConfig(new_cfg) => {
                    info!("預覽管線套用新攝影機連線設定: {}:{}", new_cfg.ip, new_cfg.port);
                    camera_cfg = new_cfg;
                }
            }
        }

        let _ = event_tx.send(ViewerEvent::Status {
            connected: false,
            fps: 0.0,
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
        match run_single_viewer_session(&camera_cfg, &event_tx, &cmd_rx) {
            Ok(ViewerSessionOutcome::Exit) => break,
            Ok(ViewerSessionOutcome::ConfigUpdated(new_cfg)) => {
                info!("預覽管線立即切換至新攝影機設定: {}:{}", new_cfg.ip, new_cfg.port);
                camera_cfg = new_cfg;
                immediate_reconnect = true;
            }
            Ok(ViewerSessionOutcome::Reconnect) => {}
            Err(err) => {
                warn!("720p 預覽管線異常中斷: {:#}", err);
            }
        }

        if immediate_reconnect {
            continue;
        }

        // 斷線後每 5 秒自動重連 (若收到新設定則立即跳出倒數並重連)
        for remaining in (1..=5).rev() {
            let _ = event_tx.send(ViewerEvent::Status {
                connected: false,
                fps: 0.0,
                message: format!("預覽斷線，{} 秒後自動重連...", remaining),
            });
            match cmd_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(ViewerCommand::Shutdown) => return,
                Ok(ViewerCommand::UpdateConfig(new_cfg)) => {
                    info!("預覽管線於倒數期間收到新設定，立即重連: {}:{}", new_cfg.ip, new_cfg.port);
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
    ConfigUpdated(CameraConfig),
    Exit,
}

/// 建立並執行單次預覽 Pipeline
fn run_single_viewer_session(
    camera_cfg: &CameraConfig,
    event_tx: &Sender<ViewerEvent>,
    cmd_rx: &Receiver<ViewerCommand>,
) -> Result<ViewerSessionOutcome> {
    let frame_counter = Arc::new(AtomicU64::new(0));
    let got_first_frame = Arc::new(AtomicBool::new(false));

    let (pipeline, appsink) = build_viewer_pipeline(camera_cfg)?;

    let tx_clone = event_tx.clone();
    let counter_clone = Arc::clone(&frame_counter);
    let first_frame_clone = Arc::clone(&got_first_frame);

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
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

                // 使用 try_send 避免當 GUI 忙碌時阻塞 GStreamer 解碼執行緒
                let _ = tx_clone.try_send(ViewerEvent::Frame(PreviewFrame {
                    width,
                    height,
                    rgba,
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

        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    error!(
                        "預覽管線 GStreamer 錯誤 (來自 {:?}): {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
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
        if elapsed >= 1.0 {
            let current_frames = frame_counter.load(Ordering::Relaxed);
            let delta = current_frames.saturating_sub(prev_frames);
            prev_frames = current_frames;
            last_fps_calc = now;

            if delta > 0 {
                last_frame_time = now;
                let fps = (delta as f32) / elapsed;
                let _ = event_tx.send(ViewerEvent::Status {
                    connected: true,
                    fps,
                    message: format!("連線正常 ({:.1} FPS)", fps),
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
                break;
            }
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
    Ok(outcome)
}

fn build_viewer_pipeline(
    camera_cfg: &CameraConfig,
) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    if camera_cfg.mock_mode {
        let pipeline_str = "videotestsrc is-live=true pattern=ball ! \
            video/x-raw,width=1280,height=720,framerate=15/1 ! \
            videoconvert ! \
            video/x-raw,format=RGBA,width=1280,height=720 ! \
            appsink name=preview_sink max-buffers=1 drop=true sync=false";
        let element = gst::parse::launch(pipeline_str)
            .context("無法建立模擬預覽 Pipeline")?;
        let pipeline = element
            .dynamic_cast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("無法轉型為 gst::Pipeline"))?;
        let sink = pipeline
            .by_name("preview_sink")
            .context("找不到 preview_sink")?
            .dynamic_cast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("無法轉型為 AppSink"))?;
        return Ok((pipeline, sink));
    }

    // 選擇硬體加速解碼器 (vaapih264dec) 或純 CPU 解碼器 (avdec_h264)
    let decoder_name = if camera_cfg.use_hw_accel
        && gst::ElementFactory::find("vaapih264dec").is_some()
    {
        info!("預覽管線啟用硬體加速解碼器: vaapih264dec");
        "vaapih264dec"
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
        .property("max-size-buffers", 3u32)
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
                "無法建立 {} (請確認已安裝 gstreamer1.0-libav)",
                decoder_name
            )
        })?;
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("無法建立 videoconvert")?;
    let scale = gst::ElementFactory::make("videoscale")
        .build()
        .context("無法建立 videoscale")?;

    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .field("width", 1280i32)
        .field("height", 720i32)
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
        &scale,
        appsink.upcast_ref(),
    ])?;

    gst::Element::link_many([
        &queue,
        &depay,
        &parse,
        &decoder,
        &convert,
        &scale,
        appsink.upcast_ref(),
    ])
    .context("無法串接預覽解碼管線元件")?;

    // 動態連接 rtspsrc 的 H.264 視訊 Pad
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

    Ok((pipeline, appsink))
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
            mock_mode: true,
        };

        let (event_tx, event_rx) = flume::unbounded();
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
        assert_eq!(frame.rgba.len(), 1280 * 720 * 4);
    }
}
