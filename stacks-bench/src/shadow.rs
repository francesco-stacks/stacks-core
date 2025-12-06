use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use tempfile::TempDir;

pub struct FileDeltaReport {
    pub path: PathBuf,
    pub size_delta_bytes: i64,
    pub was_modified: bool,
}

pub struct ShadowDirDeltaReport {
    pub net_growth_bytes: i64,
    pub estimated_bytes_written: u64,
    pub file_reports: Vec<FileDeltaReport>,
}

#[derive(Debug)]
pub struct ShadowDir {
    _tmp: TempDir,
    source: PathBuf,
    root: PathBuf,
    watched_files: Vec<PathBuf>,
}

impl AsRef<Path> for ShadowDir {
    fn as_ref(&self) -> &Path {
        &self.root
    }
}

impl ShadowDir {
    /// Prefix for temporary shadow directories.
    const TMP_PREFIX: &'static str = "stacks-bench-";

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn watch_file<P: AsRef<Path>>(&mut self, path: P) {
        self.watched_files.push(path.as_ref().to_path_buf());
    }

    /// Keep the temp directory instead of deleting it on drop, consuming this
    /// [`ShadowDir`] and returning its path.
    pub fn keep(self) -> PathBuf {
        self._tmp.keep()
    }

    /// Calculates the storage delta between a base directory and a shadow directory.
    /// Returns a detailed report.
    pub fn calculate_storage_delta(&self) -> std::io::Result<ShadowDirDeltaReport> {
        let base_root = &self.source;
        let shadow_root = &self.root;
        let mut net_growth: i64 = 0;
        let mut estimated_written: u64 = 0;
        let mut file_reports = Vec::new();

        // If watched files are defined, only check them, avoiding a full recursive directory walk
        // which is slow on large chainstates.
        if !self.watched_files.is_empty() {
            for relative_path in &self.watched_files {
                let shadow_path = shadow_root.join(relative_path);

                // We only care if the file exists in the shadow dir (it's the active state)
                if let Ok(shadow_meta) = fs::metadata(&shadow_path) {
                    let shadow_len = shadow_meta.len();
                    let shadow_modified = shadow_meta.modified()?;

                    let base_path = base_root.join(relative_path);

                    if let Ok(base_meta) = fs::metadata(&base_path) {
                        let base_len = base_meta.len();
                        let base_modified = base_meta.modified()?;

                        let diff = (shadow_len as i64) - (base_len as i64);
                        net_growth += diff;

                        // If modified time changed, the file was touched
                        let was_modified = base_modified != shadow_modified;
                        if was_modified {
                            // Count positive growth as written data
                            if diff > 0 {
                                estimated_written += diff as u64;
                            }
                        }

                        if was_modified || diff != 0 {
                            file_reports.push(FileDeltaReport {
                                path: relative_path.clone(),
                                size_delta_bytes: diff,
                                was_modified,
                            });
                        }
                    } else {
                        // New file created (or didn't exist in source)
                        net_growth += shadow_len as i64;
                        estimated_written += shadow_len;
                        file_reports.push(FileDeltaReport {
                            path: relative_path.clone(),
                            size_delta_bytes: shadow_len as i64,
                            was_modified: true,
                        });
                    }
                }
            }
            return Ok(ShadowDirDeltaReport {
                net_growth_bytes: net_growth,
                estimated_bytes_written: estimated_written,
                file_reports,
            });
        }

        // Use a stack for recursive directory traversal
        let mut stack = vec![shadow_root.to_path_buf()];

        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();

                if path.is_dir() {
                    stack.push(path);
                } else {
                    let shadow_meta = entry.metadata()?;
                    let shadow_len = shadow_meta.len();
                    let shadow_modified = shadow_meta.modified()?;

                    // Calculate relative path to find base file
                    let relative_path = path
                        .strip_prefix(shadow_root)
                        .map_err(std::io::Error::other)?
                        .to_path_buf();
                    let base_path = base_root.join(&relative_path);

                    if base_path.exists() {
                        let base_meta = fs::metadata(&base_path)?;
                        let base_len = base_meta.len();
                        let base_modified = base_meta.modified()?;

                        let diff = (shadow_len as i64) - (base_len as i64);
                        net_growth += diff;

                        // If modified time changed, the file was touched
                        let was_modified = base_modified != shadow_modified;
                        if was_modified {
                            // Count positive growth as written data
                            if diff > 0 {
                                estimated_written += diff as u64;
                            }
                        }

                        if was_modified || diff != 0 {
                            file_reports.push(FileDeltaReport {
                                path: relative_path,
                                size_delta_bytes: diff,
                                was_modified,
                            });
                        }
                    } else {
                        // New file created
                        net_growth += shadow_len as i64;
                        estimated_written += shadow_len;
                        file_reports.push(FileDeltaReport {
                            path: relative_path,
                            size_delta_bytes: shadow_len as i64,
                            was_modified: true,
                        });
                    }
                }
            }
        }

        Ok(ShadowDirDeltaReport {
            net_growth_bytes: net_growth,
            estimated_bytes_written: estimated_written,
            file_reports,
        })
    }
}

// Builder for ShadowDir using `ignore` glob filtering
#[derive(Debug)]
pub struct ShadowDirBuilder {
    source: PathBuf,
    globs: Vec<String>,
    allow_plain_copy: bool, // false => strict reflink
    watch_files: Vec<PathBuf>,
}

impl ShadowDirBuilder {
    pub fn new<P: Into<PathBuf>>(source: P) -> Self {
        Self {
            source: source.into(),
            globs: Vec::new(),
            allow_plain_copy: false,
            watch_files: Vec::new(),
        }
    }

    // Add a glob relative to the source root (e.g., "burnchain/**", "chainstate/**")
    pub fn glob<S: AsRef<str>>(mut self, pattern: S) -> Self {
        self.globs.push(pattern.as_ref().to_owned());
        self
    }

    // Allow plain copies (disables strict reflink requirement)
    pub fn allow_plain_copy(mut self) -> Self {
        self.allow_plain_copy = true;
        self
    }

    /// Watch a specific file for changes (relative path from source)
    pub fn watch<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.watch_files.push(path.as_ref().to_path_buf());
        self
    }

    // Execute the copy and return the ShadowDir
    pub fn copy(self) -> Result<ShadowDir> {
        let source = self.source;

        // Place tempdir on the same FS to maximize reflink success
        let parent = source.parent().unwrap_or_else(|| Path::new("/"));
        let tmp = tempfile::Builder::new()
            .prefix(ShadowDir::TMP_PREFIX)
            .tempdir_in(parent)
            .with_context(|| format!("failed to create tempdir under {}", parent.display()))?;
        let root = tmp.path().to_path_buf();

        // Strict mode: refuse if not same device
        #[cfg(unix)]
        if !self.allow_plain_copy {
            use std::os::unix::fs::MetadataExt;
            let src_dev = fs::metadata(&source)?.dev();
            let dst_dev = fs::metadata(&root)?.dev();
            if src_dev != dst_dev {
                use anyhow::bail;

                bail!(
                    "shadow tempdir ({}) is on a different filesystem than source ({}); \
                     reflinks will fail (use allow_plain_copy() to bypass)",
                    root.display(),
                    source.display()
                );
            }
        }

        // Build whitelist overrides (default: include everything)
        let mut ob = OverrideBuilder::new(&source);
        if self.globs.is_empty() {
            ob.add("**")?;
        } else {
            for pat in &self.globs {
                ob.add(pat)?;
            }
        }
        let overrides = ob.build()?;

        // Walk with ignore
        let walker = WalkBuilder::new(&source)
            .follow_links(false)
            .standard_filters(false)
            .hidden(false)
            .parents(false)
            .overrides(overrides)
            .build();

        fs::create_dir_all(&root).with_context(|| format!("mkdir {}", root.display()))?;

        for dent in walker {
            let dent = dent.map_err(|e| anyhow!("Walk error: {e}"))?;
            let path = dent.path();
            if path == source {
                continue;
            }

            // Determine type
            let ft = if let Some(t) = dent.file_type() {
                t
            } else {
                fs::metadata(path)
                    .map(|m| m.file_type())
                    .map_err(|e| anyhow!("stat {}: {e}", path.display()))?
            };

            let rel = path
                .strip_prefix(&source)
                .map_err(|e| anyhow!("strip_prefix {}: {e}", path.display()))?;
            let out = root.join(rel);

            if ft.is_dir() {
                fs::create_dir_all(&out).with_context(|| format!("mkdir {}", out.display()))?;
                continue;
            }

            #[cfg(unix)]
            if ft.is_symlink() {
                use anyhow::bail;

                bail!("Encountered symlink at {}, refuse to clone", path.display());
            }

            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }

            if ft.is_file() {
                if self.allow_plain_copy {
                    fs::copy(path, &out)
                        .with_context(|| format!("copy {} -> {}", path.display(), out.display()))?;
                } else {
                    reflink_copy::reflink(path, &out).with_context(|| {
                        format!(
                            "reflink {} -> {} failed (use allow_plain_copy() to fallback)",
                            path.display(),
                            out.display()
                        )
                    })?;
                }
            }
        }

        Ok(ShadowDir {
            _tmp: tmp,
            root,
            source,
            watched_files: self.watch_files,
        })
    }
}
