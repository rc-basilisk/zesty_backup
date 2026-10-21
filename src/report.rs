//! Pass/warn/fail check lists, shared by `doctor` and `drill`.

use std::fmt;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn symbol(self) -> &'static str {
        match self {
            Status::Pass => "✔",
            Status::Warn => "!",
            Status::Fail => "✖",
            Status::Skip => "-",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// What to do about a warning or failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: &str, status: Status, detail: impl Into<String>) -> &mut Check {
        self.checks.push(Check {
            name: name.to_string(),
            status,
            detail: detail.into(),
            hint: None,
        });
        self.checks.last_mut().unwrap()
    }

    pub fn pass(&mut self, name: &str, detail: impl Into<String>) -> &mut Check {
        self.add(name, Status::Pass, detail)
    }

    pub fn warn(&mut self, name: &str, detail: impl Into<String>) -> &mut Check {
        self.add(name, Status::Warn, detail)
    }

    pub fn fail(&mut self, name: &str, detail: impl Into<String>) -> &mut Check {
        self.add(name, Status::Fail, detail)
    }

    pub fn skip(&mut self, name: &str, detail: impl Into<String>) -> &mut Check {
        self.add(name, Status::Skip, detail)
    }

    /// Record the outcome of a fallible step: pass with `ok(value)` as detail, or fail with the error.
    pub fn record<T>(
        &mut self,
        name: &str,
        result: anyhow::Result<T>,
        ok: impl FnOnce(&T) -> String,
    ) -> Option<T> {
        match result {
            Ok(value) => {
                self.pass(name, ok(&value));
                Some(value)
            }
            Err(e) => {
                self.fail(name, format!("{:#}", e));
                None
            }
        }
    }

    pub fn count(&self, status: Status) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }

    /// True when nothing failed (warnings are allowed).
    pub fn passed(&self) -> bool {
        self.count(Status::Fail) == 0
    }

    pub fn summary(&self) -> String {
        let (fail, warn) = (self.count(Status::Fail), self.count(Status::Warn));
        match (fail, warn) {
            (0, 0) => "PASSED".to_string(),
            (0, w) => format!("PASSED with {} warning(s)", w),
            (f, w) => format!("FAILED ({} failed, {} warning(s))", f, w),
        }
    }
}

impl Check {
    pub fn hint(&mut self, hint: impl Into<String>) -> &mut Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self
            .checks
            .iter()
            .map(|c| c.name.chars().count())
            .max()
            .unwrap_or(0);
        for check in &self.checks {
            writeln!(
                f,
                "  {} {:width$}  {}",
                check.status.symbol(),
                check.name,
                check.detail,
                width = width
            )?;
            if let Some(hint) = &check.hint {
                writeln!(f, "    {:width$}  → {}", "", hint, width = width)?;
            }
        }
        write!(f, "Result: {}", self.summary())
    }
}

/// "1.2s", "3m 04s"…
pub fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Measure how long a step takes.
pub struct Timer(Instant);

impl Timer {
    pub fn start() -> Self {
        Self(Instant::now())
    }
    pub fn elapsed(&self) -> String {
        human_duration(self.0.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_and_display() {
        let mut r = Report::new();
        r.pass("Download", "1 MiB");
        assert_eq!(r.summary(), "PASSED");
        r.warn("Age", "old").hint("check the daemon");
        assert!(r.passed());
        r.fail("Restore", "boom");
        assert!(!r.passed());
        assert_eq!(r.summary(), "FAILED (1 failed, 1 warning(s))");
        let text = r.to_string();
        assert!(text.contains("✖ Restore   boom"), "{}", text);
        assert!(text.contains("→ check the daemon"));
        assert_eq!(human_duration(Duration::from_secs(185)), "3m 05s");
    }
}
