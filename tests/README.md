# Integration Tests

This directory contains integration tests for zesty-backup.

## Running Tests

```bash
# Run all tests
cargo test

# Run specific test file
cargo test --test integration_test
cargo test --test provider_test
cargo test --test backup_test

# Run with output
cargo test -- --nocapture

# Run a specific test
cargo test test_config_parsing
```

## Test Structure

### integration_test.rs
General integration tests covering:
- Configuration parsing
- Directory creation
- File exclusion patterns
- Timestamp formatting
- Path operations

### provider_test.rs
Provider-specific tests covering:
- S3 provider configuration
- GCS provider configuration
- Azure provider configuration
- B2 provider configuration
- Consumer-grade provider configurations
- Provider name aliases

### backup_test.rs
Backup functionality tests covering:
- Backup file naming
- Directory structure creation
- Compression level validation
- Retention policy calculation
- Path exclusion logic
- File size calculations
- Timestamp parsing

## Test Environment

Tests use the `tempfile` crate to create temporary directories and files, ensuring:
- No pollution of the actual filesystem
- Automatic cleanup after tests
- Isolation between test runs

## Adding New Tests

When adding new tests:

1. **Unit Tests**: Add to the relevant module in `src/` with `#[cfg(test)]`
2. **Integration Tests**: Add to the appropriate test file in `tests/`
3. **Provider Tests**: Add to `tests/provider_test.rs` for provider-specific tests
4. **Backup Tests**: Add to `tests/backup_test.rs` for backup-specific tests

## Mock Providers

For testing with actual cloud providers, you would need:
- Test credentials (use environment variables or test configs)
- Test buckets/containers
- Cleanup after tests

**Note**: Current tests focus on configuration parsing and local file operations. Full integration tests with actual cloud providers would require test accounts and credentials.

## CI/CD

Tests run automatically in CI/CD via GitHub Actions (see `.github/workflows/ci.yml`).

