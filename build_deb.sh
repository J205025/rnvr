#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

PKG_NAME="rnvr"
PKG_VERSION="0.1.0"
ARCH="$(dpkg --print-architecture 2>/dev/null || echo "amd64")"
DEB_NAME="${PKG_NAME}_${PKG_VERSION}_${ARCH}"
STAGE_DIR="${SCRIPT_DIR}/target/debian/${DEB_NAME}"
DIST_DIR="${SCRIPT_DIR}/dist"
ICON_URI="file://${SCRIPT_DIR}/assets/rnvr.png"

echo "=========================================================="
echo "  Ubuntu Rust NVR (.deb 安裝包建置與檔案總管圖示設定)"
echo "=========================================================="

echo "[1/4] 編譯 Release 執行檔..."
cargo build --release

echo "[2/4] 為編譯完的執行檔設定 Ubuntu 檔案資料夾 (Nautilus) 自訂圖示..."
if command -v gio >/dev/null 2>&1 && [ -f "${SCRIPT_DIR}/assets/rnvr.png" ]; then
    gio set "${SCRIPT_DIR}/target/release/rnvr" metadata::custom-icon "${ICON_URI}" 2>/dev/null || true
    if [ -f "${SCRIPT_DIR}/target/debug/rnvr" ]; then
        gio set "${SCRIPT_DIR}/target/debug/rnvr" metadata::custom-icon "${ICON_URI}" 2>/dev/null || true
    fi
    echo "  -> 已透過 GIO 設定 target/release/rnvr 檔案資料夾圖示為 assets/rnvr.png"
fi

# 同時在專案目錄產生一個可直接雙擊啟動並帶有圖示的 .desktop 啟動器
cat > "${SCRIPT_DIR}/rnvr.desktop" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=Foscam Rust NVR 監控系統
Comment=Foscam C5M 單機雙碼流 (3K 錄影 + 720p 預覽) 監控錄影系統
Exec=${SCRIPT_DIR}/target/release/rnvr --config ${SCRIPT_DIR}/config.toml
Icon=${SCRIPT_DIR}/assets/rnvr.png
StartupWMClass=rnvr
Terminal=false
Categories=AudioVideo;Video;Recorder;
EOF
chmod +x "${SCRIPT_DIR}/rnvr.desktop"
if command -v gio >/dev/null 2>&1; then
    gio set "${SCRIPT_DIR}/rnvr.desktop" metadata::trusted true 2>/dev/null || true
    gio set "${SCRIPT_DIR}/rnvr.desktop" metadata::custom-icon "${ICON_URI}" 2>/dev/null || true
fi

echo "[3/4] 組裝 Debian 套件目錄結構 (${STAGE_DIR})..."
rm -rf "${STAGE_DIR}"
mkdir -p "${STAGE_DIR}/DEBIAN"
mkdir -p "${STAGE_DIR}/usr/bin"
mkdir -p "${STAGE_DIR}/etc/rnvr"
mkdir -p "${STAGE_DIR}/usr/share/applications"
mkdir -p "${STAGE_DIR}/usr/share/icons/hicolor/256x256/apps"
mkdir -p "${STAGE_DIR}/usr/share/icons/hicolor/scalable/apps"

# 1. 複製執行檔與移除除錯符號以縮小體積
install -m 755 "${SCRIPT_DIR}/target/release/rnvr" "${STAGE_DIR}/usr/bin/rnvr"
strip "${STAGE_DIR}/usr/bin/rnvr" 2>/dev/null || true

# 2. 複製預設設定檔範本
install -m 644 "${SCRIPT_DIR}/config.example.toml" "${STAGE_DIR}/etc/rnvr/config.toml"

# 3. 複製系統圖示
install -m 644 "${SCRIPT_DIR}/assets/rnvr.png" "${STAGE_DIR}/usr/share/icons/hicolor/256x256/apps/rnvr.png"
install -m 644 "${SCRIPT_DIR}/assets/rnvr.svg" "${STAGE_DIR}/usr/share/icons/hicolor/scalable/apps/rnvr.svg"

# 4. 建立系統級 Ubuntu 應用程式選單捷徑
cat > "${STAGE_DIR}/usr/share/applications/rnvr.desktop" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=Foscam Rust NVR 監控系統
GenericName=NVR 監控錄影系統
Comment=Foscam C5M 單機雙碼流 (3K 錄影 + 720p 預覽) 監控錄影系統
Exec=/usr/bin/rnvr
Icon=rnvr
StartupWMClass=rnvr
Terminal=false
Categories=AudioVideo;Video;Recorder;
Keywords=NVR;RTSP;Foscam;Camera;Recorder;監控;錄影;
EOF
chmod 644 "${STAGE_DIR}/usr/share/applications/rnvr.desktop"

# 5. 建立 DEBIAN/conffiles (保護使用者修改過的 /etc/rnvr/config.toml 不被覆蓋)
cat > "${STAGE_DIR}/DEBIAN/conffiles" <<EOF
/etc/rnvr/config.toml
EOF
chmod 644 "${STAGE_DIR}/DEBIAN/conffiles"

# 計算安裝後大小 (KB)
INSTALLED_SIZE="$(du -sk "${STAGE_DIR}/usr" "${STAGE_DIR}/etc" | awk '{s+=$1} END {print s}')"

# 6. 建立 DEBIAN/control
cat > "${STAGE_DIR}/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${PKG_VERSION}
Section: video
Priority: optional
Architecture: ${ARCH}
Installed-Size: ${INSTALLED_SIZE}
Depends: libc6, libglib2.0-0, libgstreamer1.0-0, libgstreamer-plugins-base1.0-0, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, gstreamer1.0-plugins-ugly, gstreamer1.0-libav, gstreamer1.0-tools, fonts-noto-cjk, xdg-utils
Recommends: gstreamer1.0-vaapi
Maintainer: Ubuntu Rust NVR Developer <admin@localhost>
Description: Ubuntu Rust NVR Surveillance System for Foscam C5M (Dual-Stream)
 Ultra-low CPU load desktop NVR surveillance and circular recording system
 designed for Foscam C5M IP camera using Dual-Stream Split Architecture:
  - 3K Main Stream (/videoMain) zero-decode pass-through MP4 segment recording
  - 720p Sub Stream (/videoSub) lightweight live preview in Iced GUI
  - Configurable segment duration (default 30 mins), retention days (default 30 days),
    and Cron-based disk space cleanup (default Tuesday 03:00).
EOF
chmod 644 "${STAGE_DIR}/DEBIAN/control"

# 7. 建立 DEBIAN/postinst (安裝後自動更新圖示快取與桌面資料庫)
cat > "${STAGE_DIR}/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
    fi
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q /usr/share/applications >/dev/null 2>&1 || true
    fi
fi
exit 0
EOF
chmod 755 "${STAGE_DIR}/DEBIAN/postinst"

echo "[4/4] 封裝產生 .deb 安裝檔..."
mkdir -p "${DIST_DIR}"
OUTPUT_DEB="${DIST_DIR}/${DEB_NAME}.deb"
dpkg-deb --build --root-owner-group "${STAGE_DIR}" "${OUTPUT_DEB}"

if command -v gio >/dev/null 2>&1 && [ -f "${SCRIPT_DIR}/assets/rnvr.png" ]; then
    gio set "${OUTPUT_DEB}" metadata::custom-icon "${ICON_URI}" 2>/dev/null || true
fi

echo ""
echo "=========================================================="
echo "  打包完成！"
echo "  產出檔案: ${OUTPUT_DEB}"
echo ""
echo "  在另一台 Ubuntu 主機上的安裝指令："
echo "    sudo apt install ./${DEB_NAME}.deb"
echo ""
echo "  移除指令："
echo "    sudo apt remove rnvr"
echo "=========================================================="
