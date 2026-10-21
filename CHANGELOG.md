# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`zesty-backup doctor`** checks the whole setup without making a backup:
  - paths;
  - encryption, with an encrypt/decrypt round trip; `--identity` proves the
    private key matches;
  - database access, with a schema-only dump using the real settings;
  - commands and units;
  - Docker for drills;
  - remote backup freshness;
  - an upload/list/download/delete round trip on the real storage.

  Failures come with hints, and the exit code is 1 when something fails.
- **`zesty-backup drill`** runs restore drills:
  - downloads the newest remote backup (or `--local`, or a named one);
  - checks freshness (`max_age_hours`);
  - decrypts and extracts it into a temporary folder;
  - checks that the expected files exist (`files` globs);
  - loads the PostgreSQL/MySQL/MariaDB dump into a throwaway Docker/Podman
    container, picking the image from the dump's server version, and runs
    `database_checks` SQL queries;
  - checks SQLite with `PRAGMA integrity_check`;
  - runs your own `commands`.

  The result is saved and shown by `status`.
- `[drill] interval_hours` lets the daemon run drills on a schedule.

## [2.0.0] - 2026-10-13

A full audit. Several bugs made backups unusable or uploads fail, so upgrading
is strongly recommended.

### Fixed: backups
- **Every archive was truncated.** The zstd stream was never finalized, so
  extracting failed with "unexpected end of data". Backups made with 1.0.x
  cannot be restored and should be redone.
- Files were stored with permissions `000` and a 1970 timestamp. Permissions,
  mtimes, symlinks and empty directories are now preserved.
- Whole files were loaded into memory. They are now streamed.
- `exclude` patterns such as `*.log` never matched, and excluded directories
  were still walked. Glob and name patterns now work as documented.
- `local_backup_dir` inside `project_path` was backed up into itself.
- An interrupted backup left a corrupt archive behind, which was then uploaded.
  Archives are now written to `.partial` and renamed once complete.
- Archives were world-readable. They are now created with mode `0600`.
- `restore` needed the external `tar` and `zstd` tools. It is now built in and
  refuses entries that would escape the target folder.

### Fixed: databases
- Passwords were visible in `ps` for MySQL, MongoDB and Redis.
- MongoDB dumps were thrown away, and Redis dumps were overwritten by the CLI
  output.
- SQLite required a host, port and password, and failed on absolute paths.
- Dumps went through a predictable file name in `/tmp`.

### Fixed: providers
- **S3:**
  - An empty endpoint broke AWS.
  - Uploads over 5 GB failed. Multipart upload is now used for large files.
  - Recent AWS SDK checksum headers broke R2, MinIO and others.
  - MinIO and Contabo need path-style URLs.
- **GCS:** `credentials_path` was ignored.
- **B2:**
  - File names were base64-encoded instead of percent-encoded.
  - The v2 API is gone; the code now uses v4.
  - Uploads over 5 GB failed. The large-file API is now used.
  - Tokens expired after 24h.
  - `delete` could remove the wrong file.
- **Google Drive, OneDrive, Dropbox, Box, pCloud:** listing never returned
  anything, so `list --remote` and `clean` did nothing. Other bugs:
  - OneDrive's upload URL was wrong.
  - Google Drive's multipart upload format was wrong.
  - Box listings had no sizes or dates.
  - pCloud "download" asked pCloud to fetch a URL instead of downloading.
  - Size limits (150 MB Dropbox, 250 MB OneDrive, 50 MB Box) caused failures;
    chunked upload sessions are now used.
  - Pagination was missing.
- **MEGA:** used `mega-cmd put`, which doesn't exist (the tools are `mega-put`,
  `mega-ls`, …), and parsed the wrong `mega-ls` column.

### Fixed: CLI and daemon
- The daemon waited a full interval before its first backup, never cleaned old
  backups, panicked on a 0 interval, and ignored `incremental_per_day` and
  `upload_interval_hours`.
- `upload` re-uploaded every backup each time. It now skips backups already in
  cloud storage.
- `clean` deleted every file in the backup folder, not just backups. It could
  also delete the only remaining backup.
- `logs` read a file that was never written. `[logging]` now works.
- `generate-config` wrote `crontab_user = null`, which is invalid TOML.
- Consumer providers no longer require a `bucket`.

### Added
- **Optional encryption** with [age](https://age-encryption.org) (`[encryption]`):
  - Use public-key recipients (recommended) or a passphrase.
  - Encrypted archives (`.tar.zst.age`) can also be decrypted with the `age`
    CLI.
  - `restore` and `verify` take `--identity`.
- OAuth refresh tokens (`refresh_token`, `client_id`, `client_secret`,
  `token_cache`) for Google Drive, OneDrive, Dropbox and Box, for unattended
  use.
- `verify` command.
- `clean --local-only`.
- `ZESTY_BACKUP_CONFIG` environment variable.
- `client` options for every provider.
- S3 without keys falls back to the AWS credential chain (IAM roles).
- Config typos are now reported as errors.
- Real tests: backup/restore round trip, exclusion, upload/list/download/clean.

### Changed
- The code is now a library plus a thin CLI. Unused dependencies were removed,
  and OpenSSL is no longer needed.
- Minimum Rust version is 1.88. The Docker image was updated to match.
- `daemon --pid-file` is optional and has no default.
- `client --config` is now the global `--config`.
- Consumer drives store archives directly in the configured folder.
- Rewrote the README and the example config.

## [1.0.2] - 2025-11-19

### Fix formatting

## [1.0.1] - 2025-11-19

### Fixed
- Fixed AWS SDK S3 behavior version configuration issue that caused panics when listing backups
- Updated GitHub Actions workflows to use `actions/upload-artifact@v4` (fixes deprecation warnings)

## [1.0.0] - 2025-11-19

### Added
- Initial release
- Support for multiple cloud storage providers:
  - S3-compatible providers (AWS S3, Contabo, DigitalOcean Spaces, Wasabi, MinIO, Cloudflare R2)
  - Enterprise providers (Google Cloud Storage, Azure Blob Storage, Backblaze B2)
  - Consumer-grade providers (Google Drive, OneDrive, Dropbox, Box, pCloud, MEGA)
- Flexible TOML-based configuration
- Incremental backup support with configurable frequency
- Automatic cloud upload with scheduling
- Retention policies for local and remote backups
- zstd compression with configurable levels (0-22)
- Database backup support (PostgreSQL, MariaDB, MySQL, MongoDB, Cassandra, Scylla, Redis, SQLite)
- Systemd service and timer backup
- Command output capture for general command backups
- Preset configurations for common scenarios (Nginx, Crontab, user configs, /etc files)
- Client mode for downloading and restoring backups
- Daemon mode for background service operation

### Security
- Credentials can be provided via config file or environment variables
- Config files with credentials are excluded from version control

