//! Anomaly checks: compare each new backup with the previous ones and flag the silent
//! disasters, such as a project that suddenly lost most of its files, a database dump that
//! shrank or vanished, or data that stopped compressing (typical of ransomware encryption).
//!
//! A short history of backup statistics is kept in `<local_backup_dir>/.zesty-history.json`.
//! While the newest backup is flagged, retention (`clean`) does not delete older backups.

use crate::archive::ContentStats;
use crate::backup::human_size;
use crate::config::{AnomalyConfig, AppConfig};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};

pub const HISTORY_FILE: &str = ".zesty-history.json";
/// How many backups to remember.
const HISTORY_LEN: usize = 60;
/// How many previous backups form the baseline.
const BASELINE_LEN: usize = 5;
/// The compression rule only applies to backups bigger than this.
const MIN_BYTES_FOR_RATIO: u64 = 1024 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackupStats {
    pub name: String,
    pub time: DateTime<Utc>,
    pub files: u64,
    /// Uncompressed size of everything in the archive.
    pub bytes: u64,
    pub database_bytes: u64,
    /// Size of the archive file (compressed, possibly encrypted).
    pub archive_bytes: u64,
    /// What looked wrong about this backup (empty: nothing).
    #[serde(default)]
    pub anomalies: Vec<String>,
}

impl BackupStats {
    pub fn new(name: &str, content: ContentStats, archive_bytes: u64) -> Self {
        Self {
            name: name.to_string(),
            time: Utc::now(),
            files: content.files,
            bytes: content.bytes,
            database_bytes: content.database_bytes,
            archive_bytes,
            anomalies: Vec::new(),
        }
    }

    /// Compressed size / uncompressed size.
    fn ratio(&self) -> Option<f64> {
        (self.bytes > 0).then(|| self.archive_bytes as f64 / self.bytes as f64)
    }
}

fn history_path(config: &AppConfig) -> PathBuf {
    Path::new(&config.backup.local_backup_dir).join(HISTORY_FILE)
}

pub fn load_history(config: &AppConfig) -> Vec<BackupStats> {
    fs::read_to_string(history_path(config))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_history(config: &AppConfig, history: &[BackupStats]) -> Result<()> {
    let start = history.len().saturating_sub(HISTORY_LEN);
    let path = history_path(config);
    fs::write(&path, serde_json::to_string_pretty(&history[start..])?)
        .with_context(|| format!("Failed to write {}", path.display()))
}

/// Compare `new` with the recent history, store it, and return what looks wrong.
pub fn record(config: &AppConfig, mut new: BackupStats) -> Result<Vec<String>> {
    let default = AnomalyConfig::default();
    let settings = config.anomaly.as_ref().unwrap_or(&default);
    let mut history = load_history(config);
    if settings.enabled.unwrap_or(true) {
        new.anomalies = evaluate(settings, &history, &new);
    }
    let anomalies = new.anomalies.clone();
    history.push(new);
    save_history(config, &history)?;
    Ok(anomalies)
}

/// The rules. The baseline is the median of the last few backups, so one odd backup
/// does not become the new normal.
pub fn evaluate(
    settings: &AnomalyConfig,
    history: &[BackupStats],
    new: &BackupStats,
) -> Vec<String> {
    let start = history.len().saturating_sub(BASELINE_LEN);
    let baseline = &history[start..];
    if baseline.is_empty() {
        return Vec::new();
    }
    let median = |f: &dyn Fn(&BackupStats) -> u64| -> u64 {
        let mut v: Vec<u64> = baseline.iter().map(f).collect();
        v.sort_unstable();
        v[v.len() / 2]
    };
    let mut anomalies = Vec::new();

    let mut drop_rule =
        |what: &str, before: u64, now: u64, limit: u8, fmt: &dyn Fn(u64) -> String| {
            if before > 0 && limit > 0 && limit < 100 {
                let threshold = before as f64 * (1.0 - limit as f64 / 100.0);
                if (now as f64) < threshold {
                    let pct = 100.0 - now as f64 * 100.0 / before as f64;
                    anomalies.push(format!(
                        "{} dropped by {:.0}% ({} → {})",
                        what,
                        pct,
                        fmt(before),
                        fmt(now)
                    ));
                }
            }
        };
    let size = |b: u64| human_size(b);
    let count = |n: u64| n.to_string();
    drop_rule(
        "total size",
        median(&|s| s.bytes),
        new.bytes,
        settings.size_drop_percent.unwrap_or(50),
        &size,
    );
    drop_rule(
        "file count",
        median(&|s| s.files),
        new.files,
        settings.files_drop_percent.unwrap_or(50),
        &count,
    );
    let db_before = median(&|s| s.database_bytes);
    if db_before > 0 && new.database_bytes == 0 {
        anomalies.push("the database dump is missing (previous backups had one)".to_string());
    } else {
        drop_rule(
            "database dump",
            db_before,
            new.database_bytes,
            settings.database_drop_percent.unwrap_or(50),
            &size,
        );
    }

    // Already-compressed data barely shrinks; data that used to compress well and suddenly
    // doesn't is what files look like after ransomware has encrypted them.
    let limit = settings.incompressible_ratio.unwrap_or(0.9);
    if limit > 0.0 && new.bytes >= MIN_BYTES_FOR_RATIO {
        let mut ratios: Vec<f64> = baseline
            .iter()
            .filter(|s| s.bytes >= MIN_BYTES_FOR_RATIO)
            .filter_map(BackupStats::ratio)
            .collect();
        ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if let (Some(before), Some(now)) = (ratios.get(ratios.len() / 2), new.ratio()) {
            if now >= limit && *before <= limit - 0.2 {
                anomalies.push(format!(
                    "the data stopped compressing (compressed to {:.0}% of its size, was {:.0}%): files may have been encrypted, e.g. by ransomware",
                    now * 100.0,
                    before * 100.0
                ));
            }
        }
    }
    anomalies
}

/// Why retention is paused, if it is: the newest backup was flagged.
pub fn retention_pause_reason(config: &AppConfig) -> Option<String> {
    let pause = config
        .anomaly
        .as_ref()
        .and_then(|a| a.pause_retention)
        .unwrap_or(true);
    if !pause {
        return None;
    }
    let history = load_history(config);
    let latest = history.last()?;
    (!latest.anomalies.is_empty()).then(|| {
        format!(
            "the newest backup ({}) looked wrong: {}",
            latest.name,
            latest.anomalies.join("; ")
        )
    })
}

/// Accept the newest backup as normal: it becomes the only baseline and its flags are cleared.
pub fn accept(config: &AppConfig) -> Result<Option<String>> {
    let mut history = load_history(config);
    let Some(mut latest) = history.pop() else {
        return Ok(None);
    };
    latest.anomalies.clear();
    let name = latest.name.clone();
    save_history(config, &[latest])?;
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(files: u64, bytes: u64, db: u64, archive: u64) -> BackupStats {
        BackupStats {
            name: "b".into(),
            time: Utc::now(),
            files,
            bytes,
            database_bytes: db,
            archive_bytes: archive,
            anomalies: vec![],
        }
    }

    const MB: u64 = 1024 * 1024;

    #[test]
    fn normal_changes_are_fine() {
        let history = vec![stats(1000, 100 * MB, 10 * MB, 30 * MB); 5];
        let s = AnomalyConfig::default();
        assert!(evaluate(&s, &history, &stats(1010, 104 * MB, 11 * MB, 31 * MB)).is_empty());
        assert!(
            evaluate(&s, &[], &stats(1, 1, 0, 1)).is_empty(),
            "no history, nothing to compare"
        );
    }

    #[test]
    fn drops_are_flagged() {
        let history = vec![stats(1000, 100 * MB, 10 * MB, 30 * MB); 5];
        let s = AnomalyConfig::default();
        let a = evaluate(&s, &history, &stats(300, 20 * MB, 10 * MB, 6 * MB));
        assert_eq!(a.len(), 2, "{:?}", a);
        assert!(a[0].starts_with("total size dropped by 80%"), "{}", a[0]);
        assert!(
            a[1].starts_with("file count dropped by 70% (1000 → 300)"),
            "{}",
            a[1]
        );

        let a = evaluate(&s, &history, &stats(1000, 90 * MB, 0, 27 * MB));
        assert_eq!(
            a,
            vec!["the database dump is missing (previous backups had one)"]
        );
        let a = evaluate(&s, &history, &stats(1000, 92 * MB, 2 * MB, 28 * MB));
        assert!(a[0].starts_with("database dump dropped by 80%"), "{:?}", a);
    }

    #[test]
    fn ransomware_like_data_is_flagged() {
        let history = vec![stats(1000, 100 * MB, 0, 30 * MB); 5];
        let s = AnomalyConfig::default();
        let a = evaluate(&s, &history, &stats(1000, 100 * MB, 0, 98 * MB));
        assert_eq!(a.len(), 1);
        assert!(a[0].contains("stopped compressing"), "{}", a[0]);
        // Data that never compressed (photos, videos) is not suspicious.
        let photos = vec![stats(1000, 100 * MB, 0, 97 * MB); 5];
        assert!(evaluate(&s, &photos, &stats(1000, 100 * MB, 0, 98 * MB)).is_empty());
    }

    #[test]
    fn one_odd_backup_does_not_become_the_baseline() {
        let mut history = vec![stats(1000, 100 * MB, 0, 30 * MB); 4];
        history.push(stats(100, 10 * MB, 0, 3 * MB)); // the odd one
        let s = AnomalyConfig::default();
        assert!(!evaluate(&s, &history, &stats(100, 10 * MB, 0, 3 * MB)).is_empty());
    }

    #[test]
    fn thresholds_are_configurable() {
        let history = vec![stats(1000, 100 * MB, 0, 30 * MB); 5];
        let s = AnomalyConfig {
            size_drop_percent: Some(90),
            files_drop_percent: Some(90),
            ..Default::default()
        };
        assert!(evaluate(&s, &history, &stats(300, 20 * MB, 0, 6 * MB)).is_empty());
    }
}
