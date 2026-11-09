# Ubuntu Rust NVR 監控錄影系統 (`rnvr`)

專為 **Ubuntu Linux 桌面環境** 與 **Foscam C5M (3K 5MP)** 網路攝影機打造的超低負載桌面監控與循環錄影 GUI 系統。

本系統採用 **「單機雙碼流分離架構 (Dual-Stream Split Architecture)」**，將高畫質錄影與即時預覽徹底解耦，在一般迷你主機或舊筆電上也能以極低的 CPU 資源實現 **$3\text{K}$ 無損循環錄影** 與 **$720\text{p}$ 流暢即時預覽**。

---

## 目錄

1. [核心設計與軟體架構](#1-核心設計與軟體架構)
2. [攝影機規格與前置設定 (Foscam C5M)](#2-攝影機規格與前置設定-foscam-c5m)
3. [專案目錄與模組職責](#3-專案目錄與模組職責)
4. [安裝與跨機部署說明 (Ubuntu & Windows `.exe`)](#4-安裝與跨機部署說明)
5. [設定檔詳解 (`config.toml`)](#5-設定檔詳解-configtoml)
6. [軟體使用說明](#6-軟體使用說明)
7. [終端機先行驗證與常見問題排查](#7-終端機先行驗證與常見問題排查)

---

## 1. 核心設計與軟體架構

### 1.1 主副碼流分離架構 (Dual-Stream Split Architecture)

系統同時運作兩條完全獨立的 GStreamer 多媒體管線（Pipeline），彼此互不干擾：

* **錄影管線 (Recorder — Pipeline 1)**：
  拉取攝影機 $3\text{K}$ 主碼流 (`/videoMain`, $3072 \times 1728$ @ 25fps)，**完全不經過 CPU/GPU 解碼 (Zero-Decode Pass-through)**，直接將 H.264 視訊封包與 AAC 音訊封包無損封裝寫入硬碟 `.mp4` 切片檔（CPU 佔用率 $< 1\%$）。
* **預覽管線 (Viewer — Pipeline 2)**：
  拉取攝影機 $720\text{p}$ 副碼流 (`/videoSub`, $1280 \times 720$ @ 15fps)，僅針對低解析度畫面進行解碼並轉為 RGBA 畫面幀送交 Iced GUI 渲染，省去 $3\text{K} \rightarrow 720\text{p}$ 高達 $5.76$ 倍像素量的解碼與縮放負擔。

```text
                   ┌──> [Pipeline 1: 3K 錄影專線 (Zero-Decode Pass-through)]
                   │      rtsp://<IP>:88/videoMain
                   │        └──> rtspsrc (protocols=tcp, latency=500)
                   │        ├──> rtph264depay -> h264parse (config-interval=-1) ──┐
                   │        └──> rtpmp4gdepay -> aacparse (選用 AAC 音軌) ────────┼──> splitmuxsink
                   │                                                              │   (依日期分資料夾、預設每 30 分鐘切檔)
[Foscam C5M] ──────┤
 (單機雙碼流)      │
                   └──> [Pipeline 2: 720p 預覽專線 (HW Accel + Frame Throttling)]
                          rtsp://<IP>:88/videoSub
                            └──> rtspsrc (protocols=tcp, latency=200)
                            └──> rtph264depay -> h264parse -> vah264dec / vaapih264dec (自動降級 avdec_h264)
                            └──> videoconvert (RGBA) -> appsink (前端 FPS 節流 + O(1) Zero-Copy Bytes)
                                        │
                                 (flume 事件驅動 Subscription)
                                        ▼
                            [Iced 輕量 2D (tiny-skia) 主畫面 GUI 渲染]
```

### 1.2 核心技術棧 (Tech Stack)

| 系統層級 | 選用 Crate / 套件 | 版本 | 職責說明 |
| :--- | :--- | :--- | :--- |
| **GUI 框架** | `iced` | `0.13` | 純 Rust Elm 架構，提供單一 16:9 監控主畫面與底部狀態控制面板 |
| **多媒體管線** | `gstreamer` (`gstreamer-rs`) | `0.23` | 獨立管理「3K 錄影」與「720p 預覽」兩條 GStreamer Pipeline |
| **視訊幀擷取** | `gstreamer-app`, `gstreamer-video` | `0.23` | 透過 `AppSink` 擷取 `/videoSub` 解碼後的 $1280 \times 720$ RGBA 畫面幀 |
| **非同步執行期** | `tokio` | `1.x` | 負責背景硬碟空間巡檢、自動刪除舊檔 (Retention Policy) |
| **跨執行緒通訊** | `flume` | `0.11` | 高效能無鎖 Channel，傳遞預覽畫面幀、錄影狀態與控制指令 |
| **設定檔解析** | `serde`, `toml` | `1.0` / `0.8` | 解析 `config.toml` 攝影機連線資訊與循環錄影參數 |
| **截圖輸出** | `png` | `0.17` | 將預覽畫面即時編碼儲存為無損 PNG 截圖 |

### 1.3 關鍵可靠性機制

1. **故障隔離 (Fault Isolation)**：錄影管線與 GUI 預覽管線完全脫鉤，調整視窗大小或預覽重連皆不影響背景正在寫入的 $3\text{K}$ `.mp4` 錄影檔。
2. **MP4 檔尾 (`moov` atom) 完整性保護**：當網路閃斷、使用者按下「暫停錄影」或關閉視窗時，錄影管線會主動發送 `EOS` 訊號並等待 `splitmuxsink` 完整寫入 MP4 `moov` 檔尾後才釋放資源，確保已錄製的影片絕不損毀。
3. **強制 TCP 傳輸與 5 秒自動重連**：兩條管線的 `rtspsrc` 皆強制使用 TCP (`protocols=tcp`) 避免 UDP 掉包破圖；偵測到網路中斷或串流逾時後，每 **5 秒**自動重試連線，恢復後自動建立新切片檔繼續錄影。
4. **依日期歸檔、Cron 定時巡檢與雙重循環覆寫保護**：
   * **依日期歸檔與單檔時間自訂**：錄影檔自動依日期建立子資料夾 `<record_dir>/YYYY-MM-DD/foscam_YYYYMMDD_HHMMSS.mp4`，預設每 **30 分鐘**切分一個 MP4 檔（可於 GUI 自訂分鐘數）。
   * **Cron 排程巡檢**：內建標準 5 欄位 Cron 排程器（預設 `"0 3 * * 2"`，即**每週二凌晨 03:00** 自動巡檢，亦可於 GUI 自訂排程或手動立即巡檢）。
   * **雙階段清理機制**：
     1. **過期天數清理**：自動刪除超過設定保留天數（預設 **30 天**，`retention_days = 30`）的舊 `.mp4` 錄影檔與空日期資料夾。
     2. **容量水位保護**：若刪除過期檔案後總容量仍超過設定上限 (`max_storage_gb`)，自動依修改時間**由舊到新**刪除，直到總容量降回上限的 **$90\%$ 水位以下**（並自動保護正在寫入的最新檔案）。

### 1.4 低耗能與硬體加速優化機制 (Low-Power & CPU Optimization)

為了讓 `rnvr` 安裝在舊款電腦、文書機、迷你主機（Mini-PC）或工控機上長時間運作時依然維持極低功耗與發熱量，系統內建五層全方位省電優化：

1. **預設啟用輕量 2D 渲染後端 (`tiny-skia`)，徹底避開 `llvmpipe` 陷阱**：
   * 在許多無獨立顯卡或舊款內顯（如 Intel 第 3～7 代內顯）的 Linux 主機上，若使用預設 Vulkan 3D 後端，`wgpu` 常因硬體不具備完整 Vulkan 1.3 規範而退回 Mesa `llvmpipe`（使用 CPU 跑 LLVM 向量指令來軟體模擬整套 3D Vulkan Shader 與 2048×2048 貼圖集），導致 CPU 瞬間飆破 100%。
   * `rnvr` 啟動時自動選用輕量 2D 渲染後端 (`ICED_BACKEND=tiny-skia`) 搭配 `FilterMethod::Nearest` 快速像素縮放，完全不觸發 3D 軟體模擬，亦支援透過 `--renderer <tiny-skia|vulkan|gl>` 手動指定。
2. **廣域硬體 H.264 解碼加速與自動平滑降級**：
   * 預覽管線優先自動偵測並啟用硬體解碼器（依序為 `vah264dec` $\rightarrow$ `vaapih264dec` $\rightarrow$ `nvh264dec` $\rightarrow$ `v4l2slh264dec` $\rightarrow$ `v4l2h264dec`），將 720p H.264 解碼工作交由內顯（如 Intel `i965` / `iHD` VA-API）處理。
   * 若目標電腦無硬體解碼器或驅動初始化異常，系統會自動無縫降級回 CPU 解碼 (`avdec_h264`)，並自動限制雙執行緒 (`max-threads=2`) 避免塞滿多核心 CPU。
3. **事件驅動 (Event-Driven) GUI 架構，消除 30Hz 空轉重繪**：
   * 廢除傳統每 33ms (30Hz) 固定輪詢重繪的做法，改以 `iced::Subscription` 非同步串流監聽預覽幀與管線狀態，搭配 1Hz 時鐘更新。當無新畫面幀時，GUI 主執行緒完全不進行 `view()` 建構與中文字型排版。
4. **O(1) Zero-Copy 影像記憶體共享與前端幀率節流 (`preview_max_fps`)**：
   * `AppSink` 取幀後直接封裝為引用計數的 `iced::widget::image::Handle` (`bytes::Bytes`)，傳遞給 GUI 渲染與截圖功能時皆為 **O(1) 零記憶體複製 (Zero-Copy)**，消除每幀 3.68 MB 的二次 Heap 配置與 `memcpy`。
   * 內建預覽幀率上限節流（預設 `preview_max_fps = 12`），在像素轉換前即過濾超額畫面幀並移除多餘的 CPU `videoscale` 運算。
5. **一鍵「暫停預覽(省電)」純 3K 背景錄影模式**：
   * 監控主機 24 小時運作時，多數時間不需人員盯著螢幕。點擊底部控制列的 **`[ 暫停預覽(省電) ]`** 按鈕後，系統會立即關閉並釋放 720p 預覽管線（預覽執行緒進入阻塞休眠，耗電量 `0%`），僅保留 Zero-Decode 的 **3K 主碼流無損錄影管線**持續運作，使整機 `rnvr` CPU 使用率降至 **$1\%$ 以下**；需要監看時點擊 **`[ 恢復預覽畫面 ]`** 即可秒速恢復。

#### 實機效能實測對比（Intel Core i5-3340S + Intel HD 2500 內顯 + Foscam C5M 雙碼流）

| 運作模式 / 指標 | 優化前 (預設 `wgpu` + 30Hz 輪詢) | 優化後 (3K 錄影 + 720p 預覽同時開啟) | 優化後 (啟用「暫停預覽(省電)」純 3K 錄影) |
| :--- | :--- | :--- | :--- |
| **圖形渲染與解碼器** | `llvmpipe` (CPU 3D 模擬) + `avdec_h264` | `tiny-skia` (2D) + `vah264dec` (VA-API 硬體解碼) | 預覽管線休眠 + 3K Zero-Decode 直寫 |
| **單核等效 CPU 使用率** | **~115%** (約佔 4 核總算力 29%) | **~20%** (約佔 4 核總算力 **5%**) | **< 1% ~ 2%** |
| **記憶體峰值 (RSS Peak)** | `156.7 MB` | `108.4 MB` | `< 65 MB` |

---

## 2. 攝影機規格與前置設定 (Foscam C5M)

在啟動軟體前，請先於路由器與 Foscam 後台完成以下設定：

* **固定區網 IP**：建議於路由器 DHCP 綁定攝影機 MAC 位址為固定 IP（例如 `192.168.1.101`）。
* **視訊編碼**：確認設定為標準 **`H.264`**。
* **音訊編碼**：請於攝影機後台將音訊格式設為 **`AAC`**，讓錄影管線可將音軌與 H.264 視訊直接無損封裝進 `.mp4` 容器（亦可於 `config.toml` 中透過 `enable_audio = false` 關閉錄音）。
* **RTSP 連接埠**：預設為 `88`（或標準 `554`，依攝影機韌體設定為準）。
* **雙碼流路徑**：
  * **主碼流（3K 錄影專用）**：`rtsp://<username>:<password>@<IP>:88/videoMain`
  * **副碼流（720p 預覽專用）**：`rtsp://<username>:<password>@<IP>:88/videoSub`

---

## 3. 專案目錄與模組職責

```text
rnvr/
├── Cargo.toml             # Rust 專案依賴與 [package.metadata.deb] 打包配置
├── config.toml            # 攝影機連線參數與循環錄影設定檔
├── config.example.toml    # 設定檔預設範本 (供 cargo deb 封裝與版控使用)
├── install.sh             # Ubuntu 跨機一鍵安裝與桌面捷徑部署腳本
├── rnvr.md                # 原始系統架構規劃書
├── README.md              # 軟體架構、安裝與使用說明文件
├── assets/
│   ├── rnvr.desktop       # Ubuntu 桌面應用程式捷徑設定檔
│   ├── rnvr.png           # 256x256 專屬 NVR 監控鏡頭 + REC 紅燈圖示 (PNG)
│   └── rnvr.svg           # 可縮放向量圖示 (SVG)
├── debian/
│   └── postinst           # .deb 安裝後自動更新系統圖示快取與桌面資料庫腳本
└── src/
    ├── main.rs            # 程式進入點：CLI 解析、自動初始化設定檔、啟動 Iced 視窗
    ├── config.rs          # Serde 設定檔結構定義與 RTSP URL 編碼組裝
    ├── icon.rs            # 視窗與標題列專屬 NVR 攝影機圖示渲染器
    ├── app.rs             # Iced GUI 主畫面 (頂部狀態列 + 16:9 視訊區 + 系統與連線設定面板)
    ├── recorder.rs        # [Pipeline 1] /videoMain 3K 無損分段錄影、AAC 音軌封裝與斷線重連
    ├── viewer.rs          # [Pipeline 2] /videoSub 720p 解碼預覽、AppSink 取幀與 FPS 計算
    └── storage_mgr.rs     # 空間管理員：Cron 排程解析、30 天過期清理與 90% 水位保護
```

---

## 4. 安裝與跨機部署說明

### 方式 A：使用 `cargo deb` 產生 `.deb` 安裝包跨機安裝（最推薦）

本專案已在 `Cargo.toml` 中完整設定 `[package.metadata.deb]`（包含 `$auto` 動態函式庫依賴解析、GStreamer 插件與中文字型依賴、`conf-files` 設定保護、系統圖示與桌面捷徑）：

1. **在專案根目錄執行 `cargo deb` 打包**（若新環境尚未安裝 `cargo-deb`，可先執行 `cargo install cargo-deb`）：
   ```bash
   cargo deb
   ```
   執行完成後會自動在 `target/debian/` 目錄下產出標準安裝包：
   `target/debian/rnvr_1.0.0-1_amd64.deb`

2. **將單一 `.deb` 檔案複製到另一台 Ubuntu 主機並安裝**：
   ```bash
   scp target/debian/rnvr_1.0.0-1_amd64.deb user@<目標主機IP>:~/
   ssh user@<目標主機IP>
   sudo apt install ./rnvr_1.0.0-1_amd64.deb
   ```
   > 使用 `sudo apt install ./rnvr_1.0.0-1_amd64.deb` 時，Ubuntu 的 `apt` 會**全自動下載並安裝**所有必要的 GStreamer 插件、**Intel 新舊世代與 AMD 的 VA-API 硬體加速解碼驅動 (`va-driver-all`, `i965-va-driver`, `intel-media-va-driver`, `gstreamer1.0-vaapi`)** 與 `fonts-noto-cjk` 繁體中文字型，並自動註冊系統圖示與桌面選單捷徑 **「Foscam Rust NVR 監控系統」**。初次啟動時程式會自動於該使用者的 `~/.config/rnvr/config.toml` 建立可讀寫設定檔。
   >
   > **移除套件指令**：`sudo apt remove rnvr`

3. **Ubuntu 檔案資料夾 (Nautilus) 執行檔自訂圖示補充說明**：
   * 若希望在本機開發目錄的 Ubuntu 檔案管理員中讓 `target/release/rnvr` 二進位檔直接顯示自訂攝影機圖示，可執行：
     ```bash
     gio set target/release/rnvr metadata::custom-icon "file://$(pwd)/assets/rnvr.png"
     ```

---

### 方式 B：直接複製資料夾透過 `install.sh` 安裝

若直接複製整個專案資料夾至目標主機，亦可執行 `install.sh`：

1. **將整個專案資料夾複製到目標 Ubuntu 主機**：
   ```bash
   scp -r /home/ubuntu/ws_rust/rnvr user@<目標主機IP>:~/rnvr
   ```
2. **在目標 Ubuntu 主機執行安裝腳本**：
   ```bash
   cd ~/rnvr
   chmod +x install.sh
   ./install.sh
   ```
   `install.sh` 會自動完成以下四個步驟：
   * **[1/4]** 自動透過 `apt` 安裝所需的 GStreamer 執行期插件（`base`, `good`, `bad`, `ugly`, `libav`, `vaapi`）。
   * **[2/4]** 將編譯好的 `target/release/rnvr` 安裝至 `/usr/local/bin/rnvr`（若無預編譯檔則自動呼叫 `cargo build --release` 編譯）。
   * **[3/4]** 於 `~/.config/rnvr/config.toml` 建立個人化設定檔，並建立預設錄影目錄 `~/Foscam_Recordings`。
   * **[4/4]** 建立 Ubuntu 桌面應用程式捷徑 `~/.local/share/applications/rnvr.desktop`（可在 Ubuntu 應用程式選單直接找到 **「Foscam Rust NVR 監控系統」**），並執行 `rnvr --check-env` 驗證所有多媒體插件。

---

### 方式 C：手動安裝依賴與從原始碼編譯

若您希望在新主機上從頭手動安裝與編譯：

1. **安裝 Ubuntu 系統與 GStreamer 套件（含 VA-API 硬體解碼驅動）**：
   ```bash
   sudo apt update
   sudo apt install -y \
       build-essential pkg-config \
       libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
       gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
       gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
       gstreamer1.0-libav gstreamer1.0-vaapi gstreamer1.0-tools \
       va-driver-all i965-va-driver intel-media-va-driver vainfo \
       fonts-noto-cjk xdg-utils
   ```
2. **編譯 Release 版本**：
   ```bash
   cargo build --release
   ```
3. **檢查 GStreamer 插件與硬體解碼環境完整性**：
   ```bash
   ./target/release/rnvr --check-env
   ```
   正常輸出如下：
   ```text
   === GStreamer 雙碼流管線環境檢查 ===
     [OK] rtspsrc          (來自 gstreamer1.0-plugins-good)
     [OK] rtph264depay     (來自 gstreamer1.0-plugins-good)
     [OK] rtpmp4gdepay     (來自 gstreamer1.0-plugins-good)
     [OK] splitmuxsink     (來自 gstreamer1.0-plugins-good)
     [OK] h264parse        (來自 gstreamer1.0-plugins-bad)
     [OK] aacparse         (來自 gstreamer1.0-plugins-good)
     [OK] avdec_h264       (來自 gstreamer1.0-libav)
     [OK] videoconvert     (來自 gstreamer1.0-plugins-base)
     [OK] videoscale       (來自 gstreamer1.0-plugins-base)
     [OK] appsink          (來自 gstreamer1.0-plugins-base)

   === 硬體 H.264 解碼加速支援檢查 ===
     [OK] vah264dec        (已就緒！預覽管線將優先使用 VA-API (gst-plugins-bad va) 硬體解碼)

   === 所有必要 GStreamer 插件皆已就緒！ ===
   ```

---

### 方式 D：編譯為 Windows 執行檔 (`rnvr.exe`)

本專案程式碼已內建 Windows 跨平台相容處理（Windows 下自動切換為微軟正黑體 `Microsoft JhengHei`、使用檔案總管 `explorer` 開啟錄影資料夾、支援 `%USERPROFILE%` 家目錄，並在 Release 模式下透過 `windows_subsystem = "windows"` 自動隱藏背後的黑色 CMD 視窗）。

您可以選擇在 Windows 本機使用 **官方 MSVC 工具鏈** 或 **MSYS2 MinGW64** 編譯出 `rnvr.exe`：

#### 方法 1：於 Windows 原生 MSVC 工具鏈編譯（最推薦）

1. **安裝 Rust 與 Visual Studio C++ Build Tools**：
   * 前往 [rustup.rs](https://rustup.rs/) 下載並執行 `rustup-init.exe`，維持預設的 `x86_64-pc-windows-msvc` 工具鏈（若系統尚未安裝 Visual Studio C++ Build Tools，安裝程式會引導自動安裝）。
   * 安裝 `pkg-config` 工具（可透過 Scoop 或 Chocolatey 安裝，例如 `scoop install pkg-config` 或 `choco install pkgconfiglite`）。

2. **安裝 GStreamer Windows 官方 SDK (MSVC 64-bit)**：
   * 前往 [GStreamer 官方下載頁](https://gstreamer.freedesktop.org/download/#windows)，下載 **MSVC 64-bit (VS 2019, Release CRT)** 的**兩個** `.msi` 安裝檔：
     1. **Runtime installer**（例如 `gstreamer-1.0-msvc-x86_64-1.24.x.msi`）
     2. **Development installer**（例如 `gstreamer-1.0-devel-msvc-x86_64-1.24.x.msi`）
   * ⚠️ **安裝時請務必選擇「Complete (完整安裝)」**，以確保包含 `rtspsrc`、`splitmuxsink` 與 `avdec_h264` (`gst-libav`) 等所有必要插件。

3. **設定環境變數並編譯 `rnvr.exe`（以 PowerShell 為例）**：
   假設 GStreamer 安裝於預設路徑 `C:\gstreamer\1.0\msvc_x86_64`（請依實際安裝路徑調整）：
   ```powershell
   # 設定 GStreamer 根目錄與 pkg-config 搜尋路徑
   $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = "C:\gstreamer\1.0\msvc_x86_64"
   $env:PKG_CONFIG_PATH = "$env:GSTREAMER_1_0_ROOT_MSVC_X86_64\lib\pkgconfig"
   $env:Path = "$env:GSTREAMER_1_0_ROOT_MSVC_X86_64\bin;" + $env:Path

   # 編譯 Release 版本 Windows 執行檔
   cargo build --release
   ```
   編譯完成後即可在 `target\release\rnvr.exe` 取得執行檔。

#### 方法 2：於 Windows 使用 MSYS2 (MinGW64) 一鍵安裝依賴與編譯

若習慣使用套件管理員自動配置依賴與 `pkg-config` 路徑，可透過 [MSYS2](https://www.msys2.org/) 的 **MSYS2 MINGW64** 終端機視窗快速建立編譯環境：

1. **在 MSYS2 MINGW64 視窗安裝編譯工具與 GStreamer 套件**：
   ```bash
   pacman -Syu
   pacman -S --needed \
       mingw-w64-x86_64-toolchain \
       mingw-w64-x86_64-rust \
       mingw-w64-x86_64-pkgconf \
       mingw-w64-x86_64-gstreamer \
       mingw-w64-x86_64-gst-plugins-base \
       mingw-w64-x86_64-gst-plugins-good \
       mingw-w64-x86_64-gst-plugins-bad \
       mingw-w64-x86_64-gst-plugins-ugly \
       mingw-w64-x86_64-gst-libav
   ```
2. **在專案目錄執行編譯**：
   ```bash
   cargo build --release
   ```
   產出檔案同樣位於 `target/release/rnvr.exe`。

#### 方法 3：於 Ubuntu Linux 交叉編譯 Windows `.exe` (`x86_64-pc-windows-gnu`)

若希望在 Ubuntu 主機上直接交叉編譯出 Windows `rnvr.exe`，可按照以下步驟準備 MinGW-w64 工具鏈與 Windows 版 GStreamer (MinGW 64-bit) 開發庫：

1. **安裝 Rust Windows 目標平台與 MinGW 工具鏈**：
   ```bash
   sudo apt update && sudo apt install -y mingw-w64 pkg-config zstd curl
   rustup target add x86_64-pc-windows-gnu
   ```
2. **下載並解壓縮 Windows GStreamer (MinGW 64-bit) 開發套件至 `/opt/gst-win64`**：
   ```bash
   sudo mkdir -p /opt/gst-win64 && sudo chown -R $USER:$USER /opt/gst-win64
   cd /tmp
   for pkg in \
     mingw-w64-x86_64-gettext-runtime-0.24-1-any.pkg.tar.zst \
     mingw-w64-x86_64-libffi-3.4.7-1-any.pkg.tar.zst \
     mingw-w64-x86_64-pcre2-10.45-1-any.pkg.tar.zst \
     mingw-w64-x86_64-zlib-1.3.1-1-any.pkg.tar.zst \
     mingw-w64-x86_64-orc-0.4.41-1-any.pkg.tar.zst \
     mingw-w64-x86_64-glib2-2.82.5-1-any.pkg.tar.zst \
     mingw-w64-x86_64-gstreamer-1.24.12-1-any.pkg.tar.zst \
     mingw-w64-x86_64-gst-plugins-base-1.24.12-1-any.pkg.tar.zst; do
     curl -sLO "https://mirror.msys2.org/mingw/mingw64/$pkg"
     tar --zstd -xf "$pkg" --strip-components=1 -C /opt/gst-win64 mingw64/
   done
   sed -i 's|^prefix=.*|prefix=/opt/gst-win64|g' /opt/gst-win64/lib/pkgconfig/*.pc
   ```
3. **指定 `PKG_CONFIG_LIBDIR` 並執行交叉編譯**：
   ```bash
   cd /home/ubuntu/ws_rust/rnvr
   export PKG_CONFIG_ALLOW_CROSS=1
   export PKG_CONFIG_LIBDIR="/opt/gst-win64/lib/pkgconfig"

   cargo build --release --target x86_64-pc-windows-gnu
   ```
   編譯完成後即可於 `target/x86_64-pc-windows-gnu/release/rnvr.exe` 取得 Windows 執行檔。

#### Windows 執行與跨機部署注意事項

* **首次啟動與設定檔位置**：
  * 可將 `config.example.toml` 複製到與 `rnvr.exe` 同一資料夾並改名為 `config.toml`，將 `record_dir` 改為 Windows 路徑格式（例如 `record_dir = "D:/Foscam_Recordings"` 或 `"C:\\Users\\您的帳號\\Foscam_Recordings"`）。
  * 若同目錄下無 `config.toml`，程式啟動時會自動在 `%USERPROFILE%\.config\rnvr\config.toml` 建立設定檔，並預設將錄影存於 `%USERPROFILE%\Foscam_Recordings`。
* **跨機複製 `rnvr.exe` 到其他 Windows 電腦**：
  * 目標 Windows 電腦只需安裝對應的 GStreamer 官方 **Runtime installer (`.msi`)**（安裝時選擇 **Complete**，並確認 GStreamer 的 `bin` 目錄如 `C:\gstreamer\1.0\msvc_x86_64\bin` 已加入系統 `PATH` 環境變數），即可直接雙擊執行 `rnvr.exe`。
* **在 Windows PowerShell 檢視環境檢查或除錯輸出**：
  * 由於 Release 版的 `rnvr.exe` 預設隱藏背景主控台視窗 (`windows_subsystem = "windows"`)，若需在 PowerShell 檢視 `--check-env` 或除錯日誌，可將輸出導向管線，或在開發時使用 `cargo run`：
    ```powershell
    .\target\release\rnvr.exe --check-env | Out-Host
    ```

---

## 5. 設定檔詳解 (`config.toml`)

程式啟動時會依序搜尋以下位置的設定檔：
1. 命令列 `--config <PATH>` 指定之路徑
2. 當前工作目錄下的 `./config.toml`
3. 使用者家目錄下的 `~/.config/rnvr/config.toml`
4. 系統目錄 `/etc/rnvr/config.toml`

設定檔內容與各參數說明如下：

```toml
[camera]
name = "大門監控 (Foscam C5M)"    # 顯示於 GUI 視窗標題與頂部狀態列的名稱
ip = "192.168.1.101"             # Foscam C5M 的固定區網 IP
port = 88                        # RTSP 連接埠 (Foscam 預設為 88 或 554)
username = "nvr_admin"           # 攝影機登入帳號 (支援特殊字元，系統會自動 URL Encode)
password = "your_password"       # 攝影機登入密碼
main_stream_path = "/videoMain"  # 3K 主碼流路徑 (錄影專用)
sub_stream_path = "/videoSub"    # 720p 副碼流路徑 (預覽專用)
enable_audio = true              # 是否同時無損封裝 AAC 音訊軌至 MP4
use_hw_accel = true              # 是否優先使用硬體加速解碼預覽 (vah264dec / vaapih264dec，若無則自動退回 avdec_h264)
preview_max_fps = 12             # 預覽畫面最大更新率上限 (預設 12 FPS，降低舊電腦 CPU 與顯示卡負載)
preview_enabled = true           # 是否開啟即時預覽畫面 (設為 false 可進入純 3K 背景錄影省電模式)
mock_mode = false                # 測試模式：設為 true 時使用 GStreamer 內建訊號源模擬雙碼流

[storage]
record_dir = "/home/ubuntu/Foscam_Recordings" # 錄影檔案與截圖儲存根目錄
segment_duration_mins = 30       # 單一 MP4 檔案錄影時間 (預設 30 分鐘，可於 GUI 自訂)
retention_days = 30              # 錄影檔案保留天數 (預設超過 30 天自動刪除，可於 GUI 自訂)
cleanup_cron = "0 3 * * 2"       # 磁碟巡檢 Cron 排程 (分 時 日 月 週，預設每週二 03:00 檢查)
max_storage_gb = 100             # 錄影資料夾最大容量上限 (單位: GB)
auto_start_recording = true      # 啟動軟體時是否立即自動開始 3K 錄影
```

---

## 6. 軟體使用說明

### 6.1 啟動方式與命令列參數

* **一般啟動**（連接實體攝影機）：
  ```bash
  rnvr
  ```
* **指定設定檔啟動**：
  ```bash
  rnvr --config ~/.config/rnvr/config.toml
  ```
* **指定圖形渲染後端啟動**（預設自動啟用輕量 2D `tiny-skia`，亦可手動指定 `vulkan` 或 `gl`）：
  ```bash
  rnvr --renderer tiny-skia
  ```
* **模擬測試模式啟動**（無須連接實體攝影機，自動產生 SMPTE / 動態球測試畫面驗證 720p 預覽與 MP4 分段錄影）：
  ```bash
  rnvr --mock
  ```
* **檢查環境插件與硬體解碼支援**：
  ```bash
  rnvr --check-env
  ```

### 6.2 GUI 介面導覽與操作

```text
┌────────────────────────────────────────────────────────────────────────┐
│ [📷] [● REC 3K] 大門監控 (Foscam C5M) (192.168.1.101:88)  2026-11-08 18:00 │
├────────────────────────────────────────────────────────────────────────┤
│                                                                        │
│                                                                        │
│                       1280 x 720 即時預覽畫面區                          │
│           (自動等比例縮放填滿視窗，滑鼠左鍵連點兩下切換全螢幕)           │
│                                                                        │
│                                                                        │
├────────────────────────────────────────────────────────────────────────┤
│ 預覽狀態: 連線正常 (11.0 FPS, vah264dec)  |  錄影狀態: 寫入中 (foscam_...mp4) │
│ 磁碟空間: [████████░░] 52.40 / 100.0 GB | 單檔: 30分 | 保留: 30天 | 下次 Cron 巡檢: 2026-11-10 03:00 │
│ [ 暫停 3K 錄影 ] [ 暫停預覽(省電) ] [ 畫面截圖 ] [ 全螢幕預覽 ] [ 開啟錄影資料夾 ] [ 立即巡檢清理 ] [ 系統與連線設定 ] │
└────────────────────────────────────────────────────────────────────────┘
```

* **全螢幕預覽模式 (Fullscreen)**：
  * **進入全螢幕**：在中央預覽畫面處**滑鼠左鍵連續點擊兩下 (Double-Click)**、點擊底部 **`[ 全螢幕預覽 ]`** 按鈕，或按下鍵盤 **`F11`**，視窗即會切換為全螢幕並自動隱藏頂部與底部工具列，讓監控畫面完整填滿整個螢幕。
  * **退出全螢幕**：在全螢幕畫面處再次**滑鼠左鍵連續點擊兩下**，或按下鍵盤 **`Esc`** / **`F11`**，即可立即恢復一般視窗與操作面板。
* **頂部狀態列**：
  * **`[● REC 3K]`**（紅燈）：代表背景 3K 主碼流正在正常寫入硬碟。
  * **`[● 連線錄影中]`**（黃燈）：代表錄影功能已啟用，正在連線或等待自動重連。
  * **`[○ 錄影待命]`**（灰燈）：代表使用者已手動暫停錄影。
* **底部控制按鈕**：
  * **`[ 暫停 3K 錄影 ]` / `[ 開始 3K 錄影 ]`**：隨時切換背景 3K 錄影狀態。按下暫停時會安全封裝當前 MP4 切片檔尾；再次開始時會建立新的時間戳記切片檔。
  * **`[ 暫停預覽(省電) ]` / `[ 恢復預覽畫面 ]`**：一鍵切換「省電純錄影模式」。暫停預覽時會釋放 720p 解碼管線與副碼流連線，使背景 3K 錄影時的 CPU 負載降至 $< 1\%$；點擊恢復即可隨時重新監看。
  * **`[ 畫面截圖 ]`**：將當前預覽畫面儲存為無損 PNG 圖檔，存放於 `<record_dir>/screenshots/snapshot_YYYYMMDD_HHMMSS.png`。
  * **`[ 全螢幕預覽 ]`**：一鍵進入全螢幕監控模式（亦可直接於預覽畫面連點兩下滑鼠左鍵或按 `F11`）。
  * **`[ 開啟錄影資料夾 ]`**：直接呼叫系統檔案管理員開啟 `<record_dir>` 檢視歷史錄影與截圖。
  * **`[ 立即巡檢清理 ]`**：立即手動觸發一次硬碟空間統計、超過保留天數過期檔案刪除與 $90\%$ 水位清理檢查。
  * **`[ 系統與連線設定 ]`**：在軟體內直接展開設定面板，可即時修改並儲存：
    1. **攝影機連線與預覽效能參數**：
       * 攝影機名稱、IP 位址/網址、RTSP 埠號、登入帳號、密碼（支援顯示/隱藏）、3K 主碼流與 720p 副碼流路徑。
       * **預覽最大 FPS**：限制預覽畫面每秒最大更新幀數（預設 `12` FPS）。
       * **硬體解碼加速開關**：切換優先使用 VA-API 硬體加速解碼或純 CPU 軟體解碼。
    2. **錄影與磁碟排程參數**：
       * **單檔時間 (分)**：單一 MP4 錄影切片時間（預設 `30` 分鐘）。
       * **保留天數 (天)**：自動刪除超過指定天數的舊錄影檔（預設 `30` 天）。
       * **巡檢 Cron**：標準 5 欄位 Cron 排程（預設 `0 3 * * 2`，即每週二凌晨 03:00）。
       * **容量上限 (GB)**：錄影資料夾最大容量上限（預設 `100` GB）。
    * 點擊 **「儲存並套用設定」** 後，系統會即時驗證參數與 Cron 語法、寫入 `config.toml`，並立即通知背景錄影管線與空間管理員套用新設定（無須重啟軟體）。

---

## 7. 終端機先行驗證與常見問題排查

在將系統部署至新主機或剛接上 Foscam C5M 時，可使用以下 `gst-launch-1.0` 指令先行獨立測試網路與攝影機雙碼流：

### 7.1 測試 3K 主碼流無損分段錄影（不解碼）
```bash
gst-launch-1.0 -e \
  rtspsrc location="rtsp://帳號:密碼@192.168.1.101:88/videoMain" protocols=tcp latency=500 ! \
  rtph264depay ! h264parse ! \
  splitmuxsink location="record_3k_%03d.mp4" max-size-time=60000000000
```
> 按 `Ctrl+C` 結束後，會自動寫入 MP4 檔尾，可直接打開 `record_3k_000.mp4` 驗證 $3072 \times 1728$ 3K 畫質。

### 7.2 測試 720p 副碼流低負載即時預覽
```bash
gst-launch-1.0 \
  rtspsrc location="rtsp://帳號:密碼@192.168.1.101:88/videoSub" protocols=tcp latency=200 ! \
  rtph264depay ! h264parse ! avdec_h264 ! videoconvert ! autovideosink sync=false
```

### 7.3 常見問題 (FAQ)

1. **預覽或錄影一直顯示「連線中」或「斷線重連中」？**
   * 請確認 `config.toml` 中的 `ip`、`port`（Foscam 預設為 `88`）、`username` 與 `password` 是否正確。
   * 若密碼含有 `@`、`#`、`:` 等特殊字元，直接填入 `config.toml` 即可（程式內部已內建 URL Encoding 處理）。
2. **錄影檔案沒有聲音？**
   * 請登入 Foscam 攝影機後台，將音訊編碼格式改為 **`AAC`**（MP4 容器不支援直接無損封裝 G.711 PCMU/PCMA），並確認 `config.toml` 中 `enable_audio = true`。
3. **如何檢視詳細除錯日誌？**
   * 在終端機啟動時加上 `RUST_LOG=debug` 或 `GST_DEBUG=3` 環境變數：
     ```bash
     RUST_LOG=debug rnvr
     ```
