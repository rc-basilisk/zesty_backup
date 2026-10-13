# Tests

```bash
cargo test                      # everything
cargo test --test backup_test   # end-to-end workflow only
cargo test --lib                # unit tests inside src/
```

- `backup_test.rs` creates a real backup from a temporary project folder, then
  verifies and restores it. It checks content, exclusions, permissions and
  symlinks. It also runs upload, list, download and clean against `DirProvider`,
  a local folder standing in for cloud storage.
- Unit tests next to the code (`#[cfg(test)]` modules) cover config parsing,
  exclude patterns, `.env` password parsing, file-name/timestamp handling and
  provider response parsing (MEGA `ls` output, Dropbox header escaping).

Cloud providers aren't tested against real accounts in CI. To try the S3 code
for free, run a local S3 server such as MinIO or `moto_server`, then use
`provider = "minio"` with `endpoint = "http://127.0.0.1:<port>"`.
