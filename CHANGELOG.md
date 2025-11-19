# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

