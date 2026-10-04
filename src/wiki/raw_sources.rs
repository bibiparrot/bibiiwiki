use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawSourceRecord {
    pub source: PathBuf,
    pub backup: PathBuf,
    pub sha256: String,
    pub status: String,
}

#[derive(Clone, Debug)]
pub struct RawSourceBackup {
    root: PathBuf,
}

impl RawSourceBackup {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Copies an input byte-for-byte without replacing a different prior version.
    ///
    /// # Errors
    ///
    /// Returns an error when the source cannot be read or the backup cannot be
    /// created atomically.
    pub fn copy(&self, source: &Path) -> Result<RawSourceRecord> {
        let source = source
            .canonicalize()
            .with_context(|| format!("failed to resolve raw source {}", source.display()))?;
        let digest = sha256(&source)?;
        let name = source
            .file_name()
            .context("raw source path has no file name")?;
        let primary = self.root.join(name);
        if primary.exists() && sha256(&primary)? == digest {
            return Ok(record(source, primary, digest, "unchanged"));
        }

        let target = if primary.exists() {
            let stem = source
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("source");
            let extension = source.extension().and_then(|value| value.to_str());
            let name = extension.map_or_else(
                || format!("{stem}-{}", &digest[..12]),
                |extension| format!("{stem}-{}.{extension}", &digest[..12]),
            );
            self.root.join(name)
        } else {
            primary
        };
        if target.exists() && sha256(&target)? == digest {
            return Ok(record(source, target, digest, "unchanged"));
        }

        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let temporary = self.root.join(format!(
            ".{}.{}-{unique}.tmp",
            target
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("source"),
            std::process::id()
        ));
        let result = (|| -> Result<()> {
            let bytes = fs::read(&source)
                .with_context(|| format!("failed to read {}", source.display()))?;
            let mut file = File::create(&temporary)
                .with_context(|| format!("failed to create {}", temporary.display()))?;
            file.write_all(&bytes)
                .with_context(|| format!("failed to write {}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", temporary.display()))?;
            fs::rename(&temporary, &target).with_context(|| {
                format!(
                    "failed to publish raw source {} as {}",
                    source.display(),
                    target.display()
                )
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(record(source, target, digest, "copied"))
    }
}

fn record(source: PathBuf, backup: PathBuf, sha256: String, status: &str) -> RawSourceRecord {
    RawSourceRecord {
        source,
        backup,
        sha256,
        status: status.to_string(),
    }
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024].into_boxed_slice();
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to hash {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
