//! `zesty-backup drill`: prove that the latest backup can actually be restored.
//!
//! Downloads the newest remote backup (or a local one), decrypts and extracts it into a
//! temporary folder, checks that the expected files are there, loads the database dump into
//! a throwaway container and runs SQL checks against it, then runs user-defined commands.

use crate::archive;
use crate::backup::{self, human_size, REMOTE_PREFIX};
use crate::config::{AppConfig, DrillConfig};
use crate::database::DbKind;
use crate::encryption::{self, DecryptionKeys};
use crate::providers::StorageProvider;
use crate::report::{Report, Status, Timer};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// File in `local_backup_dir` holding the result of the last drill (shown by `status`).
pub const LAST_DRILL_FILE: &str = ".zesty-last-drill.json";

#[derive(Default)]
pub struct DrillOptions<'a> {
    /// Backup to test (default: the newest).
    pub backup: Option<&'a str>,
    /// Test a local backup instead of downloading from remote storage.
    pub local: bool,
    /// age private key, for encrypted backups.
    pub identity: Option<&'a Path>,
    /// Keep the restored files instead of deleting them.
    pub keep: bool,
}

pub struct DrillOutcome {
    pub report: Report,
    pub backup: Option<String>,
    /// Where the restored files are, when `keep` was set.
    pub kept_dir: Option<PathBuf>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct LastDrill {
    pub time: DateTime<Utc>,
    pub backup: Option<String>,
    pub passed: bool,
    pub summary: String,
    pub report: Report,
}

pub fn load_last(config: &AppConfig) -> Option<LastDrill> {
    let path = Path::new(&config.backup.local_backup_dir).join(LAST_DRILL_FILE);
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn save_last(config: &AppConfig, outcome: &DrillOutcome) {
    let last = LastDrill {
        time: Utc::now(),
        backup: outcome.backup.clone(),
        passed: outcome.report.passed(),
        summary: outcome.report.summary(),
        report: Report {
            checks: outcome.report.checks.clone(),
        },
    };
    let path = Path::new(&config.backup.local_backup_dir).join(LAST_DRILL_FILE);
    let result = fs::create_dir_all(&config.backup.local_backup_dir).and_then(|_| {
        fs::write(
            &path,
            serde_json::to_string_pretty(&last).unwrap_or_default(),
        )
    });
    if let Err(e) = result {
        warn!("Could not save drill result to {}: {}", path.display(), e);
    }
}

pub async fn run(
    config: &AppConfig,
    provider: Option<&dyn StorageProvider>,
    options: &DrillOptions<'_>,
) -> DrillOutcome {
    let default_drill = DrillConfig::default();
    let drill = config.drill.as_ref().unwrap_or(&default_drill);
    let mut report = Report::new();

    let work = match work_dir(drill) {
        Ok(dir) => dir,
        Err(e) => {
            report.fail("Work directory", format!("{:#}", e));
            return finish(config, report, None, None, options.keep);
        }
    };

    // 1. Pick the backup and check how fresh it is.
    let Some((name, created, size)) = pick_backup(config, provider, options, &mut report).await
    else {
        return finish(config, report, None, Some(work), options.keep);
    };
    let outcome_backup = Some(name.clone());
    check_freshness(drill, created, &mut report);

    // 2. Get the archive.
    let archive_path = if options.local {
        Path::new(&config.backup.local_backup_dir).join(&name)
    } else {
        let provider = provider.expect("remote drill needs a provider");
        let t = Timer::start();
        match backup::download(provider, &name, work.path()).await {
            Ok(path) => {
                let got = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if size.is_some_and(|s| s != got) {
                    report.fail(
                        "Download",
                        format!(
                            "got {} but storage lists {}",
                            human_size(got),
                            human_size(size.unwrap())
                        ),
                    );
                    return finish(config, report, outcome_backup, Some(work), options.keep);
                }
                report.pass(
                    "Download",
                    format!("{} in {}", human_size(got), t.elapsed()),
                );
                path
            }
            Err(e) => {
                report.fail("Download", format!("{:#}", e));
                return finish(config, report, outcome_backup, Some(work), options.keep);
            }
        }
    };

    // 3. Decrypt and extract.
    let keys = DecryptionKeys::new(options.identity, config.encryption.as_ref());
    let encrypted = encryption::is_encrypted(&archive_path).unwrap_or(false);
    if encrypted && keys.identity_files.is_empty() && keys.passphrase.is_none() {
        report
            .warn("Decrypt", "backup is encrypted and no key is available here: contents NOT checked")
            .hint("run the drill where the private key is (`--identity key.txt`), e.g. from your own machine");
        return finish(config, report, outcome_backup, Some(work), options.keep);
    }
    let restored = work.path().join("restored");
    let t = Timer::start();
    if let Err(e) = archive::extract(&archive_path, &restored, &keys) {
        report.fail(
            if encrypted {
                "Decrypt & extract"
            } else {
                "Extract"
            },
            format!("{:#}", e),
        );
        return finish(config, report, outcome_backup, Some(work), options.keep);
    }
    let files = list_files(&restored);
    let total: u64 = files
        .iter()
        .filter_map(|f| fs::metadata(restored.join(f)).ok())
        .map(|m| m.len())
        .sum();
    report.pass(
        if encrypted {
            "Decrypt & extract"
        } else {
            "Extract"
        },
        format!(
            "{}, {} in {}",
            plural(files.len(), "file"),
            human_size(total),
            t.elapsed()
        ),
    );

    // 4. Files.
    let project_files = files.iter().filter(|f| f.starts_with("project/")).count();
    if project_files > 0 {
        report.pass(
            "Project files",
            format!("{} under project/", plural(project_files, "file")),
        );
    } else {
        report
            .fail("Project files", "the backup contains no project files")
            .hint("check backup.project_path and the exclude patterns");
    }
    check_file_patterns(drill, &files, &mut report);

    // 5. Database.
    if let Some(db) = config
        .database
        .as_ref()
        .filter(|d| d.enabled.unwrap_or(false))
    {
        match DbKind::from_config(db) {
            Ok(kind) => check_database(drill, kind, &restored, &mut report),
            Err(e) => {
                report.fail("Database dump", format!("{:#}", e));
            }
        }
    } else if drill
        .database_checks
        .as_ref()
        .is_some_and(|c| !c.is_empty())
    {
        report.warn(
            "Database checks",
            "database_checks are set but [database] is not enabled",
        );
    }

    // 6. User commands.
    for cmd in drill.commands.iter().flatten() {
        run_command(
            cmd.name.as_deref().unwrap_or(&cmd.run),
            &cmd.run,
            &restored,
            &mut report,
        );
    }

    finish(config, report, outcome_backup, Some(work), options.keep)
}

fn finish(
    config: &AppConfig,
    report: Report,
    backup: Option<String>,
    work: Option<tempfile::TempDir>,
    keep: bool,
) -> DrillOutcome {
    let kept_dir = match work {
        Some(dir) if keep => Some(dir.keep()),
        _ => None, // dropping the TempDir deletes everything
    };
    let outcome = DrillOutcome {
        report,
        backup,
        kept_dir,
    };
    save_last(config, &outcome);
    outcome
}

fn work_dir(drill: &DrillConfig) -> Result<tempfile::TempDir> {
    let builder = {
        let mut b = tempfile::Builder::new();
        b.prefix("zesty-drill-");
        b
    };
    match &drill.work_dir {
        Some(dir) => {
            fs::create_dir_all(dir).with_context(|| format!("Cannot create {}", dir))?;
            builder
                .tempdir_in(dir)
                .with_context(|| format!("Cannot use {}", dir))
        }
        None => builder
            .tempdir()
            .context("Cannot create a temporary directory"),
    }
}

/// (name, creation time, size) of the backup to test.
async fn pick_backup(
    config: &AppConfig,
    provider: Option<&dyn StorageProvider>,
    options: &DrillOptions<'_>,
    report: &mut Report,
) -> Option<(String, Option<DateTime<Utc>>, Option<u64>)> {
    let wanted = options
        .backup
        .map(|b| b.strip_prefix(REMOTE_PREFIX).unwrap_or(b).to_string());
    let candidates: Vec<(String, Option<DateTime<Utc>>, Option<u64>)> = if options.local {
        match backup::local_backups(Path::new(&config.backup.local_backup_dir)) {
            Ok(paths) => paths
                .iter()
                .map(|p| {
                    let name = p.file_name().unwrap().to_string_lossy().to_string();
                    let created = backup::timestamp_from_name(&name);
                    (name, created, fs::metadata(p).ok().map(|m| m.len()))
                })
                .collect(),
            Err(e) => {
                report.fail("Find backup", format!("{:#}", e));
                return None;
            }
        }
    } else {
        let Some(provider) = provider else {
            report.fail("Find backup", "no storage provider");
            return None;
        };
        match backup::remote_backups(provider).await {
            Ok(items) => items
                .into_iter()
                .map(|i| {
                    let name = i.key[REMOTE_PREFIX.len()..].to_string();
                    let created = backup::timestamp_from_name(&name).or(i.last_modified);
                    (name, created, Some(i.size))
                })
                .collect(),
            Err(e) => {
                report.fail(
                    "Find backup",
                    format!("cannot list remote backups: {:#}", e),
                );
                return None;
            }
        }
    };
    let where_ = if options.local { "local" } else { "remote" };
    let chosen = match &wanted {
        Some(name) => candidates.into_iter().find(|c| &c.0 == name),
        None => candidates.into_iter().last(),
    };
    match chosen {
        Some(c) => {
            report.pass("Find backup", format!("{} ({})", c.0, where_));
            Some(c)
        }
        None => {
            match wanted {
                Some(name) => report.fail(
                    "Find backup",
                    format!("{} not found in {} backups", name, where_),
                ),
                None => report.fail("Find backup", format!("no {} backups at all", where_)),
            };
            None
        }
    }
}

fn check_freshness(drill: &DrillConfig, created: Option<DateTime<Utc>>, report: &mut Report) {
    let Some(created) = created else {
        report.warn("Freshness", "creation time unknown");
        return;
    };
    let hours = (Utc::now() - created).num_minutes() as f64 / 60.0;
    let age = format!("newest backup is {:.1}h old", hours);
    match drill.max_age_hours {
        Some(max) if hours > max as f64 => {
            report
                .fail("Freshness", format!("{} (limit {}h)", age, max))
                .hint("backups have stopped: check the daemon/cron job and its logs");
        }
        Some(max) => {
            report.pass("Freshness", format!("{} (limit {}h)", age, max));
        }
        None => {
            report.pass("Freshness", age);
        }
    }
}

/// All regular files under `root`, as `/`-separated relative paths.
fn list_files(root: &Path) -> Vec<String> {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_type().is_dir())
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect()
}

fn check_file_patterns(drill: &DrillConfig, files: &[String], report: &mut Report) {
    for pattern in drill.files.iter().flatten() {
        let matcher = match globset::GlobBuilder::new(pattern.trim_start_matches('/'))
            .literal_separator(true)
            .build()
        {
            Ok(g) => g.compile_matcher(),
            Err(e) => {
                report.fail("File check", format!("{}: invalid pattern: {}", pattern, e));
                continue;
            }
        };
        let n = files
            .iter()
            .filter(|f| matcher.is_match(f.as_str()))
            .count();
        if n > 0 {
            report.pass(
                "File check",
                format!(
                    "{} ({} match{})",
                    pattern,
                    n,
                    if n == 1 { "" } else { "es" }
                ),
            );
        } else {
            report
                .fail("File check", format!("{}: nothing matches", pattern))
                .hint("paths are inside the archive, e.g. project/<folder>/..., system/..., database/...");
        }
    }
}

fn run_command(name: &str, run: &str, restored: &Path, report: &mut Report) {
    let t = Timer::start();
    let output = Command::new("sh")
        .arg("-c")
        .arg(run)
        .current_dir(restored)
        .env("ZESTY_RESTORE_DIR", restored)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            report.pass("Command", format!("{} ({})", name, t.elapsed()));
        }
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if text.is_empty() {
                text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            report.fail(
                "Command",
                format!("{} failed ({}): {}", name, out.status, last_lines(&text, 3)),
            );
        }
        Err(e) => {
            report.fail("Command", format!("{}: cannot run sh: {}", name, e));
        }
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{} {}{}", n, word, if n == 1 { "" } else { "s" })
}

/// The most useful line of a database client's error output (the `ERROR: …` one).
fn error_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| l.contains("ERROR"))
        .or_else(|| stderr.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("unknown error")
        .to_string()
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join(" | ")
}

// ───────────────────────── database ─────────────────────────

fn find_dump(kind: DbKind, restored: &Path) -> Option<PathBuf> {
    let dir = restored.join("database");
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    let wanted: &[&str] = match kind {
        DbKind::Postgres | DbKind::MySql | DbKind::MariaDb => &[".sql"],
        DbKind::MongoDb => &[".archive.gz"],
        DbKind::Cassandra => &[".cql"],
        DbKind::Redis => &[".rdb"],
        DbKind::Sqlite => &[""],
    };
    entries
        .into_iter()
        .find(|p| wanted.iter().any(|ext| p.to_string_lossy().ends_with(ext)))
}

fn check_database(drill: &DrillConfig, kind: DbKind, restored: &Path, report: &mut Report) {
    let Some(dump) = find_dump(kind, restored) else {
        report
            .fail("Database dump", "no database dump in the backup")
            .hint("check the [database] section and the backup logs");
        return;
    };
    let size = fs::metadata(&dump).map(|m| m.len()).unwrap_or(0);
    let dump_name = dump
        .strip_prefix(restored)
        .unwrap_or(&dump)
        .display()
        .to_string();
    if size == 0 {
        report.fail("Database dump", format!("{} is empty", dump_name));
        return;
    }
    report.pass(
        "Database dump",
        format!("{} ({})", dump_name, human_size(size)),
    );

    let checks = drill.database_checks.clone().unwrap_or_default();
    match kind {
        DbKind::Postgres | DbKind::MySql | DbKind::MariaDb => {
            if drill.database_restore == Some(false) {
                report.skip(
                    "Database restore",
                    "disabled (drill.database_restore = false)",
                );
                return;
            }
            if !runtime_available(drill.runtime()) {
                let check = report.add(
                    "Database restore",
                    if drill.database_restore == Some(true) {
                        Status::Fail
                    } else {
                        Status::Warn
                    },
                    format!(
                        "{} is not available: dump not test-restored",
                        drill.runtime()
                    ),
                );
                check.hint("install Docker (or set container_runtime = \"podman\") to test-restore the dump");
                return;
            }
            let image = drill
                .database_image
                .clone()
                .unwrap_or_else(|| default_image(kind, &dump));
            let engine = if kind == DbKind::Postgres {
                Engine::Postgres
            } else {
                Engine::MySql
            };
            restore_in_container(drill.runtime(), engine, &image, &dump, &checks, report);
        }
        DbKind::Sqlite => sqlite_checks(&dump, &checks, report),
        other => {
            report.skip(
                "Database restore",
                format!(
                    "test restore not supported for {:?} yet; dump presence checked",
                    other
                ),
            );
            if !checks.is_empty() {
                report.warn(
                    "Database checks",
                    "not run: test restore not supported for this database type",
                );
            }
        }
    }
}

fn runtime_available(runtime: &str) -> bool {
    Command::new(runtime)
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Pick the image matching the server that made the dump, from the dump's header comments.
pub fn default_image(kind: DbKind, dump: &Path) -> String {
    let header = fs::read(dump)
        .map(|b| String::from_utf8_lossy(&b[..b.len().min(4096)]).to_string())
        .unwrap_or_default();
    image_from_header(kind, &header)
}

fn image_from_header(kind: DbKind, header: &str) -> String {
    match kind {
        DbKind::Postgres => header
            .lines()
            .find_map(|l| l.strip_prefix("-- Dumped from database version "))
            .and_then(|v| v.split(['.', ' ']).next())
            .filter(|major| major.chars().all(|c| c.is_ascii_digit()) && !major.is_empty())
            .map(|major| format!("postgres:{}-alpine", major))
            .unwrap_or_else(|| "postgres:17-alpine".into()),
        _ => {
            let version = header
                .lines()
                .find_map(|l| l.strip_prefix("-- Server version"))
                .map(|v| v.trim().trim_start_matches("5.5.5-").to_string())
                .unwrap_or_default();
            let major_minor: String = version.split('.').take(2).collect::<Vec<_>>().join(".");
            let numeric = !major_minor.is_empty()
                && major_minor.chars().all(|c| c.is_ascii_digit() || c == '.');
            let is_maria = kind == DbKind::MariaDb || version.contains("MariaDB");
            match (is_maria, numeric) {
                (true, true) => format!("mariadb:{}", major_minor),
                (true, false) => "mariadb:11".into(),
                (false, true) => format!("mysql:{}", major_minor),
                (false, false) => "mysql:8.4".into(),
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Engine {
    Postgres,
    MySql,
}

const DRILL_PASSWORD: &str = "zesty-drill";

/// A throwaway database container, removed when dropped.
struct Container {
    runtime: String,
    name: String,
}

impl Container {
    fn start(runtime: &str, image: &str, env: &[(&str, &str)]) -> Result<Self> {
        let name = format!(
            "zesty-drill-{}-{}",
            std::process::id(),
            Utc::now().timestamp_millis()
        );
        let mut cmd = Command::new(runtime);
        cmd.args(["run", "-d", "--name", &name]);
        for (k, v) in env {
            cmd.arg("-e").arg(format!("{}={}", k, v));
        }
        cmd.arg(image);
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .context("Failed to start container")?;
        // Remove it in any case from here on.
        let container = Self {
            runtime: runtime.to_string(),
            name,
        };
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let first = err
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("");
            let mut message = format!("{} run {} failed: {}", runtime, image, first);
            if err.contains("429") || err.contains("toomanyrequests") {
                message.push_str(" (Docker Hub rate limit: retry later, or `docker login`)");
            } else if err.contains("pull access denied") || err.contains("manifest unknown") {
                message.push_str(" (image not found: check drill.database_image)");
            }
            bail!("{}", message);
        }
        Ok(container)
    }

    fn exec(&self, env: &[(&str, &str)], args: &[&str], stdin: Option<&Path>) -> Result<Output> {
        let mut cmd = Command::new(&self.runtime);
        cmd.arg("exec");
        if stdin.is_some() {
            cmd.arg("-i");
        }
        for (k, v) in env {
            cmd.arg("-e").arg(format!("{}={}", k, v));
        }
        cmd.arg(&self.name).args(args);
        match stdin {
            Some(path) => cmd.stdin(File::open(path)?),
            None => cmd.stdin(Stdio::null()),
        };
        cmd.output()
            .context("Failed to run a command in the drill container")
    }

    fn wait_until(&self, timeout: Duration, mut ready: impl FnMut(&Self) -> bool) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if ready(self) {
                return Ok(());
            }
            // The container may have died (bad image, OOM...).
            let running = Command::new(&self.runtime)
                .args(["inspect", "-f", "{{.State.Running}}", &self.name])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
                .unwrap_or(false);
            if !running {
                let logs = Command::new(&self.runtime)
                    .args(["logs", "--tail", "5", &self.name])
                    .output();
                let logs = logs
                    .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                    .unwrap_or_default();
                bail!("database container stopped: {}", last_lines(logs.trim(), 3));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        bail!(
            "database did not become ready within {}s",
            timeout.as_secs()
        )
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(&self.runtime)
            .args(["rm", "-f", "-v", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn restore_in_container(
    runtime: &str,
    engine: Engine,
    image: &str,
    dump: &Path,
    checks: &[String],
    report: &mut Report,
) {
    let t = Timer::start();
    info!(
        "Starting {} for the database drill (the first run downloads the image)...",
        image
    );
    let maria = image.contains("mariadb");
    let client = if maria { "mariadb" } else { "mysql" };
    let pwd_env: [(&str, &str); 1] = [("MYSQL_PWD", DRILL_PASSWORD)];

    let container = match engine {
        Engine::Postgres => Container::start(
            runtime,
            image,
            &[
                ("POSTGRES_PASSWORD", DRILL_PASSWORD),
                ("POSTGRES_DB", "drill"),
            ],
        ),
        Engine::MySql => Container::start(
            runtime,
            image,
            &[
                ("MYSQL_ROOT_PASSWORD", DRILL_PASSWORD),
                ("MARIADB_ROOT_PASSWORD", DRILL_PASSWORD),
                ("MYSQL_DATABASE", "drill"),
                ("MARIADB_DATABASE", "drill"),
            ],
        ),
    };
    let container = match container {
        Ok(c) => c,
        Err(e) => {
            report.fail("Database restore", format!("{:#}", e));
            return;
        }
    };

    // Both images first run a setup server that is only reachable through the local socket;
    // checking over TCP waits for the real server.
    let ready = container.wait_until(Duration::from_secs(180), |c| match engine {
        Engine::Postgres => c
            .exec(
                &[],
                &[
                    "pg_isready",
                    "-h",
                    "127.0.0.1",
                    "-U",
                    "postgres",
                    "-d",
                    "drill",
                ],
                None,
            )
            .map(|o| o.status.success())
            .unwrap_or(false),
        Engine::MySql => c
            .exec(
                &pwd_env,
                &[client, "-h127.0.0.1", "-uroot", "-e", "SELECT 1"],
                None,
            )
            .map(|o| o.status.success())
            .unwrap_or(false),
    });
    if let Err(e) = ready {
        report.fail(
            "Database restore",
            format!("{} ({}): {:#}", image, runtime, e),
        );
        return;
    }

    let load = match engine {
        Engine::Postgres => {
            // pg_dump keeps object owners; create those roles so the restore matches.
            for role in roles_in_dump(dump) {
                let sql = format!("CREATE ROLE \"{}\"", role.replace('"', "\"\""));
                let _ = container.exec(
                    &[],
                    &["psql", "-U", "postgres", "-d", "drill", "-q", "-c", &sql],
                    None,
                );
            }
            container.exec(
                &[],
                &["psql", "-U", "postgres", "-d", "drill", "-q", "-f", "-"],
                Some(dump),
            )
        }
        Engine::MySql => container.exec(
            &pwd_env,
            &[client, "-h127.0.0.1", "-uroot", "--force", "drill"],
            Some(dump),
        ),
    };
    let output = match load {
        Ok(o) => o,
        Err(e) => {
            report.fail("Database restore", format!("{:#}", e));
            return;
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let errors: Vec<&str> = stderr.lines().filter(|l| l.contains("ERROR")).collect();

    let tables_sql = match engine {
        Engine::Postgres => "SELECT count(*) FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog','information_schema')",
        Engine::MySql => "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'drill'",
    };
    let query = |sql: &str| -> Result<String> {
        let out = match engine {
            Engine::Postgres => container.exec(
                &[],
                &["psql", "-U", "postgres", "-d", "drill", "-tA", "-c", sql],
                None,
            )?,
            Engine::MySql => container.exec(
                &pwd_env,
                &[
                    client,
                    "-h127.0.0.1",
                    "-uroot",
                    "-N",
                    "-B",
                    "drill",
                    "-e",
                    sql,
                ],
                None,
            )?,
        };
        if !out.status.success() {
            bail!("{}", error_line(&String::from_utf8_lossy(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string())
    };
    let tables = query(tables_sql).unwrap_or_default();

    let detail = format!(
        "{} table{} restored into {} in {}",
        tables,
        if tables == "1" { "" } else { "s" },
        image,
        t.elapsed()
    );
    if errors.is_empty() {
        if tables == "0" {
            report
                .warn("Database restore", detail)
                .hint("the dump restored but contains no tables");
        } else {
            report.pass("Database restore", detail);
        }
    } else {
        report
            .warn("Database restore", format!("{}, with {} error(s): {}", detail, errors.len(), errors.iter().take(2).copied().collect::<Vec<_>>().join(" | ")))
            .hint("errors during restore usually mean a missing extension or role, or an image older than the server; set drill.database_image");
    }

    for sql in checks {
        match query(sql) {
            Ok(value) if is_truthy(&value) => {
                report.pass("Database check", format!("{} → {}", sql, value));
            }
            Ok(value) => {
                report.fail(
                    "Database check",
                    format!(
                        "{} → {}",
                        sql,
                        if value.is_empty() { "(empty)" } else { &value }
                    ),
                );
            }
            Err(e) => {
                report.fail("Database check", format!("{} → error: {:#}", sql, e));
            }
        }
    }
}

/// Roles that own objects (`ALTER … OWNER TO x;`) or get privileges (`GRANT … TO x;`) in a pg_dump.
fn roles_in_dump(dump: &Path) -> BTreeSet<String> {
    let mut roles = BTreeSet::new();
    let Ok(content) = fs::read_to_string(dump) else {
        return roles;
    };
    for line in content.lines() {
        let names = if let Some(i) = line.find(" OWNER TO ") {
            &line[i + 10..]
        } else if line.starts_with("GRANT ") {
            match line.rfind(" TO ") {
                Some(i) => &line[i + 4..],
                None => continue,
            }
        } else {
            continue;
        };
        let names = names
            .trim_end_matches(';')
            .split(" WITH ")
            .next()
            .unwrap_or("");
        for name in names.split(',') {
            let name = name.trim().trim_matches('"');
            if !name.is_empty() && !name.eq_ignore_ascii_case("public") && name != "postgres" {
                roles.insert(name.to_string());
            }
        }
    }
    roles
}

/// A check passes when its first value is not empty, 0, false or NULL.
pub fn is_truthy(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    !(v.is_empty() || v == "0" || v == "f" || v == "false" || v == "null" || v == "0.0")
}

fn sqlite_checks(db: &Path, checks: &[String], report: &mut Report) {
    if which::which("sqlite3").is_err() {
        let header_ok = fs::read(db)
            .map(|b| b.starts_with(b"SQLite format 3\0"))
            .unwrap_or(false);
        if header_ok {
            report.warn(
                "Database restore",
                "valid SQLite header; install sqlite3 to run an integrity check",
            );
        } else {
            report.fail("Database restore", "the file is not an SQLite database");
        }
        if !checks.is_empty() {
            report.warn("Database checks", "not run: sqlite3 is not installed");
        }
        return;
    }
    let query = |sql: &str| -> Result<String> {
        let out = Command::new("sqlite3")
            .arg("-readonly")
            .arg(db)
            .arg(sql)
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            bail!("{}", error_line(&String::from_utf8_lossy(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string())
    };
    match query("PRAGMA integrity_check") {
        Ok(v) if v == "ok" => {
            report.pass("Database restore", "SQLite integrity_check: ok");
        }
        Ok(v) => {
            report.fail("Database restore", format!("SQLite integrity_check: {}", v));
        }
        Err(e) => {
            report.fail("Database restore", format!("{:#}", e));
        }
    }
    for sql in checks {
        match query(sql) {
            Ok(v) if is_truthy(&v) => {
                report.pass("Database check", format!("{} → {}", sql, v));
            }
            Ok(v) => {
                report.fail(
                    "Database check",
                    format!("{} → {}", sql, if v.is_empty() { "(empty)" } else { &v }),
                );
            }
            Err(e) => {
                report.fail("Database check", format!("{} → error: {:#}", sql, e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_error_line() {
        let psql = "ERROR:  relation \"orders\" does not exist\nLINE 1: SELECT count(*) FROM orders\n                             ^\n";
        assert_eq!(
            error_line(psql),
            "ERROR:  relation \"orders\" does not exist"
        );
        assert_eq!(error_line("\nsomething odd\n"), "something odd");
    }

    #[test]
    fn truthiness() {
        for v in ["1", "42", "t", "true", "abc"] {
            assert!(is_truthy(v), "{}", v);
        }
        for v in ["", "0", "f", "false", "NULL", " 0 "] {
            assert!(!is_truthy(v), "{:?}", v);
        }
    }

    #[test]
    fn images_from_dump_headers() {
        let pg = "--\n-- PostgreSQL database dump\n--\n\n-- Dumped from database version 16.4 (Ubuntu 16.4-1)\n";
        assert_eq!(
            image_from_header(DbKind::Postgres, pg),
            "postgres:16-alpine"
        );
        assert_eq!(
            image_from_header(DbKind::Postgres, ""),
            "postgres:17-alpine"
        );
        let my = "-- MySQL dump 10.13\n-- Server version\t8.0.36\n";
        assert_eq!(image_from_header(DbKind::MySql, my), "mysql:8.0");
        let maria = "-- Server version\t10.11.6-MariaDB-0+deb12u1\n";
        assert_eq!(image_from_header(DbKind::MySql, maria), "mariadb:10.11");
        assert_eq!(image_from_header(DbKind::MariaDb, ""), "mariadb:11");
    }

    #[test]
    fn finds_roles() {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("d.sql");
        fs::write(
            &dump,
            "ALTER TABLE public.users OWNER TO app;\nALTER SCHEMA s OWNER TO \"Weird Role\";\n\
             GRANT SELECT ON TABLE public.users TO reader, PUBLIC;\nGRANT ALL ON SCHEMA public TO postgres;\n",
        )
        .unwrap();
        let roles: Vec<String> = roles_in_dump(&dump).into_iter().collect();
        assert_eq!(roles, vec!["Weird Role", "app", "reader"]);
    }
}
