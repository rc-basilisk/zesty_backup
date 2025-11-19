# Zesty Backup

[![CI](https://github.com/rc-basilisk/zesty-backup/workflows/CI/badge.svg)](https://github.com/rc-basilisk/zesty-backup/actions)
[![Crates.io](https://img.shields.io/crates/v/zesty-backup.svg)](https://crates.io/crates/zesty-backup)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

A flexible, multi-provider backup utility for cloud storage, written in Rust. Supports multiple cloud storage providers including AWS S3, Google Cloud Storage, Azure Blob Storage, Backblaze B2, and S3-compatible services.

## Features

- ✅ **Multiple Storage Providers**: AWS S3, Google Cloud Storage, Azure Blob Storage, Backblaze B2, and S3-compatible services (Contabo, DigitalOcean Spaces, Wasabi, MinIO, Cloudflare R2)
- ✅ **Flexible Configuration**: Easy-to-use TOML configuration file
- ✅ **Incremental Backups**: Efficient local backups with configurable frequency
- ✅ **Automatic Cloud Upload**: Scheduled uploads to your chosen cloud provider
- ✅ **Retention Policies**: Automatic cleanup of old backups (local and remote)
- ✅ **High Compression**: zstd compression with configurable levels (0-22)
- ✅ **Database Backup**: Optional PostgreSQL database backup
- ✅ **Systemd Integration**: Optional backup of systemd services and timers
- ✅ **Client Mode**: Download and restore backups from any machine
- ✅ **Daemon Mode**: Run as a background service with scheduled backups

## Supported Providers

### S3-Compatible Providers (Fully Supported)
- **AWS S3**: Set `provider = "aws"` or `provider = "s3"`
- **Contabo Object Storage**: Set `provider = "contabo"`
- **DigitalOcean Spaces**: Set `provider = "digitalocean"`
- **Wasabi**: Set `provider = "wasabi"` ✅ Fully supported (S3-compatible)
- **MinIO**: Set `provider = "minio"`
- **Cloudflare R2**: Set `provider = "r2"`

### Enterprise Cloud Storage
- **Backblaze B2**: Set `provider = "b2"` or `provider = "backblaze"` ✅ Fully supported
- **Google Cloud Storage**: Set `provider = "gcs"` or `provider = "google"` ✅ Fully supported
- **Azure Blob Storage**: Set `provider = "azure"` ✅ Fully supported

### Consumer-Grade Cloud Storage (Fully Supported)
- **Google Drive**: Set `provider = "googledrive"` or `provider = "gdrive"` ✅ Fully supported
- **OneDrive**: Set `provider = "onedrive"` ✅ Fully supported
- **Dropbox**: Set `provider = "dropbox"` ✅ Fully supported
- **Box**: Set `provider = "box"` ✅ Fully supported
- **pCloud**: Set `provider = "pcloud"` ✅ Fully supported
- **MEGA**: Set `provider = "mega"` ✅ Fully supported (requires MEGAcmd)

> **Note**: 
> - Consumer-grade providers require OAuth2 access tokens (see configuration examples)
> - MEGA requires MEGAcmd to be installed (handles client-side encryption automatically)
> - GCS requires service account credentials (see configuration examples)
> - Azure requires storage account name and access key (see configuration examples)

## Installation

### Prerequisites

- Rust 1.70+ (install from [rustup.rs](https://rustup.rs/))
- For PostgreSQL backups: `pg_dump` installed
- For MEGA provider: [MEGAcmd](https://mega.nz/cmd) installed and in PATH
- For GCS provider: Service account credentials (JSON key file) from [Google Cloud Console](https://console.cloud.google.com/)
- For Azure provider: Storage account name and access key from [Azure Portal](https://portal.azure.com/)

### Build from Source

```bash
git clone https://github.com/rc-basilisk/zesty-backup.git
cd zesty-backup
cargo build --release
```

The binary will be at `target/release/zesty-backup`.

### Install System-Wide (Optional)

```bash
sudo cp target/release/zesty-backup /usr/local/bin/zesty-backup
```

## Quick Start

### 1. Generate Example Configuration

```bash
zesty-backup generate-config
```

This creates `config.toml.example` with all available options.

### 2. Configure Your Backup

Copy the example config and edit it:

```bash
cp config.toml.example config.toml
nano config.toml
```

**Minimum required configuration:**
- Set your storage provider
- Configure provider credentials
- Set `project_path` to the directory you want to backup
- Set `local_backup_dir` for local backup storage

### 3. Create Your First Backup

```bash
zesty-backup backup
```

### 4. Upload to Cloud Storage

```bash
zesty-backup upload
```

## Usage

### Basic Commands

```bash
# Create an incremental backup
zesty-backup backup

# Create a full backup
zesty-backup backup --full

# List local backups
zesty-backup list

# List remote backups
zesty-backup list --remote

# Upload backups to cloud storage
zesty-backup upload

# Upload a specific backup file
zesty-backup upload --file ./backups/backup-20240101-120000.tar.zst

# Download a backup from cloud storage
zesty-backup download backup-20240101-120000.tar.zst --output ./restored

# Clean old backups (dry run)
zesty-backup clean --dry-run

# Clean old backups (actually delete)
zesty-backup clean

# Restore from a backup file
zesty-backup restore ./backups/backup-20240101-120000.tar.zst --target /path/to/restore

# Show backup system status
zesty-backup status

# Show recent logs
zesty-backup logs

# Generate example configuration
zesty-backup generate-config
```

### Daemon Mode

Run as a background service with automatic scheduled backups:

```bash
zesty-backup daemon \
  --backup-interval 6 \
  --upload-interval 24 \
  --pid-file /var/run/zesty-backup.pid
```

### Client Mode (Desktop Access)

Access your backups from any machine without a full config file:

```bash
# List remote backups
zesty-backup client \
  --provider s3 \
  --endpoint https://s3.amazonaws.com \
  --region us-east-1 \
  --bucket my-backups \
  --access-key YOUR_KEY \
  --secret-key YOUR_SECRET \
  list

# Download a backup
zesty-backup client \
  --provider s3 \
  --endpoint https://s3.amazonaws.com \
  --region us-east-1 \
  --bucket my-backups \
  --access-key YOUR_KEY \
  --secret-key YOUR_SECRET \
  download backup-20240101-120000.tar.zst \
  --output ./restored
```

Or use a config file:

```bash
zesty-backup client --config config.toml list
```

## Configuration

### Obtaining OAuth2 Tokens for Consumer-Grade Providers

Consumer-grade providers (Google Drive, OneDrive, Dropbox, Box) require OAuth2 access tokens:

- **Google Drive**: Create a project in [Google Cloud Console](https://console.cloud.google.com/), enable Drive API, create OAuth2 credentials, and use the access token
- **OneDrive**: Register an app in [Azure Portal](https://portal.azure.com/), get OAuth2 token via Microsoft Graph API
- **Dropbox**: Create an app in [Dropbox App Console](https://www.dropbox.com/developers/apps), generate access token
- **Box**: Create an app in [Box Developer Console](https://developer.box.com/), get OAuth2 token
- **pCloud**: Go to [pCloud API Keys](https://my.pcloud.com/#page=apikeys), create an API key, and use it as the access token

> **Note**: Access tokens expire. For production use, implement token refresh or use long-lived tokens where available.

### Storage Configuration Examples

#### AWS S3

```toml
[storage]
provider = "aws"
region = "us-east-1"
bucket = "my-backups"
access_key = "AKIAIOSFODNN7EXAMPLE"
secret_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
# endpoint can be omitted for AWS
```

#### Google Cloud Storage

Requires service account credentials. Get them from [Google Cloud Console](https://console.cloud.google.com/).

```toml
[storage]
provider = "gcs"  # or "google"
bucket = "my-backups"
credentials_path = "/path/to/service-account-key.json"  # Optional: uses GOOGLE_APPLICATION_CREDENTIALS env var if not set
```

> **Note**: You can either:
> - Set `credentials_path` in the config file, or
> - Set the `GOOGLE_APPLICATION_CREDENTIALS` environment variable to point to your service account JSON key file, or
> - Use default credentials from `gcloud` if you're running on a GCP instance

#### Azure Blob Storage

Requires storage account name and access key. Get them from [Azure Portal](https://portal.azure.com/).

```toml
[storage]
provider = "azure"
account_name = "mystorageaccount"
account_key = "your-account-key"  # Optional: can also use AZURE_STORAGE_ACCOUNT_KEY env var
bucket = "my-container"  # Azure uses "container" instead of "bucket"
```

> **Note**: You can either:
> - Set `account_key` in the config file, or
> - Set the `AZURE_STORAGE_ACCOUNT_KEY` environment variable
> 
> Azure Blob Storage uses "containers" instead of "buckets", but the config uses `bucket` for consistency with other providers.

#### Backblaze B2

```toml
[storage]
provider = "b2"
account_id = "your-account-id"
application_key = "your-application-key"
bucket_id = "your-bucket-id"
bucket = "my-backups"
```

#### Google Drive

Requires OAuth2 access token. Get one from [Google Cloud Console](https://console.cloud.google.com/).

```toml
[storage]
provider = "googledrive"  # or "gdrive"
access_key = "ya29.a0AfH6SMC..."  # OAuth2 access token
bucket_id = "folder-id-here"  # Optional: Google Drive folder ID (defaults to root)
```

#### OneDrive

Requires OAuth2 access token. Get one from [Azure Portal](https://portal.azure.com/).

```toml
[storage]
provider = "onedrive"
access_key = "eyJ0eXAiOiJKV1QiLCJub..."  # OAuth2 access token
bucket_id = "/drive/root:/Backups"  # Optional: folder path (defaults to root)
```

#### Dropbox

Requires access token. Get one from [Dropbox App Console](https://www.dropbox.com/developers/apps).

```toml
[storage]
provider = "dropbox"
access_key = "sl.Bk..."  # Dropbox access token
bucket_id = "/Backups"  # Optional: folder path (defaults to root)
```

#### Box

Requires OAuth2 access token. Get one from [Box Developer Console](https://developer.box.com/).

```toml
[storage]
provider = "box"
access_key = "T9cE5asOhuy8CC6..."  # OAuth2 access token
bucket_id = "123456789"  # Optional: folder ID (defaults to root folder "0")
```

#### pCloud

Requires API access token. Get one from [pCloud API Keys](https://my.pcloud.com/#page=apikeys).

```toml
[storage]
provider = "pcloud"
access_key = "your-api-access-token"  # Get from https://my.pcloud.com/#page=apikeys
region = "us"  # "us" for US data center (default) or "eu" for European data center
bucket_id = "/Backups"  # Optional: folder path (defaults to root "/")
```

> **Note**: pCloud has two data centers (US and EU). Set `region = "eu"` if your account is in the European data center. The provider automatically uses the correct API endpoint.

#### MEGA

Uses MEGAcmd (official MEGA command-line tool) which handles client-side encryption automatically.

**Prerequisites**: Install MEGAcmd from [https://mega.nz/cmd](https://mega.nz/cmd)

```toml
[storage]
provider = "mega"
account_name = "your-email@example.com"  # MEGA email
account_key = "your-password"  # MEGA password
bucket_id = "/Backups"  # Optional: folder path (defaults to root "/")
```

> **Note**: MEGA uses client-side encryption, which MEGAcmd handles automatically. The provider will automatically log in using your credentials and manage the encryption/decryption process.

#### DigitalOcean Spaces

```toml
[storage]
provider = "digitalocean"
endpoint = "https://nyc3.digitaloceanspaces.com"
region = "nyc3"
bucket = "my-backups"
access_key = "your-spaces-key"
secret_key = "your-spaces-secret"
```

#### Contabo Object Storage

```toml
[storage]
provider = "contabo"
endpoint = "https://eu2.contabostorage.com"
region = "eu2"
bucket = "my-backups"
access_key = "your-access-key"
secret_key = "your-secret-key"
```

### Backup Configuration

```toml
[backup]
# Local directory for storing backups
local_backup_dir = "./backups"

# Main directory to backup
project_path = "/var/www/myapp"

# Additional files/directories to include
additional_paths = [
    "/etc/nginx/nginx.conf",
    "/etc/nginx/sites-available/myapp",
]

# Number of incremental backups per day
incremental_per_day = 4

# Upload interval in hours
upload_interval_hours = 24

# Retention period in days
retention_days = 7

# Compression level (0-22)
# 0 = no compression, 3 = balanced, 22 = maximum
compression_level = 3

# Paths to exclude (supports patterns)
exclude = [
    "node_modules",
    ".git",
    "*.log",
]
```

### Database Backup (Optional)

Supports multiple database types: `postgres`, `mariadb`, `mysql`, `mongodb`, `cassandra`, `scylla`, `redis`, `sqlite`

```toml
[database]
enabled = true
type = "postgres"  # postgres, mariadb, mysql, mongodb, cassandra, scylla, redis, sqlite
host = "localhost"
port = 5432
database = "myapp_db"
username = "myuser"
password = "your_password"  # Optional: can also use DB_PASSWORD env var or .env file
```

### System Configuration

#### Systemd Services and Timers

```toml
[system]
systemd_services = [
    "myapp.service",
    "myapp-worker.service",
]

systemd_timers = [
    "myapp-cleanup.timer",
]
```

#### Command Outputs (General Pattern)

Backup the output of any command as a text file. This is a general pattern that works for any command:

```toml
[system]
command_outputs = [
    { command = "docker", args = ["ps", "-a"], output_file = "docker_containers.txt", enabled = true },
    { command = "systemctl", args = ["list-units", "--type=service"], output_file = "systemd_services.txt", enabled = true },
    { command = "dpkg", args = ["-l"], output_file = "installed_packages.txt", enabled = true },
]
```

#### Presets for Common Scenarios

Quick configuration presets for common backup needs:

```toml
[system.presets]
# Nginx configuration
nginx_enabled = true  # Backs up /etc/nginx/nginx.conf and sites-available/enabled
nginx_sites = [
    "example.com",
    "another-site.com",
]

# Crontab
crontab_enabled = true
crontab_user = null  # null = current user, or specify like "www-data"

# User config files (from home directory)
user_configs = [
    ".zshrc",
    ".bashrc",
    ".vimrc",
    ".gitconfig",
]
user_configs_home = null  # null = $HOME, or specify path

# Common /etc files and directories
etc_files = [
    "hosts",
    "fstab",
]
etc_dirs = [
    "ssl",
    "letsencrypt",
]
```

## Systemd Service Setup

Create a systemd service file at `/etc/systemd/system/zesty-backup.service`:

```ini
[Unit]
Description=Zesty Backup Service
After=network.target

[Service]
Type=simple
User=your-user
WorkingDirectory=/opt/zesty-backup
ExecStart=/usr/local/bin/zesty-backup daemon \
  --backup-interval 6 \
  --upload-interval 24 \
  --pid-file /var/run/zesty-backup.pid \
  --config /opt/zesty-backup/config.toml
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
sudo systemctl enable zesty-backup
sudo systemctl start zesty-backup
```

## What Gets Backed Up

- **Project Directory**: Everything in `project_path` (respects `exclude` patterns)
- **Additional Paths**: Files and directories listed in `additional_paths`
- **Systemd Services/Timers**: If configured in `[system.systemd_services]` and `[system.systemd_timers]`
- **Database**: If enabled in `[database]` (supports postgres, mariadb, mysql, mongodb, cassandra, scylla, redis, sqlite)
- **Command Outputs**: Text dumps of any command output (configured in `[system.command_outputs]`)
- **Nginx Configs**: If `nginx_enabled = true` in presets
- **Crontab**: If `crontab_enabled = true` in presets
- **User Config Files**: Files from home directory (configured in `[system.presets.user_configs]`)
- **System Files**: Files and directories from `/etc/` (configured in presets)

## Compression

Zesty Backup uses zstd compression with configurable levels:

- **Level 0**: No compression (fastest)
- **Level 3**: Balanced (recommended default)
- **Level 22**: Maximum compression (slowest, best space savings)

Choose based on your priorities: speed vs. storage space.

## Security

- **Credentials**: Never commit `config.toml` with real credentials to version control
- **Environment Variables**: Use `DB_PASSWORD` environment variable for database passwords
- **File Permissions**: Ensure `config.toml` has restrictive permissions: `chmod 600 config.toml`
- **IAM Roles**: For cloud providers, prefer IAM roles over access keys when possible

## Troubleshooting

### Backup Fails

- Check that `project_path` exists and is readable
- Verify storage provider credentials are correct
- Check disk space in `local_backup_dir`
- Review logs: `zesty-backup logs`

### Upload Fails

- Verify network connectivity
- Check storage provider credentials and permissions
- Ensure bucket/container exists and is accessible
- Check provider-specific requirements (e.g., B2 requires bucket_id)

### Database Backup Fails

- Ensure the appropriate dump tool is installed (`pg_dump`, `mysqldump`, `mongodump`, etc.) and in PATH
- Verify database password is set (in config, `DB_PASSWORD` env var, or `.env` file)
- Check database connection settings (host, port, username)
- Ensure database user has backup permissions
- For SQLite: ensure the database file path is correct and accessible

## Contributing

Contributions welcome! Open an issue or submit a PR. Keep it simple - format code with `cargo fmt`, run `cargo clippy`, and test your changes.

## License

MIT License - do what you want with it.

## Docker Support

### Using Docker

```bash
# Build the image
docker build -t zesty-backup .

# Run a backup
docker run --rm -v $(pwd)/config:/app/config:ro -v $(pwd)/backups:/app/backups zesty-backup backup

# Run as daemon
docker-compose up -d
```

See `Dockerfile` and `docker-compose.yml` for more details.

## Development

### Running Tests

```bash
# Run all tests
cargo test

# Run specific test suite
cargo test --test integration_test
cargo test --test provider_test
cargo test --test backup_test
```

### Examples

See the `examples/` directory for usage examples.

## Acknowledgments

- Built with [Rust](https://www.rust-lang.org/)
- Uses [object_store](https://crates.io/crates/object_store) for unified cloud storage access
- Inspired by the need for flexible, multi-provider backup solutions

## Support

- **Issues**: [GitHub Issues](https://github.com/rc-basilisk/zesty-backup/issues)
- **Documentation**: See this README and inline code documentation
