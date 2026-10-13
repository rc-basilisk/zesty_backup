//! Optional archive encryption in the [age](https://age-encryption.org) format.
//!
//! Encrypted archives are named `….tar.zst.age` and are plain age files, so they can also be
//! decrypted without zesty-backup: `age -d -i key.txt backup.tar.zst.age | zstd -d | tar -x`.

use crate::config::EncryptionConfig;
use age::secrecy::SecretString;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// Environment variable holding the passphrase (instead of `passphrase` in the config).
pub const PASSPHRASE_ENV: &str = "ZESTY_BACKUP_PASSPHRASE";
/// Extension added to encrypted archives.
pub const EXTENSION: &str = ".age";

const AGE_MAGIC: &[u8] = b"age-encryption.org/";
const AGE_ARMOR_MAGIC: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----";

/// How new archives are encrypted.
pub enum Encryption {
    /// Public keys (`age1…`): the machine making backups cannot decrypt them.
    Recipients(Vec<age::x25519::Recipient>),
    Passphrase(SecretString),
}

impl Encryption {
    /// `None` when encryption is not enabled.
    pub fn from_config(config: Option<&EncryptionConfig>) -> Result<Option<Self>> {
        let Some(config) = config else {
            return Ok(None);
        };
        if !config.enabled.unwrap_or(true) {
            return Ok(None);
        }
        let mut recipients = Vec::new();
        for r in config.recipients.iter().flatten() {
            recipients.push(parse_recipient(r)?);
        }
        if let Some(file) = &config.recipients_file {
            let content = std::fs::read_to_string(file)
                .with_context(|| format!("Failed to read recipients_file {}", file))?;
            for line in content.lines().map(str::trim) {
                if !line.is_empty() && !line.starts_with('#') {
                    recipients.push(parse_recipient(line)?);
                }
            }
        }
        let passphrase = passphrase(config);
        match (recipients.is_empty(), passphrase) {
            (false, None) => Ok(Some(Self::Recipients(recipients))),
            (true, Some(p)) => Ok(Some(Self::Passphrase(p))),
            (false, Some(_)) => bail!(
                "[encryption]: use either recipients or a passphrase, not both (age does not allow mixing them)"
            ),
            (true, None) => bail!(
                "[encryption] is enabled but has no recipients, and no passphrase (config or {})",
                PASSPHRASE_ENV
            ),
        }
    }

    pub fn wrap<W: Write>(&self, output: W) -> Result<age::stream::StreamWriter<W>> {
        let encryptor = match self {
            Self::Recipients(recipients) => age::Encryptor::with_recipients(
                recipients.iter().map(|r| r as &dyn age::Recipient),
            )?,
            Self::Passphrase(p) => age::Encryptor::with_user_passphrase(p.clone()),
        };
        Ok(encryptor.wrap_output(output)?)
    }
}

fn parse_recipient(s: &str) -> Result<age::x25519::Recipient> {
    s.trim().parse().map_err(|e| {
        anyhow::anyhow!(
            "Invalid age recipient {:?}: {} (expected a public key starting with age1, from `age-keygen`)",
            s,
            e
        )
    })
}

fn passphrase(config: &EncryptionConfig) -> Option<SecretString> {
    std::env::var(PASSPHRASE_ENV)
        .ok()
        .or_else(|| config.passphrase.clone())
        .filter(|p| !p.is_empty())
        .map(SecretString::from)
}

/// What can decrypt archives: identity files (private keys) and/or a passphrase.
#[derive(Default)]
pub struct DecryptionKeys {
    pub identity_files: Vec<PathBuf>,
    pub passphrase: Option<SecretString>,
}

impl DecryptionKeys {
    /// From an explicit `--identity` file, the config's `identity_file`/`passphrase`,
    /// and the passphrase environment variable.
    pub fn new(identity: Option<&Path>, config: Option<&EncryptionConfig>) -> Self {
        let mut identity_files: Vec<PathBuf> =
            identity.map(Path::to_path_buf).into_iter().collect();
        if let Some(file) = config.and_then(|c| c.identity_file.as_ref()) {
            identity_files.push(file.into());
        }
        let passphrase = std::env::var(PASSPHRASE_ENV)
            .ok()
            .or_else(|| config.and_then(|c| c.passphrase.clone()))
            .filter(|p| !p.is_empty())
            .map(SecretString::from);
        Self {
            identity_files,
            passphrase,
        }
    }

    fn decrypt<R: Read + 'static>(&self, input: R) -> Result<Box<dyn Read>> {
        let decryptor = age::Decryptor::new(input).context("Invalid age file")?;
        if decryptor.is_scrypt() {
            let Some(p) = &self.passphrase else {
                bail!(
                    "This backup is passphrase-encrypted: set {} (or passphrase in [encryption])",
                    PASSPHRASE_ENV
                );
            };
            let identity = age::scrypt::Identity::new(p.clone());
            let reader = decryptor
                .decrypt(std::iter::once(&identity as &dyn age::Identity))
                .context("Decryption failed (wrong passphrase?)")?;
            return Ok(Box::new(reader));
        }
        if self.identity_files.is_empty() {
            bail!("This backup is encrypted: pass --identity <key file> (or set identity_file in [encryption])");
        }
        let mut identities = Vec::new();
        for file in &self.identity_files {
            let parsed = age::IdentityFile::from_file(file.to_string_lossy().to_string())
                .with_context(|| format!("Failed to read identity file {}", file.display()))?
                .into_identities()
                .with_context(|| format!("Invalid identity file {}", file.display()))?;
            identities.extend(parsed);
        }
        let reader = decryptor
            .decrypt(identities.iter().map(|i| i.as_ref() as &dyn age::Identity))
            .context("Decryption failed (no matching key in the identity file)")?;
        Ok(Box::new(reader))
    }
}

/// Open an archive for reading, decrypting it first if it is an age file.
pub fn open_archive(path: &Path, keys: &DecryptionKeys) -> Result<Box<dyn Read>> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let head = reader.fill_buf()?;
    if head.starts_with(AGE_ARMOR_MAGIC) {
        return keys.decrypt(age::armor::ArmoredReader::new(reader));
    }
    if head.starts_with(AGE_MAGIC) {
        return keys.decrypt(reader);
    }
    Ok(Box::new(reader))
}
