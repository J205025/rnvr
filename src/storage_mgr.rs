use crate::config::StorageConfig;
use anyhow::{bail, Result};
use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, Timelike};
use flume::{Receiver, Sender};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{error, info, warn};

const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;
const SECS_PER_DAY: u64 = 86_400;

#[derive(Debug, Clone)]
pub struct StorageStats {
    pub used_bytes: u64,
    pub max_bytes: u64,
    pub file_count: usize,
    pub deleted_in_last_run: usize,
    #[allow(dead_code)]
    pub last_check_time: String,
    pub next_cron_time: String,
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
    UpdateConfig(StorageConfig),
    Shutdown,
}

/// 標準 5 欄位 Cron 排程解析與計算器 (`分 時 日 月 週`)
/// 例如 `"0 3 * * 2"` 代表每個星期二 03:00
#[derive(Debug, Clone)]
pub struct CronSchedule {
    minutes: [bool; 60],
    hours: [bool; 24],
    days_of_month: [bool; 32], // 1..=31
    months: [bool; 13],        // 1..=12
    days_of_week: [bool; 7],   // 0..=6 (0 = 週日, 1 = 週一, 2 = 週二 ... 6 = 週六)
    dom_is_wildcard: bool,
    dow_is_wildcard: bool,
}

impl CronSchedule {
    pub fn parse(expr: &str) -> Result<Self> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            bail!(
                "Cron 表達式必須為 5 個欄位 (分 時 日 月 週)，例如 '0 3 * * 2'，目前為 {} 個欄位",
                fields.len()
            );
        }

        let mut minutes = [false; 60];
        let mut hours = [false; 24];
        let mut days_of_month = [false; 32];
        let mut months = [false; 13];
        let mut days_of_week = [false; 7];

        parse_cron_field(fields[0], 0, 59, false, &mut minutes)?;
        parse_cron_field(fields[1], 0, 23, false, &mut hours)?;
        parse_cron_field(fields[2], 1, 31, false, &mut days_of_month)?;
        parse_cron_field(fields[3], 1, 12, false, &mut months)?;
        parse_cron_field(fields[4], 0, 7, true, &mut days_of_week)?;

        Ok(Self {
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            dom_is_wildcard: fields[2] == "*",
            dow_is_wildcard: fields[4] == "*",
        })
    }

    pub fn matches(&self, dt: &DateTime<Local>) -> bool {
        let min = dt.minute() as usize;
        let hour = dt.hour() as usize;
        let dom = dt.day() as usize;
        let month = dt.month() as usize;
        let dow = dt.weekday().num_days_from_sunday() as usize;

        if !self.minutes[min] || !self.hours[hour] || !self.months[month] {
            return false;
        }

        let dom_match = self.days_of_month[dom];
        let dow_match = self.days_of_week[dow];

        // 遵循標準 Vixie Cron 規則：若「日」與「週」皆非 '*'，則滿足其一即可；若其一為 '*'，則需兩者皆滿足
        if !self.dom_is_wildcard && !self.dow_is_wildcard {
            dom_match || dow_match
        } else {
            dom_match && dow_match
        }
    }

    /// 計算 `after` 時間點之後的下一次觸發時間 (精確到分，秒數歸零)
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        // 從下一分鐘整點開始往後搜尋 (最多搜尋 366 天 = 527,040 分鐘)
        let start = (after + ChronoDuration::minutes(1))
            .with_second(0)?
            .with_nanosecond(0)?;

        let mut current = start;
        for _ in 0..(366 * 24 * 60) {
            if self.matches(&current) {
                return Some(current);
            }
            current += ChronoDuration::minutes(1);
        }
        None
    }
}

fn parse_cron_field(
    field: &str,
    min: usize,
    max: usize,
    is_dow: bool,
    out: &mut [bool],
) -> Result<()> {
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            bail!("Cron 欄位不可為空");
        }

        let (range_part, step) = if let Some((r, s)) = part.split_once('/') {
            let step_val: usize = s
                .parse()
                .map_err(|_| anyhow::anyhow!("無效的 Cron 步進值: {}", s))?;
            if step_val == 0 {
                bail!("Cron 步進值不可為 0");
            }
            (r, step_val)
        } else {
            (part, 1usize)
        };

        let (start, end) = if range_part == "*" {
            (min, if is_dow { 6 } else { max })
        } else if let Some((a, b)) = range_part.split_once('-') {
            let s_val = parse_cron_atom(a, is_dow)?;
            let e_val = parse_cron_atom(b, is_dow)?;
            if s_val < min || e_val > max || s_val > e_val {
                bail!("Cron 區間超出範圍: {}", range_part);
            }
            (s_val, e_val)
        } else {
            let val = parse_cron_atom(range_part, is_dow)?;
            if val < min || val > max {
                bail!("Cron 數值超出範圍 ({}..={}): {}", min, max, val);
            }
            if part.contains('/') {
                (val, if is_dow { 6 } else { max })
            } else {
                (val, val)
            }
        };

        let mut cur = start;
        while cur <= end {
            let idx = if is_dow && cur == 7 { 0 } else { cur };
            if idx < out.len() {
                out[idx] = true;
            }
            cur += step;
        }
    }
    Ok(())
}

fn parse_cron_atom(token: &str, is_dow: bool) -> Result<usize> {
    if is_dow {
        match token.to_ascii_uppercase().as_str() {
            "SUN" => return Ok(0),
            "MON" => return Ok(1),
            "TUE" => return Ok(2),
            "WED" => return Ok(3),
            "THU" => return Ok(4),
            "FRI" => return Ok(5),
            "SAT" => return Ok(6),
            _ => {}
        }
    }
    token
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("無效的 Cron 數值: {}", token))
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

/// 執行空間統計與雙重循環覆寫清理：
/// 1. 【過期天數清理】：若 `retention_days > 0`，自動刪除修改時間超過 `retention_days` 天的舊錄影檔。
/// 2. 【容量水位保護】：若剩餘總容量仍超過 `max_bytes`，由最舊到最新依序刪除 `.mp4`，直到總容量降至 `90%` 水位以下。
pub fn enforce_retention_policy(
    record_dir: &Path,
    max_bytes: u64,
    retention_days: u32,
    perform_delete: bool,
    next_cron_time: String,
) -> StorageStats {
    let _ = fs::create_dir_all(record_dir);

    let mut files = Vec::new();
    collect_mp4_files(record_dir, &mut files);

    // 依修改時間由舊到新排序 (最舊的排在最前面)
    files.sort_by_key(|f| f.modified);

    let mut total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let mut deleted_count = 0usize;

    if perform_delete && !files.is_empty() {
        // 保留最後一個最新檔案 (可能正在被錄影管線寫入)
        let deletable_len = if files.len() > 1 {
            files.len() - 1
        } else {
            0
        };

        let mut removed_indices = vec![false; files.len()];

        // 第一階段：刪除超過 retention_days 天的過期錄影檔
        if retention_days > 0 {
            let max_age = Duration::from_secs((retention_days as u64).saturating_mul(SECS_PER_DAY));
            if let Some(cutoff_time) = SystemTime::now().checked_sub(max_age) {
                for idx in 0..deletable_len {
                    let entry = &files[idx];
                    if entry.modified < cutoff_time {
                        match fs::remove_file(&entry.path) {
                            Ok(_) => {
                                total_bytes = total_bytes.saturating_sub(entry.size);
                                deleted_count += 1;
                                removed_indices[idx] = true;
                                info!(
                                    "已自動刪除超過 {} 天之過期錄影檔: {} (釋放 {:.2} MB)",
                                    retention_days,
                                    entry.path.display(),
                                    entry.size as f64 / (1024.0 * 1024.0)
                                );
                            }
                            Err(err) => {
                                warn!("刪除過期錄影檔失敗 ({}): {}", entry.path.display(), err);
                            }
                        }
                    }
                }
            }
        }

        // 第二階段：若總容量仍超過上限 max_bytes，依序由舊到新刪除至 90% 水位以下
        let target_watermark = ((max_bytes as f64) * 0.90) as u64;
        if total_bytes > max_bytes {
            info!(
                "錄影資料夾容量 ({:.2} GB) 超過上限 ({:.2} GB)，開始清理舊檔至 90% 水位 ({:.2} GB) 以下...",
                total_bytes as f64 / BYTES_PER_GB,
                max_bytes as f64 / BYTES_PER_GB,
                target_watermark as f64 / BYTES_PER_GB,
            );

            for idx in 0..deletable_len {
                if removed_indices[idx] {
                    continue;
                }
                if total_bytes <= target_watermark {
                    break;
                }
                let entry = &files[idx];
                match fs::remove_file(&entry.path) {
                    Ok(_) => {
                        total_bytes = total_bytes.saturating_sub(entry.size);
                        deleted_count += 1;
                        removed_indices[idx] = true;
                        info!(
                            "已自動刪除超量舊錄影檔: {} (釋放 {:.2} MB)",
                            entry.path.display(),
                            entry.size as f64 / (1024.0 * 1024.0)
                        );
                    }
                    Err(err) => {
                        warn!("刪除舊錄影檔失敗 ({}): {}", entry.path.display(), err);
                    }
                }
            }
        }

        if deleted_count > 0 {
            remove_empty_date_dirs(record_dir);
        }
    }

    let remaining_count = files.len().saturating_sub(deleted_count);

    StorageStats {
        used_bytes: total_bytes,
        max_bytes,
        file_count: remaining_count,
        deleted_in_last_run: deleted_count,
        last_check_time: Local::now().format("%H:%M:%S").to_string(),
        next_cron_time,
    }
}

/// 啟動背景非同步循環錄影空間管理員 (Tokio Task，支援 Cron 排程與過期天數清理)
pub fn spawn_storage_manager(
    mut config: StorageConfig,
    stats_tx: Sender<StorageStats>,
    cmd_rx: Receiver<StorageCommand>,
) {
    tokio::spawn(async move {
        if let Err(e) = fs::create_dir_all(&config.record_dir) {
            error!(
                "無法建立錄影主目錄 ({}): {}",
                config.record_dir.display(),
                e
            );
        }

        let mut cron_sched = CronSchedule::parse(&config.cleanup_cron).unwrap_or_else(|e| {
            warn!(
                "Cron 表達式 '{}' 解析失敗 ({})，退回預設每週二 03:00 ('0 3 * * 2')",
                config.cleanup_cron, e
            );
            CronSchedule::parse("0 3 * * 2").expect("預設 Cron 解析不可失敗")
        });

        let mut next_cron_dt = cron_sched.next_after(Local::now());
        let next_cron_str = format_next_cron(&next_cron_dt);
        let max_bytes = (config.max_storage_gb * BYTES_PER_GB) as u64;

        // 啟動時立即執行一次檢查與清理
        let initial_stats = enforce_retention_policy(
            &config.record_dir,
            max_bytes,
            config.retention_days,
            true,
            next_cron_str,
        );
        let mut last_deleted_count = initial_stats.deleted_in_last_run;
        let _ = stats_tx.send(initial_stats);

        // 每 15 秒檢查一次是否到達 Cron 排程時間，並更新介面磁碟容量顯示 (降低背景磁碟掃描與 UI 重繪頻率)
        let mut ticker = tokio::time::interval(Duration::from_secs(15));

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let now = Local::now();
                    let cron_triggered = match next_cron_dt {
                        Some(target) => now >= target,
                        None => false,
                    };

                    if cron_triggered {
                        info!(
                            "到達 Cron 排程巡檢時間 ({})，開始執行超過 {} 天過期檔案與容量清理...",
                            config.cleanup_cron, config.retention_days
                        );
                        next_cron_dt = cron_sched.next_after(now);
                    }

                    let record_dir = config.record_dir.clone();
                    let max_b = (config.max_storage_gb * BYTES_PER_GB) as u64;
                    let ret_days = config.retention_days;
                    let next_str = format_next_cron(&next_cron_dt);

                    if let Ok(mut stats) = tokio::task::spawn_blocking(move || {
                        // Cron 觸發時執行完整過期與水位刪除；平時僅統計容量 (除非硬碟已超標才立即啟動保護)
                        let s = enforce_retention_policy(&record_dir, max_b, ret_days, cron_triggered, next_str.clone());
                        if !cron_triggered && s.used_bytes > max_b {
                            enforce_retention_policy(&record_dir, max_b, ret_days, true, next_str)
                        } else {
                            s
                        }
                    }).await {
                        if cron_triggered || stats.deleted_in_last_run > 0 {
                            last_deleted_count = stats.deleted_in_last_run;
                        } else {
                            stats.deleted_in_last_run = last_deleted_count;
                        }
                        let _ = stats_tx.send(stats);
                    }
                }
                Ok(cmd) = cmd_rx.recv_async() => {
                    match cmd {
                        StorageCommand::RefreshNow => {
                            let record_dir = config.record_dir.clone();
                            let max_b = (config.max_storage_gb * BYTES_PER_GB) as u64;
                            let ret_days = config.retention_days;
                            let next_str = format_next_cron(&next_cron_dt);
                            if let Ok(mut stats) = tokio::task::spawn_blocking(move || {
                                enforce_retention_policy(&record_dir, max_b, ret_days, false, next_str)
                            }).await {
                                stats.deleted_in_last_run = last_deleted_count;
                                let _ = stats_tx.send(stats);
                            }
                        }
                        StorageCommand::RunCleanupNow => {
                            let record_dir = config.record_dir.clone();
                            let max_b = (config.max_storage_gb * BYTES_PER_GB) as u64;
                            let ret_days = config.retention_days;
                            let next_str = format_next_cron(&next_cron_dt);
                            if let Ok(stats) = tokio::task::spawn_blocking(move || {
                                enforce_retention_policy(&record_dir, max_b, ret_days, true, next_str)
                            }).await {
                                last_deleted_count = stats.deleted_in_last_run;
                                let _ = stats_tx.send(stats);
                            }
                        }
                        StorageCommand::UpdateConfig(new_cfg) => {
                            info!(
                                "空間管理員套用新設定：保留 {} 天、上限 {:.1} GB、Cron '{}'",
                                new_cfg.retention_days, new_cfg.max_storage_gb, new_cfg.cleanup_cron
                            );
                            if let Ok(new_sched) = CronSchedule::parse(&new_cfg.cleanup_cron) {
                                cron_sched = new_sched;
                            }
                            config = new_cfg;
                            next_cron_dt = cron_sched.next_after(Local::now());

                            let record_dir = config.record_dir.clone();
                            let max_b = (config.max_storage_gb * BYTES_PER_GB) as u64;
                            let ret_days = config.retention_days;
                            let next_str = format_next_cron(&next_cron_dt);
                            if let Ok(stats) = tokio::task::spawn_blocking(move || {
                                enforce_retention_policy(&record_dir, max_b, ret_days, true, next_str)
                            }).await {
                                last_deleted_count = stats.deleted_in_last_run;
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

fn format_next_cron(dt: &Option<DateTime<Local>>) -> String {
    match dt {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => "無".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Write;
    use std::thread::sleep;

    #[test]
    fn test_cron_schedule_tuesday_0300() {
        let sched = CronSchedule::parse("0 3 * * 2").expect("應成功解析每週二 03:00 Cron");
        // 2026-11-08 是星期日 (Sunday)
        let sun = Local.with_ymd_and_hms(2026, 11, 8, 17, 50, 0).unwrap();
        let next = sched.next_after(sun).expect("應找到下一次觸發時間");
        // 下一個星期二是 2026-11-10 03:00
        assert_eq!(next.year(), 2026);
        assert_eq!(next.month(), 11);
        assert_eq!(next.day(), 10);
        assert_eq!(next.hour(), 3);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn test_retention_days_deletes_expired_files() {
        let temp_dir =
            std::env::temp_dir().join(format!("rnvr_age_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let date_dir = temp_dir.join("2026-09-01");
        fs::create_dir_all(&date_dir).unwrap();

        let old_file = date_dir.join("foscam_old.mp4");
        let recent_file = date_dir.join("foscam_recent.mp4");
        let newest_file = date_dir.join("foscam_newest.mp4");

        for p in [&old_file, &recent_file, &newest_file] {
            let mut f = fs::File::create(p).unwrap();
            f.write_all(&vec![0u8; 1000]).unwrap();
        }

        // 將 old_file 的修改時間設為 35 天前，recent_file 設為 10 天前
        let now = SystemTime::now();
        let thirty_five_days_ago = now - Duration::from_secs(35 * SECS_PER_DAY);
        let ten_days_ago = now - Duration::from_secs(10 * SECS_PER_DAY);

        fs::File::open(&old_file)
            .unwrap()
            .set_modified(thirty_five_days_ago)
            .unwrap();
        fs::File::open(&recent_file)
            .unwrap()
            .set_modified(ten_days_ago)
            .unwrap();

        // 執行保留 30 天清理 (容量上限設大，純測天數清理)
        let stats = enforce_retention_policy(
            &temp_dir,
            100_000_000,
            30,
            true,
            "2026-11-10 03:00".to_string(),
        );

        assert_eq!(stats.deleted_in_last_run, 1);
        assert_eq!(stats.file_count, 2);
        assert!(!old_file.exists(), "超過 30 天的檔案應被刪除");
        assert!(recent_file.exists(), "10 天前的檔案應保留");
        assert!(newest_file.exists(), "最新檔案應保留");

        let _ = fs::remove_dir_all(&temp_dir);
    }

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
        let stats = enforce_retention_policy(
            &temp_dir,
            9000,
            30,
            true,
            "2026-11-10 03:00".to_string(),
        );
        assert_eq!(stats.deleted_in_last_run, 2);
        assert_eq!(stats.file_count, 8);
        assert_eq!(stats.used_bytes, 8000);
        assert!(!date_dir.join("foscam_20261108_150000.mp4").exists());
        assert!(!date_dir.join("foscam_20261108_150001.mp4").exists());
        assert!(date_dir.join("foscam_20261108_150002.mp4").exists());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
