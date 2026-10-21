# Zesty Backup

[![CI](https://github.com/rc-basilisk/zesty_backup/actions/workflows/ci.yml/badge.svg)](https://github.com/rc-basilisk/zesty_backup/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Back up a server (a project folder, config files, a database) into compressed
`.tar.zst` archives, then send them to cloud storage: S3 and S3-compatible
services, Google Cloud Storage, Azure, Backblaze B2, Google Drive, OneDrive,
Dropbox, Box, pCloud or MEGA.

It ships as one binary. You can run each step by hand or leave it running as a daemon.

```text
 project folder ┐
 extra files    ├──▶  backup-incr-20250101-120000.tar.zst  ──▶  cloud storage
 database dump  │        (local_backup_dir)                       (backups/…)
 /etc, crontab… ┘
```

## Contents

- [Quick start](#quick-start)
- [Commands](#commands)
- [Configuration](#configuration)
- [Storage providers](#storage-providers)
- [Running on a schedule](#running-on-a-schedule)
- [Encryption](#encryption)
- [Checking your setup: doctor](#checking-your-setup-doctor)
- [Restore drills: proving backups work](#restore-drills-proving-backups-work)
- [Alerts](#alerts)
- [Anomaly checks](#anomaly-checks)
- [Restoring a backup](#restoring-a-backup)
- [Docker](#docker)
- [Security](#security)
- [Troubleshooting](#troubleshooting)
- [Development](#development)

## Quick start

**1. Install.** You need Rust 1.88 or newer ([rustup.rs](https://rustup.rs)).

```bash
git clone https://github.com/rc-basilisk/zesty_backup.git
cd zesty_backup
cargo build --release
sudo cp target/release/zesty-backup /usr/local/bin/
```

**2. Create a config file.**

```bash
zesty-backup generate-config --output config.toml
chmod 600 config.toml        # it will contain credentials
```

Then edit `config.toml`. At minimum, set:

- `[storage]`: your provider and its credentials (see [Storage providers](#storage-providers))
- `[backup] project_path`: the folder to back up
- `[backup] local_backup_dir`: where archives are written

**3. Check the setup.**

```bash
zesty-backup doctor          # paths, keys, database access, and a real upload/download test
```

**4. Run a backup and upload it.**

```bash
zesty-backup backup          # creates ./backups/backup-incr-<date>.tar.zst
zesty-backup upload          # uploads every backup that isn't in the cloud yet
zesty-backup list --remote   # check it arrived
```

**5. Prove it can be restored.**

```bash
zesty-backup drill           # restores the latest remote backup somewhere temporary and checks it
```

**6. Automate it** with the [daemon or a systemd timer](#running-on-a-schedule).

## Commands

All commands read `config.toml` from the current directory. Use
`--config /path/to/config.toml`, or set `ZESTY_BACKUP_CONFIG`, to point
elsewhere.

| Command | What it does |
| --- | --- |
| `backup` | Create a new archive in `local_backup_dir`. `--full` only changes the name to `backup-full-…`. |
| `upload` | Upload local backups that aren't in cloud storage yet. `--file <path>` uploads one file. |
| `list` | List local backups. `--remote` lists backups in cloud storage. |
| `download <name>` | Download a backup (`--output <dir>`, default `./restored`). |
| `restore <file>` | Extract an archive (`--target <dir>`, default `./restored`). |
| `verify <file>` | Check that an archive is complete and not corrupted. |
| `doctor` | Check the setup: paths, encryption keys, database access, and an upload/list/download/delete test on the storage. |
| `accept-changes` | After an anomaly warning you expected (e.g. you deleted a lot of files): make the newest backup the new normal. |
| `test-alerts` | Send a test message to every channel in `[alerts]`. |
| `drill` | Restore the newest remote backup into a temporary folder and check it (files, database, your own commands). |
| `clean` | Delete backups older than `retention_days`, locally and remotely. Add `--dry-run` to preview, or `--local-only` to skip remote. |
| `daemon` | Keep running: back up, upload and clean on a schedule. |
| `client …` | Use remote backups without a config file (e.g. from your laptop). |
| `status` | Show the configuration and local/remote backup counts. |
| `logs` | Show the last lines of the log file (`-l 100` for more). |
| `generate-config` | Write the commented example config. |

`zesty-backup <command> --help` shows every option.

> **About "incremental":** every archive is a complete snapshot. The
> `backup-incr-` and `backup-full-` prefixes only help you tell scheduled and
> manual backups apart.

## Configuration

[`config.example.toml`](config.example.toml) is the full reference; every option
is commented there. Only `[storage]` and `[backup]` are required. Unknown keys
are rejected, so a typo is reported as an error instead of being silently ignored.

```toml
[storage]
provider = "s3"
region = "eu-west-3"
bucket = "my-backups"
access_key = "AKIA..."
secret_key = "..."

[backup]
project_path = "/var/www/myapp"
local_backup_dir = "/var/backups/myapp"
additional_paths = ["/etc/nginx/sites-available/myapp"]
exclude = ["node_modules", ".git", "*.log", "storage/cache/*"]
retention_days = 14
compression_level = 3
```

### What can be included

| Config | Stored in the archive as |
| --- | --- |
| `[backup] project_path` | `project/<folder name>/…` |
| `[backup] additional_paths` | `system/<name>` |
| `[database]` (postgres, mysql/mariadb, mongodb, redis, sqlite, cassandra/scylla schema) | `database/…` |
| `[system] systemd_services` / `systemd_timers` | `systemd/services/…`, `systemd/timers/…` |
| `[system] command_outputs` (output of any command) | `commands/<output_file>` |
| `[system.presets]` nginx, crontab | `system/nginx/…`, `system/crontab-<user>.txt` |
| `[system.presets]` user_configs, etc_files, etc_dirs | `user-configs/…`, `etc/…` |

### Exclude patterns

- **A name or glob without `/`** matches a file or folder at any depth.
  `node_modules` skips every `node_modules` folder; `*.log` skips every `.log` file.
- **A pattern with `/`** matches a path relative to the folder being backed up
  (`storage/cache/*`), or an absolute path (`/var/www/myapp/tmp`).
- `local_backup_dir` is always excluded, even when it sits inside `project_path`.

### Databases

The matching dump tool must be installed: `pg_dump`, `mysqldump`/`mariadb-dump`,
`mongodump`, `redis-cli`, `cqlsh`, or `sqlite3` (optional for SQLite). The
password is read from, in order:

1. `password` in `[database]`
2. the `DB_PASSWORD` environment variable
3. `DATABASE_URL` in `<project_path>/.env`

Without a password, PostgreSQL falls back to `~/.pgpass`. Passwords are passed
to the tools through environment variables, never on the command line.
Cassandra/Scylla is the exception: `cqlsh` has no other way to take a password.

## Storage providers

| Provider | `provider =` | Status |
| --- | --- | --- |
| AWS S3 | `aws` or `s3` | ✅ tested against an S3 emulator |
| MinIO, Contabo, DigitalOcean Spaces, Wasabi, Cloudflare R2, other S3-compatible | `minio`, `contabo`, `digitalocean`, `wasabi`, `r2`, `s3` | ✅ same code as AWS |
| Google Cloud Storage | `gcs` | ⚠️ not tested live |
| Azure Blob Storage | `azure` | ⚠️ not tested live |
| Backblaze B2 | `b2` | ⚠️ not tested live (or use B2's S3 endpoint with `provider = "s3"`) |
| Google Drive | `googledrive` | ⚠️ not tested live |
| OneDrive | `onedrive` | ⚠️ not tested live |
| Dropbox | `dropbox` | ⚠️ not tested live |
| Box | `box` | ⚠️ not tested live |
| pCloud | `pcloud` | ⚠️ not tested live |
| MEGA | `mega` | ⚠️ not tested live, needs [MEGAcmd](https://mega.nz/cmd) |

"Not tested live" means the code follows each provider's official API
documentation but hasn't been run against a real account. If you try one,
please report back in an issue, whether it works or not.

Uploads stream from disk, and large files are sent in parts (multipart, chunked
or upload sessions, depending on the provider), so archive size isn't limited by
RAM or by single-request limits.

Object stores (S3, GCS, Azure, B2) save archives under the `backups/` prefix.
Consumer drives (Google Drive, OneDrive, Dropbox, Box, pCloud, MEGA) save them
directly in the folder set by `bucket_id`.

<details>
<summary><b>AWS S3</b></summary>

```toml
[storage]
provider = "aws"
region = "eu-west-3"
bucket = "my-backups"
access_key = "AKIA..."
secret_key = "..."
```

Leave out `access_key` and `secret_key` to use the standard AWS credential chain
instead: `AWS_*` environment variables, `~/.aws/credentials`, or the instance's
IAM role. The key only needs `s3:PutObject`, `s3:GetObject`, `s3:ListBucket`
and `s3:DeleteObject` on the bucket.
</details>

<details>
<summary><b>S3-compatible: MinIO, Contabo, DigitalOcean, Wasabi, Cloudflare R2, Backblaze (S3 API)</b></summary>

```toml
# MinIO (and any other S3-compatible server)
provider = "minio"
endpoint = "http://localhost:9000"
bucket = "backups"
access_key = "..."
secret_key = "..."

# Contabo
provider = "contabo"
endpoint = "https://eu2.contabostorage.com"

# DigitalOcean Spaces: endpoint derived from region
provider = "digitalocean"
region = "nyc3"

# Wasabi: endpoint derived from region
provider = "wasabi"
region = "eu-central-1"

# Cloudflare R2: endpoint derived from your account ID
provider = "r2"
account_id = "your-cloudflare-account-id"

# Backblaze B2 through its S3 API
provider = "s3"
endpoint = "https://s3.us-west-004.backblazeb2.com"
region = "us-west-004"
```

Each of these also needs `bucket`, `access_key` and `secret_key`. An explicit
`endpoint` always overrides the derived one. Path-style URLs are on by default
for MinIO and Contabo; set `force_path_style = true` or `false` to override.
</details>

<details>
<summary><b>Google Cloud Storage</b></summary>

```toml
provider = "gcs"
bucket = "my-backups"
credentials_path = "/etc/zesty-backup/gcs-key.json"
```

Create a service account with the *Storage Object Admin* role on the bucket,
then download a JSON key for it. Instead of `credentials_path` you can:

- set `GOOGLE_APPLICATION_CREDENTIALS`,
- run `gcloud auth application-default login`, or
- run on a GCP VM that has a service account attached.
</details>

<details>
<summary><b>Azure Blob Storage</b></summary>

```toml
provider = "azure"
account_name = "mystorageaccount"
account_key = "..."        # Storage account → Access keys
bucket = "backups"         # the container name
```

Instead of `account_key`, you can use any `AZURE_STORAGE_*` environment variable
that the `object_store` crate supports, for example `AZURE_STORAGE_SAS_TOKEN`,
or a service principal (`AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`,
`AZURE_TENANT_ID`).
</details>

<details>
<summary><b>Backblaze B2 (native API)</b></summary>

```toml
provider = "b2"
account_id = "0051234abcd..."     # the keyID of an application key
application_key = "K005..."
bucket = "my-backups"             # bucket name
# bucket_id = "..."               # optional, looked up from the name
```

Create the key under *Application Keys* in the Backblaze web UI. Prefer a key
restricted to the backup bucket.
</details>

<details>
<summary><b>Google Drive, OneDrive, Dropbox, Box (OAuth)</b></summary>

These services use OAuth access tokens, which expire after one to four hours.
For anything unattended (the daemon, cron), give Zesty a **refresh token**, and
it will fetch fresh access tokens by itself:

```toml
provider = "dropbox"              # or "googledrive", "onedrive", "box"
refresh_token = "..."
client_id = "..."
client_secret = "..."             # not needed for public/PKCE apps
bucket_id = "/Backups"            # the folder (see the table below)
# token_cache = "/var/lib/zesty-backup/token"   # required for Box, see below
```

For a one-off command you can set `access_key = "<access token>"` instead.

| | Folder (`bucket_id`) | Where to create the app | Authorize URL (open in a browser) | Token URL |
| --- | --- | --- | --- | --- |
| **Google Drive** | folder **ID** (from the folder's URL); default: My Drive root | [Google Cloud Console](https://console.cloud.google.com/apis/credentials): enable the Drive API, create an OAuth client of type *Desktop app* | `https://accounts.google.com/o/oauth2/v2/auth?client_id=ID&redirect_uri=http://localhost&response_type=code&access_type=offline&prompt=consent&scope=https://www.googleapis.com/auth/drive` | `https://oauth2.googleapis.com/token` |
| **OneDrive** | folder **path**, e.g. `/Backups` | [Azure portal → App registrations](https://portal.azure.com/#view/Microsoft_AAD_RegisteredApps): allow personal accounts, add redirect URI `http://localhost` (*Mobile and desktop*) | `https://login.microsoftonline.com/common/oauth2/v2.0/authorize?client_id=ID&redirect_uri=http://localhost&response_type=code&scope=offline_access%20Files.ReadWrite` | `https://login.microsoftonline.com/common/oauth2/v2.0/token` |
| **Dropbox** | folder **path**, e.g. `/Backups` | [App Console](https://www.dropbox.com/developers/apps): enable `files.content.write` and `files.content.read` | `https://www.dropbox.com/oauth2/authorize?client_id=APP_KEY&response_type=code&token_access_type=offline` (no redirect: the code is shown on screen) | `https://api.dropboxapi.com/oauth2/token` |
| **Box** | folder **ID**; default: `0` (root) | [Developer Console](https://app.box.com/developers/console): Custom App → *User Authentication (OAuth 2.0)*, redirect URI `http://localhost` | `https://account.box.com/api/oauth2/authorize?client_id=ID&redirect_uri=http://localhost&response_type=code` | `https://api.box.com/oauth2/token` |

After you approve access, the browser is sent to `http://localhost/?code=…`.
The page won't load; that's expected. Copy the `code` value from the address
bar, then quickly exchange it (Box codes expire after 30 seconds):

```bash
curl -s -X POST <token URL> \
  -d grant_type=authorization_code -d code=<CODE> \
  -d client_id=<ID> -d client_secret=<SECRET> \
  -d redirect_uri=http://localhost     # leave this line out for Dropbox
```

Put the `refresh_token` from the response into your config.

**Watch out for:**

- **Google:** while your OAuth app is in *Testing* mode, refresh tokens stop
  working after 7 days. Publish the app (it can stay unverified for personal
  use) to avoid this.
- **Box:** every refresh token can only be used once. Set `token_cache` to a
  writable file. Zesty saves each new refresh token there and reads it on the
  next start.
- **OneDrive:** for work or school accounts, set `tenant_id` to your tenant
  (default: `common`).
</details>

<details>
<summary><b>pCloud</b></summary>

```toml
provider = "pcloud"
access_key = "..."       # OAuth access token (pCloud tokens don't expire)
region = "eu"            # "us" (default) or "eu": where your account lives
bucket_id = "/Backups"   # folder path, created if missing
```

To get a token:

1. Register an app with pCloud.
2. Open `https://my.pcloud.com/oauth2/authorize?client_id=<ID>&response_type=token&redirect_uri=<your redirect URI>`.
3. Copy `access_token` from the URL you are redirected to.

That URL also contains `hostname`. If it's `eapi.pcloud.com`, set `region = "eu"`.
</details>

<details>
<summary><b>MEGA</b></summary>

Install [MEGAcmd](https://mega.nz/cmd), then log in once as the user who will run
the backups:

```bash
mega-login you@example.com
```

```toml
provider = "mega"
bucket_id = "/Backups"   # folder, created if missing
```

You can also set `account_name` (email) and `account_key` (password), and Zesty
will log in when needed. Avoid this on shared machines: MEGAcmd takes the
password as a command-line argument, which other local users can see.
</details>

## Running on a schedule

### Option A: the built-in daemon

```bash
zesty-backup daemon
```

The daemon creates a backup right away, uploads it, and cleans old backups.
After that it repeats on a schedule:

- **Backups:** every `24 / incremental_per_day` hours (default: 6)
- **Upload and clean:** every `upload_interval_hours` (default: 24)

Override them with `--backup-interval <hours>` and `--upload-interval <hours>`.
If an upload fails, the clean step is skipped, so no local copy is deleted
before it has been uploaded. The daemon stops cleanly on Ctrl-C or SIGTERM.

Example systemd unit, `/etc/systemd/system/zesty-backup.service`:

```ini
[Unit]
Description=Zesty Backup
After=network-online.target
Wants=network-online.target

[Service]
User=backup
WorkingDirectory=/opt/zesty-backup
ExecStart=/usr/local/bin/zesty-backup --config /opt/zesty-backup/config.toml daemon
Restart=on-failure
RestartSec=60

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable --now zesty-backup
journalctl -u zesty-backup -f      # follow the logs
```

### Option B: cron (or a systemd timer)

Nothing runs between backups:

```cron
# every 6 hours: back up, upload what's new, delete what's too old
0 */6 * * *  cd /opt/zesty-backup && zesty-backup backup && zesty-backup upload && zesty-backup clean
```

## Encryption

Archives can be encrypted with [age](https://age-encryption.org) before
they're written to disk, so neither the local disk nor the cloud provider ever
sees plain data. Encrypted archives are named `….tar.zst.age`.

The recommended setup uses a key pair, where the server only knows the
**public** key. Someone who breaks into the server or the bucket can't read
past backups.

```bash
# On your own machine (install age from https://age-encryption.org or your package manager):
age-keygen -o zesty-key.txt        # prints: Public key: age1...
```

```toml
[encryption]
recipients = ["age1..."]           # the public key; add several to let any of them decrypt
```

Store `zesty-key.txt` somewhere safe and **not** on the server. **Without it,
the backups can't be restored.** To restore:

```bash
zesty-backup restore backup-incr-20250101-120000.tar.zst.age --identity zesty-key.txt
# or with the age CLI:
age -d -i zesty-key.txt backup.tar.zst.age | zstd -d | tar -x
```

If you'd rather use a passphrase:

1. Set `ZESTY_BACKUP_PASSPHRASE` in the environment, or `passphrase = "…"`
   under `[encryption]`.
2. Set the same variable when you run `restore` or `verify`.

You can't use recipients and a passphrase at the same time; age doesn't allow
mixing them.

## Checking your setup: doctor

`zesty-backup doctor` checks everything a backup needs, without making one:

```text
$ zesty-backup doctor
  ✔ Project path         /srv/myshop (12 entries)
  ✔ Backup directory     /var/backups/myshop is writable (8 backups, 1.21 GiB)
  ✔ Encryption           on (public key); your private key decrypts it
  ✔ Database             pg_dump connected to shop on 127.0.0.1 (schema-only test)
  ✔ Storage credentials  b2 client ready
  ✔ Remote backups       8 backup(s), newest backup-incr-20261021-180000.tar.zst (3h old)
  ✔ Storage upload       backups/zesty-doctor-20261021-210000.txt (256.00 KiB) in 0.4s
  ✔ Storage list         test file listed with the right size
  ✔ Storage download     identical content, 0.3s
  ✖ Storage delete       HTTP 403 Forbidden: AccessDenied
                         → retention (`clean`) will not work without delete permission
Result: FAILED (1 failed, 0 warning(s))
```

What it checks:

- **Local:** that `project_path` is readable and `local_backup_dir` writable.
- **Encryption:** encrypts and decrypts a sample with your config. Pass
  `--identity key.txt` to prove your private key matches the configured public
  key. A backup you can't decrypt is not a backup.
- **Database:** runs a schema-only dump with your real settings. This catches
  wrong passwords, unreachable hosts and a `pg_dump` older than the server.
- **Storage:** uploads a small test file, lists it, downloads it, compares it
  and deletes it. Failures come with a hint (wrong credentials, missing
  permission, wrong bucket name, network).

`--skip-storage` skips the remote part. The exit code is 1 when something fails,
so `doctor` also works in scripts and CI.

## Restore drills: proving backups work

A backup is only as good as its last successful restore. `zesty-backup drill`
does one for you, safely:

1. Takes the newest **remote** backup (or `--local`, or a given name) and
   checks it is recent enough.
2. Downloads, decrypts and extracts it into a temporary folder.
3. Checks that the files you care about are there.
4. Loads the database dump into a **throwaway Docker container**, then runs
   your SQL checks on it. The image is picked from the version in the dump
   (e.g. `postgres:16-alpine`). The container is removed afterwards.
5. Runs your own commands in the restored folder.
6. Deletes everything (unless `--keep`), and records the result for `status`.

```text
$ zesty-backup drill
Restore drill: backup-incr-20261021-233656.tar.zst
  ✔ Find backup       backup-incr-20261021-233656.tar.zst (remote)
  ✔ Freshness         newest backup is 3.2h old (limit 26h)
  ✔ Download          412.80 MiB in 9.1s
  ✔ Extract           18342 files, 1.37 GiB in 6.4s
  ✔ Project files     18341 files under project/
  ✔ File check        project/myshop/.env (1 match)
  ✔ Database dump     database/shop.sql (220.17 MiB)
  ✔ Database restore  42 tables restored into postgres:16-alpine in 48.2s
  ✔ Database check    SELECT count(*) >= 1000 FROM users → t
  ✔ Command           .env has APP_KEY (0.0s)
Result: PASSED
```

Configure what "usable" means in `[drill]`:

```toml
[drill]
max_age_hours = 26                       # fail if the newest backup is older
files = ["project/myshop/.env", "project/myshop/public/**"]
database_checks = [                      # each must return something other than empty/0/false
  "SELECT count(*) >= 1000 FROM users",
  "SELECT max(created_at) > now() - interval '2 days' FROM orders",
]
commands = [                             # run with sh in the restored folder; must exit 0
  { name = "config parses", run = "php -l project/myshop/config/app.php" },
]
interval_hours = 168                     # the daemon runs a drill weekly
```

Notes:

- **Database restore tests** work for PostgreSQL, MySQL and MariaDB. They
  need Docker (or `container_runtime = "podman"`). SQLite is checked with
  `sqlite3` (`PRAGMA integrity_check`). For other database types, the drill
  checks that the dump is present.
- **Encrypted backups:** the drill needs the private key. With public-key
  encryption the server doesn't have it, which is the point, so run drills
  from another machine, e.g. your laptop or a second server. That machine
  needs a copy of the config, `identity_file` (or `--identity`), and a cron
  entry. Without a key, the drill tells you it couldn't look inside the backup
  instead of pretending it passed.
- **Disk space:** the drill needs room for the backup and its extracted
  contents. Point `work_dir` at a big disk if `/tmp` is small.
- **Exit code** is 1 when a check fails, so cron/systemd can alert on it.

## Alerts

A backup system that fails silently is worse than none. `[alerts]` sends
results to up to three kinds of channel:

```toml
[alerts]
healthchecks_url = "https://hc-ping.com/your-uuid"   # dead-man's switch (see below)
ntfy_url = "https://ntfy.sh/my-secret-topic"          # push notifications to your phone
webhook_url = "https://hooks.slack.com/services/..."  # Slack, Discord, or any JSON endpoint
# notify_success = true     # also send ntfy/webhook messages when things go well
# server_name = "web-1"     # name used in messages (default: the hostname)
```

- **[healthchecks.io](https://healthchecks.io)** (free tier, or self-hosted):
  every successful backup, upload and clean pings the check, and every failure
  pings `/fail`. Set the check's period to slightly more than your backup
  interval. If the pings stop because the server is down, the disk is full or
  the daemon crashed, *healthchecks.io* alerts you. That's the one failure Zesty
  can't report itself. Put drills on their own check with
  `healthchecks_drill_url`; otherwise only drill *failures* are reported to the
  main check.
- **[ntfy](https://ntfy.sh)**: push notifications on your phone, high priority
  on failures. Use a hard-to-guess topic name, or `ntfy_token` for a protected
  or self-hosted server.
- **Webhook:** Slack and Discord incoming-webhook URLs get a formatted message.
  Any other URL gets a JSON POST with `event`, `success`, `server`, `summary`
  and `details`.

By default, ntfy and the webhook only hear about failures. This works the same
from the daemon and from cron (`backup`, `upload`, `clean` and `drill` each
report). Run `zesty-backup test-alerts` to check the setup. A failed alert is
logged but never stops a backup.

## Anomaly checks

Some failures don't look like failures. The backup "succeeds", but a deploy
script wiped half the project, the database dump came out empty, or ransomware
encrypted the files, and now that damage is the newest backup. A week later,
retention deletes the last good copy.

Every new backup is compared with the median of the previous five, and is
flagged when:

- the **total size** drops by more than 50%;
- the **number of files** drops by more than 50%;
- the **database dump** shrinks by more than 50%, or is missing;
- the data **stopped compressing**: it used to shrink well and suddenly
  doesn't, which is what files look like after ransomware has encrypted them.

When that happens:

1. A warning is logged and an alert is sent (healthchecks `/fail`, ntfy,
   webhook).
2. **Retention pauses**: `clean` (and the daemon) won't delete any backup
   while the newest one looks wrong. `status` and `doctor` show it.
3. If the change was expected, run `zesty-backup accept-changes`. The newest
   backup becomes the new normal and retention resumes. `clean --force` also
   overrides the pause.

The checks are on by default. Tune or disable them:

```toml
[anomaly]
size_drop_percent = 50        # flag when the total size drops by more than this
files_drop_percent = 50
database_drop_percent = 50
incompressible_ratio = 0.9    # flag data that suddenly compresses to more than 90% of its size
pause_retention = true
# enabled = false
```

## Restoring a backup

```bash
zesty-backup list --remote
zesty-backup download backup-incr-20250101-120000.tar.zst --output ./restored
zesty-backup verify  ./restored/backup-incr-20250101-120000.tar.zst
zesty-backup restore ./restored/backup-incr-20250101-120000.tar.zst --target ./restored/files
```

`restore` keeps file permissions, timestamps and symlinks. It refuses archive
entries that would land outside the target folder.

On a machine with no config file, use `client`. It takes the credentials as
flags, and secrets can also come from environment variables:

```bash
export ZESTY_ACCESS_KEY=... ZESTY_SECRET_KEY=...
zesty-backup client --provider aws --region eu-west-3 --bucket my-backups list
zesty-backup client --provider aws --region eu-west-3 --bucket my-backups \
  download backup-incr-20250101-120000.tar.zst --output ./restored
```

Archives are standard `tar` + `zstd`, so you can also use system tools:
`tar --zstd -xf backup.tar.zst`, or `zstd -dc backup.tar.zst | tar -x`.

## Docker

```bash
docker build -t zesty-backup .
mkdir -p config backups logs && cp config.example.toml config/config.toml   # then edit it
docker compose up -d     # runs the daemon, see docker-compose.yml
```

Inside the container, the config is read from `/app/config/config.toml`
(`ZESTY_BACKUP_CONFIG`). Mount the folders you want backed up read-only, and
point `project_path` at them. The image doesn't include database dump tools.
To back up a database from Docker, extend the image or use `command_outputs`.

## Security

- Archives are written with `0600` permissions. Turn on
  [encryption](#encryption) so the cloud provider never sees plain data. With
  a public key, the server can't decrypt its own backups either.
- Keep `config.toml` private (`chmod 600`). Prefer IAM roles, service
  accounts, or environment variables over keys in the file where possible.
- Use credentials limited to the backup bucket or folder.

See [SECURITY.md](SECURITY.md) to report a vulnerability.

## Troubleshooting

| Symptom | Things to check |
| --- | --- |
| `Failed to parse config file … unknown field` | Typo in a key name. Compare with `config.example.toml`. |
| Backup fails on a database | Is the dump tool installed and in `PATH`? Is the password set? Run the tool by hand with the same settings. |
| `… file(s) could not be read and were skipped` | Permission problems. Run as a user that can read those files, or exclude them. |
| S3-compatible upload fails with a signature or checksum error | Set the right `region`. Try `force_path_style = true`. |
| B2/Azure/GCS "not found" | Check the bucket/container name, and that the key has access to it. |
| OAuth provider: 401 / `invalid_grant` | The access token expired, or the refresh token was revoked or used twice (Box). Get a new refresh token. |
| pCloud: `Log in failed` / code 2000 | Wrong token, or wrong `region` (EU accounts need `region = "eu"`). |
| `This backup is encrypted` | Pass `--identity <key file>`, or set `ZESTY_BACKUP_PASSPHRASE` for passphrase-encrypted backups. |
| `Not deleting any backup: the newest backup … looked wrong` | An anomaly paused retention. If the change was expected, run `zesty-backup accept-changes`. |
| No alerts arrive | Run `zesty-backup test-alerts`: it shows each channel's error. |
| Drill: `docker run … failed` | Is Docker running, and may this user use it? "rate limit" means Docker Hub throttled the image download: retry later or `docker login`. |
| Drill: database restore has errors | A missing extension or role, or an image older than your server. Set `drill.database_image`. |
| `zesty-backup logs` says no log file | Set `log_dir` in `[logging]`. Under systemd, use `journalctl -u zesty-backup`. |

Use `RUST_LOG=zesty_backup=debug zesty-backup …` for more detail.

## Development

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

Code layout:

- `src/main.rs`: the CLI
- `src/backup.rs`: create, upload, list, download and clean
- `src/archive.rs`: tar.zst writing, extraction and exclude patterns
- `src/encryption.rs`: optional age encryption
- `src/alerts.rs`: healthchecks/ntfy/webhook notifications
- `src/anomaly.rs`: comparing each backup with the previous ones
- `src/doctor.rs`, `src/drill.rs`: setup checks and restore drills (shared report format in `src/report.rs`)
- `src/database.rs`: database dumps
- `src/providers/`: one file per storage backend

The tests in `tests/` run the whole workflow against a local folder that stands
in for cloud storage. See [CHANGELOG.md](CHANGELOG.md) for what changed.

Contributions are welcome. Please open an issue or a PR.

## License

[MIT](LICENSE)
