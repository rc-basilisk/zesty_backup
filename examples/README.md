# Examples

This directory contains example usage of zesty-backup.

## Running Examples

Examples can be run using Cargo:

```bash
# Run a specific example
cargo run --example basic-backup

# Build all examples
cargo build --examples
```

## Available Examples

### basic-backup.rs
Demonstrates creating a backup and uploading it to cloud storage.

**Note**: Examples require a valid `config.toml` file with proper credentials.

## Example Configurations

See `../config.example.toml` for complete configuration examples for all providers.

## Example Usage Scenarios

### Scenario 1: Simple Backup
```bash
# Create a backup
zesty-backup backup

# Upload to cloud
zesty-backup upload
```

### Scenario 2: Full Backup
```bash
# Create a full backup
zesty-backup backup --full
```

### Scenario 3: Client Mode (Download)
```bash
# List remote backups
zesty-backup client --provider s3 --bucket my-bucket --access-key KEY --secret-key SECRET list

# Download a backup
zesty-backup client --provider s3 --bucket my-bucket --access-key KEY --secret-key SECRET download --key backup-20240101.tar.zst --output ./restored-backup.tar.zst
```

### Scenario 4: Daemon Mode
```bash
# Run as daemon with 6-hour backup interval and 24-hour upload interval
zesty-backup daemon --backup-interval 6 --upload-interval 24 --pid-file /var/run/zesty-backup.pid
```

