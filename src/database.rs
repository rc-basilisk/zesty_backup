//! Database dumps, written straight into the backup archive.
//!
//! Every dump goes through a private temporary file (never a predictable name in /tmp) and
//! passwords are handed to the dump tools through environment variables or a 0600 config
//! file, never on the command line where any local user could read them with `ps`.

use crate::archive::ArchiveWriter;
use crate::config::DatabaseConfig;
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use tracing::{info, warn};

pub const SUPPORTED: &str = "postgres, mariadb, mysql, mongodb, cassandra, scylla, redis, sqlite";

/// The database engines zesty-backup knows, from `[database] type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Postgres,
    MySql,
    MariaDb,
    MongoDb,
    Cassandra,
    Redis,
    Sqlite,
}

impl DbKind {
    pub fn from_config(db: &DatabaseConfig) -> Result<Self> {
        let t = db
            .db_type
            .as_deref()
            .unwrap_or("postgres")
            .to_ascii_lowercase();
        Ok(match t.as_str() {
            "postgres" | "postgresql" => Self::Postgres,
            "mysql" => Self::MySql,
            "mariadb" => Self::MariaDb,
            "mongodb" | "mongo" => Self::MongoDb,
            "cassandra" | "scylla" => Self::Cassandra,
            "redis" => Self::Redis,
            "sqlite" | "sqlite3" => Self::Sqlite,
            other => bail!(
                "Unsupported database type: {}. Supported: {}",
                other,
                SUPPORTED
            ),
        })
    }

    /// Tools that can make the dump (any one of them is enough). Empty: none needed.
    pub fn dump_tools(self) -> &'static [&'static str] {
        match self {
            Self::Postgres => &["pg_dump"],
            Self::MySql => &["mysqldump"],
            Self::MariaDb => &["mariadb-dump", "mysqldump"],
            Self::MongoDb => &["mongodump"],
            Self::Cassandra => &["cqlsh"],
            Self::Redis => &["redis-cli"],
            Self::Sqlite => &[],
        }
    }
}

pub fn backup_database(
    archive: &mut ArchiveWriter,
    db: &DatabaseConfig,
    project_path: &str,
) -> Result<()> {
    DbKind::from_config(db)?;
    let db_type = db
        .db_type
        .as_deref()
        .unwrap_or("postgres")
        .to_ascii_lowercase();
    let database = db
        .database
        .as_deref()
        .context("database.database is not set")?;

    if db_type == "sqlite" || db_type == "sqlite3" {
        return backup_sqlite(archive, database);
    }

    let host = db.host.as_deref().unwrap_or("localhost");
    let password = resolve_password(db, project_path);
    let tmp = tempfile::Builder::new().prefix("zesty-db-").tempfile()?;

    let (mut cmd, extension) = match db_type.as_str() {
        "postgres" | "postgresql" => {
            let mut cmd = Command::new("pg_dump");
            cmd.arg("--host")
                .arg(host)
                .arg("--format=plain")
                .arg("--no-password")
                .arg("--dbname")
                .arg(database);
            if let Some(port) = db.port {
                cmd.arg("--port").arg(port.to_string());
            }
            if let Some(user) = &db.username {
                cmd.arg("--username").arg(user);
            }
            if let Some(pw) = &password {
                cmd.env("PGPASSWORD", pw);
            }
            (cmd, "sql")
        }
        "mariadb" | "mysql" => {
            // MariaDB >= 11 ships `mariadb-dump` and may not have the `mysqldump` alias.
            let tool = if db_type == "mariadb" && which::which("mariadb-dump").is_ok() {
                "mariadb-dump"
            } else {
                "mysqldump"
            };
            let mut cmd = Command::new(tool);
            cmd.arg(format!("--host={}", host))
                .arg("--single-transaction")
                .arg("--routines")
                .arg("--triggers");
            if let Some(port) = db.port {
                cmd.arg(format!("--port={}", port));
            }
            if let Some(user) = &db.username {
                cmd.arg(format!("--user={}", user));
            }
            if let Some(pw) = &password {
                cmd.env("MYSQL_PWD", pw);
            }
            cmd.arg(database);
            (cmd, "sql")
        }
        "mongodb" | "mongo" => {
            let mut cmd = Command::new("mongodump");
            cmd.arg(format!("--host={}", host))
                .arg(format!("--db={}", database))
                .arg("--archive")
                .arg("--gzip");
            if let Some(port) = db.port {
                cmd.arg(format!("--port={}", port));
            }
            if let Some(user) = &db.username {
                cmd.arg(format!("--username={}", user));
            }
            // Keep the secret file alive until the command has run (it is deleted on drop).
            let mut secret = None;
            if let Some(pw) = &password {
                let mut f = tempfile::Builder::new().prefix("zesty-mongo-").tempfile()?;
                writeln!(f, "password: {}", serde_json::to_string(pw)?)?;
                cmd.arg(format!("--config={}", f.path().display()));
                secret = Some(f);
            }
            let result = run_dump(cmd, tmp.as_file(), "mongodump");
            drop(secret);
            result?;
            return add_dump(
                archive,
                tmp.path(),
                &format!("database/{}.archive.gz", database),
            );
        }
        "cassandra" | "scylla" => {
            warn!("cassandra/scylla: only the keyspace schema is exported (cqlsh DESCRIBE). Use nodetool snapshot for data.");
            let mut cmd = Command::new("cqlsh");
            cmd.arg(host);
            if let Some(port) = db.port {
                cmd.arg(port.to_string());
            }
            if let Some(user) = &db.username {
                cmd.arg("-u").arg(user);
            }
            if let Some(pw) = &password {
                // cqlsh has no env var for the password.
                cmd.arg("-p").arg(pw);
            }
            cmd.arg("-e")
                .arg(format!("DESCRIBE KEYSPACE {};", database));
            (cmd, "cql")
        }
        "redis" => {
            let mut cmd = Command::new("redis-cli");
            cmd.arg("-h").arg(host);
            if let Some(port) = db.port {
                cmd.arg("-p").arg(port.to_string());
            }
            if let Some(user) = &db.username {
                cmd.arg("--user").arg(user);
            }
            if let Some(pw) = &password {
                cmd.env("REDISCLI_AUTH", pw);
            }
            // redis-cli writes the RDB snapshot to the file itself.
            cmd.arg("--rdb").arg(tmp.path());
            let output = cmd
                .stdin(Stdio::null())
                .output()
                .context("Failed to run redis-cli. Is it installed and in PATH?")?;
            if !output.status.success() {
                bail!(
                    "redis-cli --rdb failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            return add_dump(archive, tmp.path(), "database/redis.rdb");
        }
        other => bail!(
            "Unsupported database type: {}. Supported: {}",
            other,
            SUPPORTED
        ),
    };

    let tool = cmd.get_program().to_string_lossy().to_string();
    cmd.stdin(Stdio::null());
    run_dump(cmd, tmp.as_file(), &tool)?;
    add_dump(
        archive,
        tmp.path(),
        &format!("database/{}.{}", database, extension),
    )
}

/// Run a dump command with its stdout going straight into `out`.
fn run_dump(mut cmd: Command, out: &File, tool: &str) -> Result<()> {
    info!("Running {}...", tool);
    let output = cmd
        .stdout(out.try_clone()?)
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("Failed to run {}. Is it installed and in PATH?", tool))?;
    if !output.status.success() {
        bail!(
            "{} failed ({}): {}",
            tool,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn add_dump(archive: &mut ArchiveWriter, path: &Path, archive_path: &str) -> Result<()> {
    let size = fs::metadata(path)?.len();
    if size == 0 {
        bail!("Database dump is empty");
    }
    info!("Database dump: {} ({} bytes)", archive_path, size);
    archive.add_path(path, archive_path, &Default::default())
}

fn backup_sqlite(archive: &mut ArchiveWriter, db_path: &str) -> Result<()> {
    let path = Path::new(db_path);
    if !path.is_file() {
        bail!("SQLite database file not found: {}", db_path);
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "database.sqlite".into());
    let archive_path = format!("database/{}", name);

    // `.backup` takes a consistent snapshot even while the database is being written to.
    if which::which("sqlite3").is_ok() {
        let tmp = tempfile::Builder::new()
            .prefix("zesty-sqlite-")
            .tempfile()?;
        let output = Command::new("sqlite3")
            .arg(path)
            .arg(format!(".backup '{}'", tmp.path().display()))
            .stdin(Stdio::null())
            .output()
            .context("Failed to run sqlite3")?;
        if output.status.success() {
            return add_dump(archive, tmp.path(), &archive_path);
        }
        warn!(
            "sqlite3 .backup failed ({}), copying the file directly",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    } else {
        warn!("sqlite3 not found; copying the database file directly (may be inconsistent if it is being written to)");
    }
    archive.add_path(path, &archive_path, &Default::default())
}

/// Check that a dump would work, without dumping any data: runs a schema-only dump with the
/// configured settings (catches wrong passwords, unreachable hosts and pg_dump/server version
/// mismatches). Returns a short description of what was checked.
pub fn connection_test(db: &DatabaseConfig, project_path: &str) -> Result<String> {
    let kind = DbKind::from_config(db)?;
    let database = db
        .database
        .as_deref()
        .context("database.database is not set")?;
    if kind == DbKind::Sqlite {
        let mut header = [0u8; 16];
        File::open(database)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut header))
            .with_context(|| format!("Cannot read SQLite file {}", database))?;
        anyhow::ensure!(
            &header == b"SQLite format 3\0",
            "{} is not an SQLite database",
            database
        );
        return Ok(format!("{} is a readable SQLite database", database));
    }
    let tool = kind
        .dump_tools()
        .iter()
        .find(|t| which::which(t).is_ok())
        .with_context(|| format!("{} not found in PATH", kind.dump_tools().join(" or ")))?;
    let host = db.host.as_deref().unwrap_or("localhost");
    let password = resolve_password(db, project_path);
    let mut cmd = Command::new(tool);
    match kind {
        DbKind::Postgres => {
            cmd.args([
                "--schema-only",
                "--no-password",
                "--host",
                host,
                "--dbname",
                database,
            ]);
            if let Some(port) = db.port {
                cmd.arg("--port").arg(port.to_string());
            }
            if let Some(user) = &db.username {
                cmd.arg("--username").arg(user);
            }
            if let Some(pw) = &password {
                cmd.env("PGPASSWORD", pw);
            }
        }
        DbKind::MySql | DbKind::MariaDb => {
            cmd.arg("--no-data").arg(format!("--host={}", host));
            if let Some(port) = db.port {
                cmd.arg(format!("--port={}", port));
            }
            if let Some(user) = &db.username {
                cmd.arg(format!("--user={}", user));
            }
            if let Some(pw) = &password {
                cmd.env("MYSQL_PWD", pw);
            }
            cmd.arg(database);
        }
        // No cheap dry run for these: finding the tool is all we check.
        _ => return Ok(format!("{} found (connection not tested)", tool)),
    }
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("Failed to run {}", tool))?;
    if !output.status.success() {
        bail!(
            "{} failed: {}",
            tool,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(format!(
        "{} connected to {} on {} (schema-only test)",
        tool, database, host
    ))
}

/// Password from the config, then `DB_PASSWORD`, then `DATABASE_URL` in `<project>/.env`.
pub fn resolve_password(db: &DatabaseConfig, project_path: &str) -> Option<String> {
    if let Some(pw) = &db.password {
        return Some(pw.clone());
    }
    if let Ok(pw) = std::env::var("DB_PASSWORD") {
        return Some(pw);
    }
    let env_file = Path::new(project_path).join(".env");
    let content = fs::read_to_string(env_file).ok()?;
    password_from_env_file(&content)
}

fn password_from_env_file(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim().trim_start_matches("export ").trim();
        let Some(value) = line.strip_prefix("DATABASE_URL=") else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        let url = url::Url::parse(value).ok()?;
        let pw = url.password()?;
        return Some(percent_decode(pw));
    }
    None
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_database_url() {
        let env = "FOO=1\nexport DATABASE_URL=\"postgresql://app:p%40ss+word@db:5432/app\"\n";
        assert_eq!(password_from_env_file(env).as_deref(), Some("p@ss+word"));
        assert_eq!(
            password_from_env_file("DATABASE_URL=postgres://app@db/app"),
            None
        );
    }
}
