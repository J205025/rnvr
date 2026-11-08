#!/usr/bin/env bash
set -euo pipefail

echo "=========================================================="
echo "  Ubuntu Rust NVR (Foscam C5M) 一鍵環境安裝與部署腳本"
echo "=========================================================="

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "[1/4] 安裝 Ubuntu GStreamer 系統依賴套件..."
sudo apt-get update
sudo apt-get install -y \
    build-essential pkg-config \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav gstreamer1.0-tools \
    fonts-noto-cjk xdg-utils

# gstreamer1.0-vaapi 在部分 Ubuntu 版本為選用套件，嘗試安裝但不強制失敗
sudo apt-get install -y gstreamer1.0-vaapi || true

echo "[2/4] 準備執行檔..."
if [ -f "${SCRIPT_DIR}/target/release/rnvr" ]; then
    echo "偵測到已編譯好的 target/release/rnvr，直接安裝至 /usr/local/bin/rnvr"
    sudo install -m 755 "${SCRIPT_DIR}/target/release/rnvr" /usr/local/bin/rnvr
elif command -v cargo >/dev/null 2>&1; then
    echo "開始以 Release 模式編譯專案..."
    (cd "${SCRIPT_DIR}" && cargo build --release)
    sudo install -m 755 "${SCRIPT_DIR}/target/release/rnvr" /usr/local/bin/rnvr
else
    echo "錯誤：找不到預編譯的 target/release/rnvr 且系統未安裝 Rust (cargo)。"
    echo "請先在開發機執行 'cargo build --release' 後再連同整個資料夾複製過來，或先安裝 rustup。"
    exit 1
fi

echo "[3/4] 配置設定檔 (~/.config/rnvr/config.toml)..."
CONFIG_DIR="${HOME}/.config/rnvr"
mkdir -p "${CONFIG_DIR}"
if [ ! -f "${CONFIG_DIR}/config.toml" ]; then
    cp "${SCRIPT_DIR}/config.toml" "${CONFIG_DIR}/config.toml"
    # 自動將預設錄影目錄替換為當前使用者的家目錄
    sed -i "s|/home/ubuntu/Foscam_Recordings|${HOME}/Foscam_Recordings|g" "${CONFIG_DIR}/config.toml"
    echo "已建立設定檔: ${CONFIG_DIR}/config.toml"
else
    echo "保留既有設定檔: ${CONFIG_DIR}/config.toml"
fi

mkdir -p "${HOME}/Foscam_Recordings"

echo "[4/4] 建立 Ubuntu 桌面應用程式捷徑..."
APP_DIR="${HOME}/.local/share/applications"
mkdir -p "${APP_DIR}"
cat > "${APP_DIR}/rnvr.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Foscam Rust NVR 監控系統
Comment=Foscam C5M 單機雙碼流 (3K 錄影 + 720p 預覽) 監控錄影系統
Exec=/usr/local/bin/rnvr --config ${CONFIG_DIR}/config.toml
Icon=camera-video
Terminal=false
Categories=AudioVideo;Video;Recorder;
EOF
chmod +x "${APP_DIR}/rnvr.desktop"

echo ""
echo "驗證 GStreamer 插件環境..."
/usr/local/bin/rnvr --check-env

echo ""
echo "=========================================================="
echo "安裝完成！"
echo "1. 請編輯設定檔填入您的 Foscam C5M IP 與密碼："
echo "   nano ${CONFIG_DIR}/config.toml"
echo "2. 啟動軟體指令："
echo "   rnvr"
echo "3. 若尚未接上攝影機想測試畫面與分段錄影，可執行："
echo "   rnvr --mock"
echo "=========================================================="
