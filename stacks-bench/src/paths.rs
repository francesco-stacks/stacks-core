use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::db::app::AppDb;

pub struct AppDataDir(PathBuf);

impl AsRef<Path> for AppDataDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl TryFrom<PathBuf> for AppDataDir {
    type Error = anyhow::Error;

    fn try_from(value: PathBuf) -> Result<Self, Self::Error> {
        Ok(AppDataDir(value.canonicalize()?))
    }
}

impl TryFrom<&Path> for AppDataDir {
    type Error = anyhow::Error;

    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        Ok(AppDataDir(value.canonicalize()?))
    }
}

impl AppDataDir {
    pub const APP_DATA_DIR_NAME: &'static str = ".stacks-bench";

    /// Resolves the app data directory from the CLI `db_path` option.
    ///
    /// Logic:
    /// 1. If `custom_path` is provided:
    ///    - If it's a directory, use it.
    ///    - If it's a file, use its parent directory.
    /// 2. If not provided:
    ///    - Use `<exe_dir>/.stacks-bench` (creating it if missing).
    pub fn resolve_from_opt<P: AsRef<Path>>(custom_path: Option<P>) -> Result<Self> {
        if let Some(path) = custom_path {
            let path_ref = path.as_ref();
            if path_ref.is_dir() {
                return path_ref.try_into();
            }
            // It's a file path.
            let parent = path_ref.parent().unwrap_or_else(|| Path::new("."));
            let dir = if parent.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                parent.to_path_buf()
            };
            return dir.try_into();
        }

        // Default behavior: <exe_dir>/.stacks-bench
        let base_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        let storage_dir = base_dir.join(Self::APP_DATA_DIR_NAME);

        if !storage_dir.exists() {
            std::fs::create_dir_all(&storage_dir).with_context(|| {
                format!("Failed to create storage directory at {:?}", storage_dir)
            })?;
        }

        storage_dir.try_into()
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn as_str(&self) -> Result<&str> {
        self.path()
            .to_str()
            .ok_or(anyhow!("Failed to convert app data path to str"))
    }

    pub fn app_db_dir(&self) -> PathBuf {
        self.path().join("appdata")
    }

    pub fn app_db_path(&self) -> PathBuf {
        self.app_db_dir().join(AppDb::DEFAULT_DB_FILENAME)
    }

    pub fn postgres_data_dir(&self) -> PathBuf {
        self.path().join("pgdata")
    }
}

pub struct BurnChainDir(PathBuf);

impl BurnChainDir {
    pub const BURNCHAIN_DIR_NAME: &'static str = "burnchain";
    pub const SORTITION_DB_RELATIVE_FILE_PATH: &str = "sortition/marf.sqlite";

    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        BurnChainDir(path.into())
    }

    pub fn from_node_root<P: AsRef<Path>>(node_root: P) -> Self {
        Self::new(node_root.as_ref().join("burnchain"))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn as_str(&self) -> Result<&str> {
        self.path()
            .to_str()
            .ok_or(anyhow!("Failed to convert burnchain path to str"))
    }

    pub fn sortition_db_path(&self) -> PathBuf {
        self.path().join(Self::SORTITION_DB_RELATIVE_FILE_PATH)
    }
}

impl AsRef<Path> for BurnChainDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

pub struct ChainStateDir(PathBuf);

impl ChainStateDir {
    pub const CHAINSTATE_DIR_NAME: &'static str = "chainstate";
    pub const INDEX_DB_RELATIVE_FILE_PATH: &'static str = "vm/index.sqlite";

    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        ChainStateDir(path.into())
    }

    pub fn from_node_root<P: AsRef<Path>>(node_root: P) -> Self {
        Self::new(node_root.as_ref().join("chainstate"))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn as_str(&self) -> Result<&str> {
        self.path()
            .to_str()
            .ok_or(anyhow!("Failed to convert chainstate path to str"))
    }

    pub fn index_db_path(&self) -> PathBuf {
        self.path().join(Self::INDEX_DB_RELATIVE_FILE_PATH)
    }
}

impl AsRef<Path> for ChainStateDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}
