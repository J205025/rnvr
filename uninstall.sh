#!/usr/bin/env bash
set -euo pipefail

echo "=========================================================="
echo "  Ubuntu Rust NVR v1.0.0 (Foscam C5M) 一鍵移除腳本"
echo "  作者: Lin Ming-Chi <j205025@gmail.com>"
echo "=========================================================="

PURGE_CONFIG=false
for arg in "$@"; do
    case "$arg" in
        --purge|-p)
            PURGE_CONFIG=true
            ;;
        --help|-h)
            echo "用法: ./uninstall.sh [選項]"
            echo "  (無參數)       移除 rnvr 執行檔、桌面捷徑與圖示 (預設保留設定檔與錄影檔)"
            echo "  -p, --purge    同時移除 ~/.config/rnvr 設定檔資料夾 (仍安全保留錄影檔)"
            echo "  -h, --help     顯示此說明"
            exit 0
            ;;
    esac
done

echo "[1/3] 檢查並關閉執行中的 rnvr 監控程序..."
if pgrep -x "rnvr" >/dev/null 2>&1; then
    echo "偵測到執行中的 rnvr，正在發送安全終止訊號 (SIGTERM)..."
    pkill -TERM -x "rnvr" || true
    sleep 1
fi

echo "[2/3] 移除系統執行檔 (/usr/local/bin/rnvr)..."
if [ -f "/usr/local/bin/rnvr" ]; then
    sudo rm -f /usr/local/bin/rnvr
    echo "已移除: /usr/local/bin/rnvr"
else
    echo "未發現 /usr/local/bin/rnvr (可能已移除)"
fi

echo "[3/3] 移除桌面應用程式捷徑與圖示..."
APP_DESKTOP="${HOME}/.local/share/applications/rnvr.desktop"
ICON_PNG="${HOME}/.local/share/icons/hicolor/256x256/apps/rnvr.png"
ICON_SVG="${HOME}/.local/share/icons/hicolor/scalable/apps/rnvr.svg"

for f in "${APP_DESKTOP}" "${ICON_PNG}" "${ICON_SVG}"; do
    if [ -f "$f" ]; then
        rm -f "$f"
        echo "已移除: $f"
    fi
done

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "${HOME}/.local/share/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "${HOME}/.local/share/icons/hicolor" >/dev/null 2>&1 || true
fi

CONFIG_DIR="${HOME}/.config/rnvr"
if [ -d "${CONFIG_DIR}" ]; then
    if [ "${PURGE_CONFIG}" = true ]; then
        rm -rf "${CONFIG_DIR}"
        echo "已清除設定檔目錄: ${CONFIG_DIR}"
    elif [ -t 0 ]; then
        echo ""
        read -r -p "是否要一併刪除使用者設定檔 (${CONFIG_DIR})？ [y/N]: " ans
        case "${ans}" in
            [yY][eE][sS]|[yY])
                rm -rf "${CONFIG_DIR}"
                echo "已清除設定檔目錄: ${CONFIG_DIR}"
                ;;
            *)
                echo "已保留設定檔目錄: ${CONFIG_DIR}"
                ;;
        esac
    else
        echo "已保留設定檔目錄: ${CONFIG_DIR} (若需一併清除可加上 --purge 參數)"
    fi
fi

echo ""
echo "=========================================================="
echo "移除完成！"
echo "※ 為保護您的監控歷史畫面，錄影資料夾預設予以保留："
echo "   ${HOME}/Foscam_Recordings"
echo "   (若確認不再需要歷史錄影，可手動執行: rm -rf ~/Foscam_Recordings)"
echo "=========================================================="
