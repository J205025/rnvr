# Ubuntu Rust NVR 監控錄影系統 — 單機雙碼流分離架構規劃書

## 1. 專案目標與硬體環境

本專案旨在 Ubuntu Linux 桌面環境下，使用 **Rust** 語言開發一套專為 **單支 Foscam C5M** 設計的超低負載桌面監控與循環錄影 GUI 軟體。

核心設計採用 **「主副碼流分離架構 (Dual-Stream Split Architecture)」**：
* **錄影管線 (Recorder)**：拉取 $3\text{K}$ 主碼流 (`/videoMain`)，**完全不經過 CPU/GPU 解碼**，直接將 H.264 封包無損封裝寫入硬碟。
* **預覽管線 (Viewer)**：拉取 $720\text{p}$ 副碼流 (`/videoSub`)，僅針對低解析度畫面進行解碼並送交 GUI 渲染，大幅降低處理器與記憶體頻寬負擔。

---

### 1.1 攝影機規格與網路配置 (Foscam C5M x 1)

* **機型**：Foscam C5M (單台部署)
* **連線方式**：RJ45 實體網路線 (10/100Mbps) 或 5GHz Wi-Fi（建議於路由器綁定固定區網 IP）
* **供電方式**：USB Type-C (5V/1A)
* **影像編碼**：標準 `H.264`
* **音訊編碼**：於攝影機後台設定為 **`AAC`**（確保錄影管線可直接將音軌與 H.264 視訊無損封裝進 `.mp4` 容器）
* **RTSP 預設連接埠**：`88`（或標準 `554`，依韌體設定為準）
* **雙碼流路徑配置**：
  * **錄影專用 — 主碼流 (3K 5MP, $3072 \times 1728$ @ 25fps)**：
    `rtsp://<user>:<password>@<IP>:88/videoMain`
  * **預覽專用 — 副碼流 (720p, $1280 \times 720$ @ 15fps)**：
    `rtsp://<user>:<password>@<IP>:88/videoSub`

---

## 2. 核心技術棧選型 (Tech Stack)

| 系統層級 | 選用 Crate / 套件 | 版本建議 | 選用理由與職責 |
| :--- | :--- | :--- | :--- |
| **GUI 框架** | `iced` | `0.13+` | 純 Rust Elm 架構，提供單一大型監控主畫面與底部狀態控制列。 |
| **多媒體管線** | `gstreamer` (`gstreamer-rs`) | `0.23+` | 分別管理「錄影」與「預覽」兩條獨立 Pipeline，互不干擾。 |
| **視訊幀處理** | `gstreamer-app`, `gstreamer-video` | `0.23+` | 透過 `AppSink` 擷取 `/videoSub` 解碼後的 $1280 \times 720$ RGBA 畫面幀。 |
| **非同步執行期** | `tokio` | `1.x` | 負責背景硬碟空間巡檢、自動刪除舊檔 (Retention Policy)、以及斷線重連計時。 |
| **跨執行緒通訊** | `flume` | `0.11+` | 高效能 Channel，將預覽管線的 RGBA 畫面幀與錄影管線狀態傳遞至 Iced GUI。 |
| **設定檔解析** | `serde`, `toml` | `1.0+` | 讀取 `config.toml`（攝影機連線資訊、存檔路徑、容量上限）。 |

---

## 3. 主副碼流分離架構 (Dual-Stream Architecture)

將「錄影」與「預覽」拆分為兩個完全獨立的 GStreamer Pipeline。這樣做的好處不僅是 **CPU 負載極低**，而且當你關閉或最小化預覽畫面時，背景的 $3\text{K}$ 錄影管線依然能零中斷運作。

```text
                  ┌──> [Pipeline 1: 3K 錄影專線 (Zero-Decode Pass-through)]
                  │      rtsp://<IP>:88/videoMain
                  │        └──> rtspsrc (protocols=tcp, latency=500)
                  │        └──> rtph264depay -> h264parse
                  │        └──> splitmuxsink (每 10 分鐘切一個 3K .mp4 檔，CPU 佔用 < 1%)
[Foscam C5M] ─────┤
 (單機雙碼流)     │
                  └──> [Pipeline 2: 720p 預覽專線 (Lightweight Decode)]
                         rtsp://<IP>:88/videoSub
                           └──> rtspsrc (protocols=tcp, latency=200)
                           └──> rtph264depay -> h264parse -> avdec_h264 (或 vaapih264dec)
                           └──> videoconvert (轉 1280x720 RGBA)
                           └──> appsink (max-buffers=1, drop=true)
                                       │
                                (flume channel)
                                       ▼
                           [Iced 單一主畫面 GUI 渲染]
```

### 為什麼單機採用「方案 B（雙碼流分離）」特別有優勢？
1. **省去 $3\text{K} \rightarrow 720\text{p}$ 的解碼與縮放成本**：解碼 $3072 \times 1728$ 的像素量是 $1280 \times 720$ 的 **$5.76$ 倍**。直接拉取攝影機硬體壓縮好的 `/videoSub` ($720\text{p}$)，連無內顯的迷你主機或舊筆電都能輕鬆用純 CPU (`avdec_h264`) 流暢預覽。
2. **故障隔離 (Fault Isolation)**：錄影管線與 GUI 顯示管線完全脫鉤，即便調整視窗大小或暫停預覽，也不會影響正在寫入磁碟的 `.mp4` 檔案完整性。

---

## 4. 軟體專案目錄與模組職責

針對單機雙管線簡化後的專案結構：

```text
foscam-single-nvr/
├── Cargo.toml
├── config.toml            # 單支攝影機連線參數與存檔設定
└── src/
    ├── main.rs            # 程式進入點：初始化 GStreamer、載入設定、啟動 Iced 視窗
    ├── config.rs          # Serde 設定檔結構定義
    ├── app.rs             # Iced GUI 主畫面 (單一 16:9 視訊檢視區 + 底部狀態與控制面板)
    ├── recorder.rs        # [Pipeline 1] 負責 /videoMain 3K 無損分段錄影與斷線自動重連
    ├── viewer.rs          # [Pipeline 2] 負責 /videoSub 720p 解碼預覽與 AppSink 畫面幀輸出
    └── storage_mgr.rs     # 循環錄影管理員：定時檢查硬碟空間並自動清理最舊的 MP4 檔案
```

### 4.1 設定檔結構設計 (`config.toml`)

```toml
[camera]
name = "大門監控 (Foscam C5M)"
ip = "192.168.1.101"
port = 88
username = "nvr_admin"
password = "your_password"
main_stream_path = "/videoMain"   # 3K 錄影用
sub_stream_path = "/videoSub"     # 720p 預覽用

[storage]
record_dir = "/home/user/Foscam_Recordings"
segment_duration_secs = 600       # 每 10 分鐘 (600秒) 切分一個 MP4 檔案
max_storage_gb = 100              # 錄影資料夾最大容量上限 (例如 100 GB)
cleanup_interval_secs = 300       # 每 5 分鐘檢查一次硬碟空間
auto_start_recording = true       # 啟動軟體時自動開始 3K 錄影
```

---

## 5. 關鍵機制設計細節

### 5.1 單機專屬 GUI 介面佈局 (`app.rs`)

由於只有一支攝影機，不需要四分割格，改採更清晰實用的**「上方主監控畫面 + 下方狀態控制列」**佈局：

```text
┌──────────────────────────────────────────────────────────┐
│  [● REC 3K] 大門監控 (Foscam C5M)          2026-11-08 15:57 │
│                                                          │
│                                                          │
│               1280 x 720 即時預覽畫面區                    │
│                 (自動等比例縮放填滿視窗)                    │
│                                                          │
│                                                          │
├──────────────────────────────────────────────────────────┤
│ 預覽狀態: 連線正常 (15 FPS)  | 錄影狀態: 寫入中 (cam_...mp4) │
│ 磁碟空間: [████████░░░░░░░░] 52.4 GB / 100.0 GB          │
│ [ 暫停/開始錄影 ]   [ 畫面截圖 ]   [ 開啟錄影資料夾 ]         │
└──────────────────────────────────────────────────────────┘
```

### 5.2 獨立斷線自動重連機制 (`recorder.rs` & `viewer.rs`)

1. `recorder.rs` 與 `viewer.rs` 各自管理自己的 `gst::Pipeline` 與 `gst::Bus` 監聽迴圈。
2. 兩者的 `rtspsrc` 皆設定強制 TCP (`protocols=tcp`) 避免掉包破圖。
3. 當 `recorder.rs` 偵測到網路閃斷 (`Error` 或 `Eos`)：
   * 立即送出 EOS / 切換至 `gst::State::Null`，確保正在錄製的 $3\text{K}$ MP4 片段正常寫入檔尾 (`moov` atom)，**絕不損毀已錄好的畫面**。
   * 每 5 秒自動重試連線，一旦恢復即自動建立新切片檔繼續錄影，並更新 GUI 上的 `[● REC 3K]` 燈號。

### 5.3 分段存檔命名與循環覆寫 (`storage_mgr.rs`)

* **目錄與檔名格式**：
  透過 `splitmuxsink` 的 `format-location` callback 依日期自動分資料夾存放：
  `/home/user/Foscam_Recordings/2026-11-08/foscam_20261108_155000.mp4`
* **自動清理演算法**：
  * 背景 Task 每 5 分鐘統計 `record_dir` 下所有 `.mp4` 總容量。
  * 若總容量超過 `max_storage_gb`（例如 $100\text{ GB}$），按檔案修改時間由舊到新依序刪除，直到容量降回 $90\%$ 水位以下。

---

## 6. Ubuntu 環境建置與終端機先行驗證

### Step 1：安裝系統依賴套件

```bash
sudo apt update
sudo apt install -y \
    build-essential pkg-config \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav gstreamer1.0-vaapi gstreamer1.0-tools
```

### Step 2：在終端機驗證「方案 B」的兩條獨立管線

拿到 Foscam C5M 後，可以開兩個終端機視窗分別測試這兩條管線是否運作順暢：

**終端機 1 — 測試 3K 主碼流無損分段錄影（不解碼）：**
```bash
gst-launch-1.0 -e \
  rtspsrc location="rtsp://帳號:密碼@192.168.1.101:88/videoMain" protocols=tcp latency=500 ! \
  rtph264depay ! h264parse ! \
  splitmuxsink location="record_3k_%03d.mp4" max-size-time=60000000000
```
*(觀察 `htop`，你會發現這條 3K 錄影指令的 CPU 使用率幾乎是 $0\%$！按 `Ctrl+C` 結束後即可點開 `.mp4` 檢查 3K 畫質。)*

**終端機 2 — 測試 720p 副碼流低負載即時預覽：**
```bash
gst-launch-1.0 \
  rtspsrc location="rtsp://帳號:密碼@192.168.1.101:88/videoSub" protocols=tcp latency=200 ! \
  rtph264depay ! h264parse ! avdec_h264 ! videoconvert ! autovideosink sync=false
```

---

## 7. 開發里程碑 (Milestones)

* [ ] **Milestone 1：終端機雙碼流驗證**
  設定好 Foscam C5M 固定 IP 與帳密，使用上方兩條 `gst-launch-1.0` 指令確認 `/videoMain` 存檔與 `/videoSub` 預覽同時執行無誤。
* [ ] **Milestone 2：預覽管線 (`viewer.rs`) + Iced 視窗打通**
  將 `/videoSub` 的 $720\text{p}$ 畫面透過 `AppSink` 與 `flume` channel 顯示在 Iced 視窗中。
* [ ] **Milestone 3：加入 3K 錄影管線 (`recorder.rs`) 與錄影控制**
  在背景啟動 `/videoMain` 的 `splitmuxsink` 分段存檔，並在 GUI 下方加入錄影狀態顯示與「暫停/開始錄影」按鈕。
* [ ] **Milestone 4：斷線自動重連與硬碟空間循環清理 (`storage_mgr.rs`)**
  完成拔網路線重連測試與自動刪除最舊 MP4 檔案機制。