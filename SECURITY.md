# Security Policy

## Supported Versions

We actively support the following versions with security updates:

| Version | Supported          |
| ------- | ------------------ |
| 1.0.x   | :white_check_mark: |
| < 1.0   | :x:                |

## Reporting a Vulnerability

We take security vulnerabilities seriously. If you discover a security vulnerability, please follow these steps:

### 1. **Do NOT** open a public issue

Security vulnerabilities should be reported privately to protect users.

### 2. Email Security Team

Please email security concerns to: **admin@zetsubou.life**

Include the following information:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)
- Your contact information

### 3. Response Timeline

- **Initial Response**: Within 48 hours
- **Status Update**: Within 7 days
- **Fix Timeline**: Depends on severity, but we aim for:
  - **Critical**: 7 days
  - **High**: 30 days
  - **Medium/Low**: Next release cycle

### 4. Disclosure Policy

- We will acknowledge receipt of your report
- We will keep you informed of our progress
- We will credit you in the security advisory (unless you prefer to remain anonymous)
- We will coordinate public disclosure after a fix is available

## Security Best Practices

When using zesty-backup:

1. **Credentials Management**:
   - Never commit `config.toml` with real credentials
   - Use environment variables when possible
   - Restrict file permissions: `chmod 600 config.toml`
   - Use IAM roles/service accounts when available

2. **Network Security**:
   - Always use HTTPS for cloud storage connections
   - Verify SSL certificates (enabled by default)
   - Use VPN or secure networks when possible

3. **Access Control**:
   - Run with minimal required permissions
   - Use read-only credentials for backup operations when possible
   - Regularly rotate access keys

4. **Backup Security**:
   - Encrypt sensitive backups before uploading
   - Use provider-side encryption when available
   - Store credentials securely (use secret management tools)

5. **Updates**:
   - Keep zesty-backup updated to the latest version
   - Monitor security advisories
   - Review CHANGELOG.md for security-related updates

## Known Security Considerations

### Credential Storage
- Credentials are stored in plaintext in `config.toml`
- Consider using environment variables or secret management systems
- File permissions should be restricted (600)

### Network Communication
- All cloud provider APIs use HTTPS/TLS
- No data is transmitted over unencrypted connections
- Provider-specific security features are respected

### Local Storage
- Backup files are stored locally before upload
- Ensure backup directories have appropriate permissions
- Consider encrypting local backups for sensitive data

### MEGA Provider
- Uses MEGAcmd which handles client-side encryption
- Requires MEGAcmd to be installed and configured
- MEGA credentials are passed to MEGAcmd process

## Security Audit

If you're conducting a security audit:

1. Review the codebase for common vulnerabilities
2. Test with various storage providers
3. Verify credential handling
4. Check for information disclosure in logs
5. Test error handling and edge cases

We welcome security audits and will work with auditors to address any findings.

## Acknowledgments

We appreciate responsible disclosure and will acknowledge security researchers who help improve zesty-backup's security.

