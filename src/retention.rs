//! Which backups to keep.
//!
//! Two styles:
//! * `[backup] retention_days = 7`: keep everything from the last 7 days (the original behaviour);
//! * `[retention]`: keep the newest backup of each of the last N days, weeks, months and years
//!   ("grandfather-father-son"), plus optionally the last N backups and everything within N days.
//!
//! The newest backup is always kept, and backups with an unknown date are never deleted.

use crate::config::AppConfig;
use chrono::{DateTime, Datelike, Local, Utc};
use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub keep_last: usize,
    pub keep_within_days: Option<u32>,
    pub keep_daily: usize,
    pub keep_weekly: usize,
    pub keep_monthly: usize,
    pub keep_yearly: usize,
}

impl Policy {
    pub fn from_config(config: &AppConfig) -> Self {
        match &config.retention {
            Some(r) => Self {
                keep_last: r.keep_last.unwrap_or(1).max(1),
                keep_within_days: r.keep_within_days,
                keep_daily: r.keep_daily.unwrap_or(0),
                keep_weekly: r.keep_weekly.unwrap_or(0),
                keep_monthly: r.keep_monthly.unwrap_or(0),
                keep_yearly: r.keep_yearly.unwrap_or(0),
            },
            None => Self {
                keep_last: 1,
                keep_within_days: Some(config.backup.retention_days()),
                ..Default::default()
            },
        }
    }

    /// One line for logs and `status`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(d) = self.keep_within_days {
            parts.push(format!("everything from the last {} days", d));
        }
        for (n, what) in [
            (self.keep_daily, "daily"),
            (self.keep_weekly, "weekly"),
            (self.keep_monthly, "monthly"),
            (self.keep_yearly, "yearly"),
        ] {
            if n > 0 {
                parts.push(format!("{} {}", n, what));
            }
        }
        if self.keep_last > 1 {
            parts.push(format!("the last {}", self.keep_last));
        } else {
            parts.push("the newest".to_string());
        }
        format!("keep {}", parts.join(", "))
    }
}

/// Decide for each backup (given its time, in any order) whether to keep it, and why.
/// Returns, per input index, the reasons to keep it; an empty list means delete.
pub fn decide(
    times: &[Option<DateTime<Utc>>],
    policy: &Policy,
    now: DateTime<Utc>,
) -> Vec<Vec<&'static str>> {
    let mut reasons: Vec<Vec<&'static str>> = vec![Vec::new(); times.len()];

    // Newest first; unknown dates are kept and don't take part.
    let mut order: Vec<usize> = (0..times.len()).filter(|&i| times[i].is_some()).collect();
    order.sort_by(|&a, &b| times[b].cmp(&times[a]));
    for (i, t) in times.iter().enumerate() {
        if t.is_none() {
            reasons[i].push("unknown date");
        }
    }

    for &i in order.iter().take(policy.keep_last.max(1)) {
        reasons[i].push("last");
    }
    if let Some(days) = policy.keep_within_days {
        let cutoff = now - chrono::Duration::days(days as i64);
        for &i in &order {
            if times[i].unwrap() >= cutoff {
                reasons[i].push("recent");
            }
        }
    }

    // For each period kind: walk from newest to oldest and keep the first backup of each
    // new period, until N periods are covered.
    type PeriodKey = fn(DateTime<Local>) -> (i32, u32);
    let periods: [(usize, &'static str, PeriodKey); 4] = [
        (policy.keep_daily, "daily", |t| (t.year(), t.ordinal())),
        (policy.keep_weekly, "weekly", |t| {
            let w = t.iso_week();
            (w.year(), w.week())
        }),
        (policy.keep_monthly, "monthly", |t| (t.year(), t.month())),
        (policy.keep_yearly, "yearly", |t| (t.year(), 0)),
    ];
    for (count, label, key) in periods {
        if count == 0 {
            continue;
        }
        let mut seen: HashSet<(i32, u32)> = HashSet::new();
        for &i in &order {
            if seen.len() >= count {
                break;
            }
            let k = key(times[i].unwrap().with_timezone(&Local));
            if seen.insert(k) {
                reasons[i].push(label);
            }
        }
    }
    reasons
}

/// For `clean --dry-run` style summaries: how many backups each rule keeps.
pub fn summarize(reasons: &[Vec<&'static str>]) -> String {
    const ORDER: [&str; 7] = [
        "last",
        "recent",
        "daily",
        "weekly",
        "monthly",
        "yearly",
        "unknown date",
    ];
    let delete = reasons.iter().filter(|r| r.is_empty()).count();
    let keep = reasons.len() - delete;
    let counts: Vec<String> = ORDER
        .iter()
        .filter_map(|label| {
            let n = reasons.iter().filter(|r| r.contains(label)).count();
            (n > 0).then(|| format!("{} {}", n, label))
        })
        .collect();
    format!(
        "{} kept, {} to delete ({})",
        keep,
        delete,
        counts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32) -> Option<DateTime<Utc>> {
        Some(
            Local
                .with_ymd_and_hms(y, m, d, h, 0, 0)
                .unwrap()
                .with_timezone(&Utc),
        )
    }

    #[test]
    fn days_policy_matches_the_old_behaviour() {
        let now = at(2025, 6, 30, 12).unwrap();
        let times = vec![at(2025, 6, 1, 0), at(2025, 6, 25, 0), at(2025, 6, 29, 0)];
        let policy = Policy {
            keep_last: 1,
            keep_within_days: Some(7),
            ..Default::default()
        };
        let r = decide(&times, &policy, now);
        assert!(r[0].is_empty());
        assert_eq!(r[1], vec!["recent"]);
        assert_eq!(r[2], vec!["last", "recent"]);
    }

    #[test]
    fn newest_is_kept_even_when_everything_is_old() {
        let now = at(2025, 6, 30, 12).unwrap();
        let times = vec![at(2020, 1, 1, 0), at(2020, 1, 2, 0)];
        let policy = Policy {
            keep_last: 1,
            keep_within_days: Some(7),
            ..Default::default()
        };
        let r = decide(&times, &policy, now);
        assert!(r[0].is_empty());
        assert_eq!(r[1], vec!["last"]);
    }

    #[test]
    fn grandfather_father_son() {
        // A backup every 6 hours for two years.
        let start = at(2023, 7, 1, 0).unwrap();
        let times: Vec<Option<DateTime<Utc>>> = (0..(2 * 365 * 4))
            .map(|i| Some(start + chrono::Duration::hours(6 * i)))
            .collect();
        let now = times.last().unwrap().unwrap() + chrono::Duration::hours(1);
        let policy = Policy {
            keep_last: 1,
            keep_within_days: None,
            keep_daily: 7,
            keep_weekly: 4,
            keep_monthly: 12,
            keep_yearly: 3,
        };
        let r = decide(&times, &policy, now);
        let kept: Vec<usize> = (0..times.len()).filter(|&i| !r[i].is_empty()).collect();

        // The newest backup is kept, as the latest of its day/week/month/year.
        let newest = times.len() - 1;
        assert!(r[newest].contains(&"last") && r[newest].contains(&"daily"));
        assert_eq!(r.iter().filter(|x| x.contains(&"daily")).count(), 7);
        assert_eq!(r.iter().filter(|x| x.contains(&"weekly")).count(), 4);
        assert_eq!(r.iter().filter(|x| x.contains(&"monthly")).count(), 12);
        assert_eq!(r.iter().filter(|x| x.contains(&"yearly")).count(), 3);
        // Overlaps mean fewer than 7 + 4 + 12 + 3 + 1 backups in total.
        assert!(kept.len() < 27 && kept.len() >= 12, "{}", kept.len());
        // Monthly backups really are one per month: 12 distinct months.
        let months: HashSet<(i32, u32)> = (0..times.len())
            .filter(|&i| r[i].contains(&"monthly"))
            .map(|i| {
                let t = times[i].unwrap().with_timezone(&Local);
                (t.year(), t.month())
            })
            .collect();
        assert_eq!(months.len(), 12);
        // The oldest yearly backup is from the first year.
        let oldest_kept = kept.iter().map(|&i| times[i].unwrap()).min().unwrap();
        assert_eq!(oldest_kept.with_timezone(&Local).year(), 2023);
    }

    #[test]
    fn unknown_dates_are_kept() {
        let now = at(2025, 6, 30, 12).unwrap();
        let r = decide(
            &[None, at(2025, 6, 29, 0)],
            &Policy {
                keep_last: 1,
                ..Default::default()
            },
            now,
        );
        assert_eq!(r[0], vec!["unknown date"]);
        assert_eq!(
            summarize(&r),
            "2 kept, 0 to delete (1 last, 1 unknown date)"
        );
    }
}
