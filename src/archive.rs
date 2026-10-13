//! Writing and extracting `.tar.zst` backup archives.

use crate::encryption::{open_archive, DecryptionKeys, Encryption};
use anyhow::{Context, Result};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};
use walkdir::WalkDir;

/// Decides which files are skipped, from the `exclude` list in the config.
///
/// * A pattern without a `/` (e.g. `node_modules`, `*.log`, `.git`) is matched against
///   every path component, so it excludes matching files *and* whole directories at any depth.
/// * A pattern containing a `/` (e.g. `storage/cache/*`, `/var/www/app/tmp`) is matched against
///   the path relative to the directory being backed up and against the absolute path.
/// * `*`, `?`, `[abc]` and `**` wildcards are supported.
#[derive(Debug, Default)]
pub struct Excluder {
    name_patterns: GlobSet,
    path_patterns: GlobSet,
    excluded_dirs: Vec<PathBuf>,
}

impl Excluder {
    pub fn new(patterns: &[String]) -> Result<Self> {
        let mut names = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();
        for pattern in patterns {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                continue;
            }
            if pattern.contains('/') {
                let trimmed = pattern.trim_end_matches('/');
                let relative = trimmed.trim_start_matches("./");
                for p in [trimmed, relative] {
                    paths.add(
                        GlobBuilder::new(p)
                            .literal_separator(true)
                            .build()
                            .with_context(|| format!("Invalid exclude pattern: {}", pattern))?,
                    );
                }
            } else {
                names.add(
                    Glob::new(pattern)
                        .with_context(|| format!("Invalid exclude pattern: {}", pattern))?,
                );
            }
        }
        Ok(Self {
            name_patterns: names.build()?,
            path_patterns: paths.build()?,
            excluded_dirs: Vec::new(),
        })
    }

    /// Always skip this directory (used to keep the local backup dir out of the backup itself).
    pub fn exclude_dir(&mut self, dir: &Path) {
        if let Ok(canonical) = dir.canonicalize() {
            self.excluded_dirs.push(canonical);
        }
    }

    /// `relative` is the path relative to the directory being walked.
    pub fn is_excluded(&self, absolute: &Path, relative: &Path) -> bool {
        if relative
            .components()
            .any(|c| self.name_patterns.is_match(c.as_os_str()))
        {
            return true;
        }
        if self.path_patterns.is_match(relative) || self.path_patterns.is_match(absolute) {
            return true;
        }
        if !self.excluded_dirs.is_empty() {
            if let Ok(canonical) = absolute.canonicalize() {
                return self.excluded_dirs.iter().any(|d| canonical.starts_with(d));
            }
        }
        false
    }
}

/// A `.tar.zst` archive being written.
///
/// The archive is written to `<path>.partial` and only renamed to its final name by
/// [`ArchiveWriter::finish`], so an interrupted backup never leaves a truncated file that
/// looks like a valid backup.
pub struct ArchiveWriter {
    builder: Option<tar::Builder<zstd::Encoder<'static, Sink>>>,
    partial_path: PathBuf,
    final_path: PathBuf,
    skipped: usize,
}

impl ArchiveWriter {
    /// `path` should end in `.tar.zst`, or `.tar.zst.age` when `encryption` is given.
    pub fn create(
        path: &Path,
        compression_level: i32,
        encryption: Option<&Encryption>,
    ) -> Result<Self> {
        let partial_path = partial_path(path);
        // Backups contain secrets (database dumps, configs, keys): owner-only permissions.
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&partial_path)
            .with_context(|| format!("Failed to create {}", partial_path.display()))?;
        let output = BufWriter::new(file);
        let sink = match encryption {
            Some(e) => Sink::Encrypted(e.wrap(output)?),
            None => Sink::Plain(output),
        };
        let mut encoder = zstd::Encoder::new(sink, compression_level)?;
        // Lets `zstd -t` / `zstd -d` detect corruption.
        encoder.include_checksum(true)?;
        let mut builder = tar::Builder::new(encoder);
        // Store symlinks as symlinks instead of copying (or failing on) their targets.
        builder.follow_symlinks(false);
        Ok(Self {
            builder: Some(builder),
            partial_path,
            final_path: path.to_path_buf(),
            skipped: 0,
        })
    }

    /// Number of files that could not be read and were left out.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Add a file or a whole directory tree under `archive_prefix`.
    pub fn add_path(
        &mut self,
        src: &Path,
        archive_prefix: &str,
        excluder: &Excluder,
    ) -> Result<()> {
        let meta = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => {
                warn!("Skipping {}: {}", src.display(), e);
                self.skipped += 1;
                return Ok(());
            }
        };
        if !meta.is_dir() {
            return self.add_entry(src, &meta.file_type(), archive_prefix);
        }

        let walker = WalkDir::new(src).follow_links(false).into_iter();
        let walker = walker.filter_entry(|entry| {
            let relative = entry.path().strip_prefix(src).unwrap_or(entry.path());
            relative.as_os_str().is_empty() || !excluder.is_excluded(entry.path(), relative)
        });
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    warn!("Skipping unreadable entry: {}", e);
                    self.skipped += 1;
                    continue;
                }
            };
            let relative = entry.path().strip_prefix(src).unwrap_or(entry.path());
            let archive_path = if relative.as_os_str().is_empty() {
                archive_prefix.to_string()
            } else {
                format!("{}/{}", archive_prefix, relative.to_string_lossy())
            };
            self.add_entry(entry.path(), &entry.file_type(), &archive_path)?;
        }
        Ok(())
    }

    fn add_entry(
        &mut self,
        path: &Path,
        file_type: &fs::FileType,
        archive_path: &str,
    ) -> Result<()> {
        let archive_path = archive_path.trim_start_matches('/');
        if file_type.is_dir() {
            if let Err(e) = self.builder().append_dir(archive_path, path) {
                warn!("Skipping directory {}: {}", path.display(), e);
                self.skipped += 1;
            }
            return Ok(());
        }
        if file_type.is_symlink() {
            return self
                .builder()
                .append_path_with_name(path, archive_path)
                .with_context(|| format!("Failed to add symlink {}", path.display()));
        }
        if !file_type.is_file() {
            // Sockets, FIFOs, device nodes: reading them would block or make no sense.
            debug!("Skipping special file {}", path.display());
            return Ok(());
        }
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                warn!("Skipping {}: {}", path.display(), e);
                self.skipped += 1;
                return Ok(());
            }
        };
        self.builder()
            .append_file(archive_path, &mut file)
            .with_context(|| format!("Failed to add {} to archive", path.display()))
    }

    /// Add in-memory content as a regular file (mode 0644, mtime = now).
    pub fn add_bytes(&mut self, archive_path: &str, data: &[u8]) -> Result<()> {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(chrono::Utc::now().timestamp().max(0) as u64);
        header.set_entry_type(tar::EntryType::Regular);
        self.builder()
            .append_data(&mut header, archive_path.trim_start_matches('/'), data)
            .with_context(|| format!("Failed to add {} to archive", archive_path))
    }

    /// Write the tar trailer, finish the zstd frame, fsync and move the file into place.
    pub fn finish(mut self) -> Result<PathBuf> {
        let encoder = self
            .builder
            .take()
            .expect("archive already finished")
            .into_inner()
            .context("Failed to finish tar archive")?;
        let sink = encoder.finish().context("Failed to finish zstd stream")?;
        let mut writer = match sink {
            Sink::Plain(w) => w,
            Sink::Encrypted(w) => w.finish().context("Failed to finish encryption")?,
        };
        writer.flush()?;
        let file = writer.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.partial_path, &self.final_path)
            .with_context(|| format!("Failed to move archive to {}", self.final_path.display()))?;
        Ok(self.final_path.clone())
    }

    fn builder(&mut self) -> &mut tar::Builder<zstd::Encoder<'static, Sink>> {
        self.builder.as_mut().expect("archive already finished")
    }
}

/// Where the compressed stream goes: the file, or an age encryptor in front of it.
enum Sink {
    Plain(BufWriter<File>),
    Encrypted(age::stream::StreamWriter<BufWriter<File>>),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sink::Plain(w) => w.write(buf),
            Sink::Encrypted(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Plain(w) => w.flush(),
            Sink::Encrypted(w) => w.flush(),
        }
    }
}

impl Drop for ArchiveWriter {
    fn drop(&mut self) {
        // Only reached with the partial file still present if `finish` was not called
        // (i.e. the backup failed); don't leave half-written archives around.
        let _ = fs::remove_file(&self.partial_path);
    }
}

fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}

/// Extract a `.tar.zst` (or encrypted `.tar.zst.age`) archive into `target`, keeping permissions and timestamps.
/// Entries that would escape `target` (absolute paths, `..`) are refused by the tar crate.
pub fn extract(archive: &Path, target: &Path, keys: &DecryptionKeys) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("Failed to create {}", target.display()))?;
    let decoder = zstd::Decoder::new(open_archive(archive, keys)?)?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    tar.set_overwrite(true);
    tar.unpack(target)
        .with_context(|| format!("Failed to extract {}", archive.display()))
}

/// Read the whole archive without writing anything, to check it is complete and uncorrupted.
/// Returns the number of entries.
pub fn verify(archive: &Path, keys: &DecryptionKeys) -> Result<usize> {
    let decoder = zstd::Decoder::new(open_archive(archive, keys)?)?;
    let mut tar = tar::Archive::new(decoder);
    let mut count = 0;
    for entry in tar.entries()? {
        let mut entry = entry.context("Archive is corrupt or truncated")?;
        std::io::copy(&mut entry, &mut std::io::sink())
            .context("Archive is corrupt or truncated")?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn excluder(patterns: &[&str]) -> Excluder {
        Excluder::new(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn excludes_by_name_and_glob() {
        let ex = excluder(&["node_modules", "*.log", ".git", "storage/cache"]);
        let check = |rel: &str| ex.is_excluded(&Path::new("/project").join(rel), Path::new(rel));
        assert!(check("node_modules"));
        assert!(check("web/node_modules/pkg/index.js"));
        assert!(check("app.log"));
        assert!(check("logs/today.log"));
        assert!(check(".git/config"));
        assert!(check("storage/cache"));
        assert!(!check("src/main.rs"));
        assert!(!check("catalog.txt"));
        assert!(!check(".gitignore"));
        assert!(!check("storage/app.db"));
    }

    #[test]
    fn excludes_absolute_paths() {
        let ex = excluder(&["/srv/app/tmp/*"]);
        assert!(ex.is_excluded(Path::new("/srv/app/tmp/x"), Path::new("tmp/x")));
        assert!(!ex.is_excluded(Path::new("/srv/app/src/x"), Path::new("src/x")));
    }
}
