use crate::config::StorageConfig;
use anyhow::Result;
use chrono::Local;
use flume::{Receiver, Sender};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{error, info, warn};

const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

#[derive(Debug, Clone)]
pub struct StorageStats {
    pub used_bytes: u64,
    pub max_bytes: u64,
    pub file_count: usize,
    pub deleted_in_last_run: usize,
    pub last_check_time: String,
}

impl StorageStats {
    pub fn used_gb(&self) -> f64 {
        self.used_bytes as f64 / BYTES_PER_GB
    }

    pub fn max_gb(&self) -> f64 {
        self.max_bytes as f64 / BYTES_PER_GB
    }

    pub fn usage_ratio(&self) -> f32 {
        if self.max_bytes == 0 {
            return 0.0;
        }
        ((self.used_bytes as f64 / self.max_bytes as f64) as f32).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone)]
pub enum StorageCommand {
    RefreshNow,
    RunCleanupNow,
    Shutdown,
}

#[derive(Debug, Clone)]
struct Mp4FileEntry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

/// 依日期建立子資料夾並產生符合規格的 MP4 存檔路徑：
/// `<record_dir>/YYYY-MM-DD/foscam_YYYYMMDD_HHMMSS.mp4`
pub fn generate_segment_filepath(record_dir: &Path) -> Result<PathBuf> {
    let now = Local::now();
    let date_folder = now.format("%Y-%m-%d").to_string();
    let filename = format!("foscam_{}.mp4", now.format("%Y%m%d_%H%M%S"));

    let dir_path = record_dir.join(date_folder);
    fs::create_dir_all(&dir_path)?;

    let mut candidate = dir_path.join(&filename);
    // 若同一秒內產生多個檔案，加上序號避免覆蓋
    if candidate.exists() {
        let millis = now.timestamp_subsec_millis();
        let alt_name = format!("foscam_{}_{:03}.mp4", now.format("%Y%m%d_%H%M%S"), millis);
        candidate = dir_path.join(alt_name);
    }

    Ok(candidate)
}

/// 遞迴掃描 `record_dir` 下所有 `.mp4` 錄影檔
fn collect_mp4_files(dir: &Path, out: &mut Vec<Mp4FileEntry>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_mp4_files(&path, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("mp4"))
            .unwrap_or(false)
        {
            if let Ok(meta) = entry.metadata() {
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                out.push(Mp4FileEntry {
                    path,
                    size: meta.len(),
                    modified,
                });
            }
        }
    }
}

/// 清理空的日期子資料夾
fn remove_empty_date_dirs(record_dir: &Path) {
    let entries = match fs::read_dir(record_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // 跳過 screenshots 資料夾
            if path.file_name().and_then(|n| n.to_str()) == Some("screenshots") {
                continue;
            }
            if let Ok(mut sub) = fs::read_dir(&path) {
                if sub.next().is_none() {
                    let _ = fs::remove_dir(&path);
                }
            }
        }
    }
}

/// 執行空間統計與循環覆寫清理：
/// 若總容量超過 `max_bytes`，由最舊到最新依序刪除 `.mp4`，直到總容量降至 `90%` 水位以下。
pub fn enforce_retention_policy(
    record_dir: &Path,
    max_bytes: u64,
    perform_delete: bool,
) -> StorageStats {
    let _ = fs::create_dir_all(record_dir);

    let mut files = Vec::new();
    collect_mp4_files(record_dir, &mut files);

    // 依修改時間由舊到新排序 (最舊的排在最前面)
    files.sort_by_key(|f| f.modified);

    let mut total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let mut deleted_count = 0usize;

    // 目標水位為上限的 90%
    let target_watermark = ((max_bytes as f64) * 0.90) as u64;

    if perform_delete && total_bytes > max_bytes {
        info!(
            "錄影資料夾容量 ({:.2} GB) 超過上限 ({:.2} GB)，開始清理舊檔至 90% 水位 ({:.2} GB) 以下...",
            total_bytes as f64 / BYTES_PER_GB,
            max_bytes as f64 / BYTES_PER_GB,
            target_watermark as f64 / BYTES_PER_GB,
        );

        // 保留最後一個最新檔案 (可能正在被錄影管線寫入)，除非只有一個檔案且超大
        let deletable_len = if files.len() > 1 {
            files.len() - 1
        } else {
            0
        };

        for entry in files.iter().take(deletable_len) {
            if total_bytes <= target_watermark {
                break;
            }
            match fs::remove_file(&entry.path) {
                Ok(_) => {
                    total_bytes = total_bytes.saturating_sub(entry.size);
                    deleted_count += 1;
                    info!(
                        "已自動刪除舊錄影檔: {} (釋放 {:.2} MB)",
                        entry.path.display(),
                        entry.size as f64 / (1024.0 * 1024.0)
                    );
                }
                Err(err) => {
                    warn!("刪除舊錄影檔失敗 ({}): {}", entry.path.display(), err);
                }
            }
        }

        remove_empty_date_dirs(record_dir);
    }

    let remaining_count = files.len().saturating_sub(deleted_count);

    StorageStats {
        used_bytes: total_bytes,
        max_bytes,
        file_count: remaining_count,
        deleted_in_last_run: deleted_count,
        last_check_time: Local::now().format("%H:%M:%S").to_string(),
    }
}

/// 啟動背景非同步循環錄影空間管理員 (Tokio Task)
pub fn spawn_storage_manager(
    config: StorageConfig,
    stats_tx: Sender<StorageStats>,
    cmd_rx: Receiver<StorageCommand>,
) {
    tokio::spawn(async move {
        let max_bytes = (config.max_storage_gb * BYTES_PER_GB) as u64;
        if let Err(e) = fs::create_dir_all(&config.record_dir) {
            error!(
                "無法建立錄影主目錄 ({}): {}",
                config.record_dir.display(),
                e
            );
        }

        // 啟動時立即執行一次檢查與清理
        let initial_stats = enforce_retention_policy(&config.record_dir, max_bytes, true);
        let _ = stats_tx.send(initial_stats);

        let mut cleanup_auto_ticker =
            tokio::time::interval(Duration::from_secs(config.cleanup_interval_secs.max(10)));
        // 每 5 秒更新一次介面上的磁碟容量統計 (不強制觸發刪除，除非已超標)
        let mut ui_refresh_ticker = tokio::time::interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                _ = cleanup_auto_ticker.tick() => {
                    let record_dir = config.record_dir.clone();
                    let stats = tokio::task::spawn_blocking(move || {
                        enforce_retention_policy(&record_dir, max_bytes, true)
                    })
                    .await
                    .unwrap_or_else(|_| StorageStats {
                        used_bytes: 0,
                        max_bytes,
                        file_count: 0,
                        deleted_in_last_run: 0,
                        last_check_time: Local::now().format("%H:%M:%S").to_string(),
                    });
                    let _ = stats_tx.send(stats);
                }
                _ = ui_refresh_ticker.tick() => {
                    let record_dir = config.record_dir.clone();
                    let stats = tokio::task::spawn_blocking(move || {
                        // 若超標也立即保護磁碟空間
                        enforce_retention_policy(&record_dir, max_bytes, true)
                    })
                    .await
                    .unwrap_or_else(|_| StorageStats {
                        used_bytes: 0,
                        max_bytes,
                        file_count: 0,
                        deleted_in_last_run: 0,
                        last_check_time: Local::now().format("%H:%M:%S").to_string(),
                    });
                    let _ = stats_tx.send(stats);
                }
                Ok(cmd) = cmd_rx.recv_async() => {
                    match cmd {
                        StorageCommand::RefreshNow => {
                            let record_dir = config.record_dir.clone();
                            if let Ok(stats) = tokio::task::spawn_blocking(move || {
                                enforce_retention_policy(&record_dir, max_bytes, false)
                            }).await {
                                let _ = stats_tx.send(stats);
                            }
                        }
                        StorageCommand::RunCleanupNow => {
                            let record_dir = config.record_dir.clone();
                            if let Ok(stats) = tokio::task::spawn_blocking(move || {
                                enforce_retention_policy(&record_dir, max_bytes, true)
                            }).await {
                                let _ = stats_tx.send(stats);
                            }
                        }
                        StorageCommand::Shutdown => {
                            info!("空間管理員收到結束指令，停止巡檢。");
                            break;
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::thread::sleep;

    #[test]
    fn test_retention_policy_deletes_oldest_until_below_90_percent() {
        let temp_dir =
            std::env::temp_dir().join(format!("rnvr_storage_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let date_dir = temp_dir.join("2026-11-08");
        fs::create_dir_all(&date_dir).unwrap();

        // 建立 10 個各 1000 bytes 的假 mp4 檔案，總共 10,000 bytes
        for i in 0..10 {
            let file_path = date_dir.join(format!("foscam_20261108_15000{}.mp4", i));
            let mut f = fs::File::create(&file_path).unwrap();
            f.write_all(&vec![0u8; 1000]).unwrap();
            sleep(Duration::from_millis(15));
        }

        // 設定上限為 9,000 bytes -> 90% 水位為 8,100 bytes
        // 目前有 10,000 bytes，應該刪除最舊的 2 個檔案 (剩 8,000 bytes <= 8,100 bytes)
        let stats = enforce_retention_policy(&temp_dir, 9000, true);
        assert_eq!(stats.deleted_in_last_run, 2);
        assert_eq!(stats.file_count, 8);
        assert_eq!(stats.used_bytes, 8000);
        assert!(!date_dir.join("foscam_20261108_150000.mp4").exists());
        assert!(!date_dir.join("foscam_20261108_150001.mp4").exists());
        assert!(date_dir.join("foscam_20261108_150002.mp4").exists());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
