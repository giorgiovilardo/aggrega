//! Where a profile lives on disk: the database and the thumbnail cache, either
//! in the platform's directories or in one folder chosen with `AGGREGA_HOME`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone)]
pub struct Paths {
    pub db: PathBuf,
    pub thumbs: PathBuf,
}

impl Paths {
    /// `$AGGREGA_HOME` if set (database and thumbnails in one directory, for
    /// throwaway profiles), otherwise the platform's data and cache directories.
    pub fn new() -> Result<Self> {
        // Empty counts as unset, so `AGGREGA_HOME= aggrega` doesn't write into
        // the working directory.
        if let Some(home) = std::env::var_os("AGGREGA_HOME").filter(|v| !v.is_empty()) {
            let home = std::path::absolute(home)?;
            eprintln!("aggrega: using profile at {}", home.display());
            return Self::in_dir(&home);
        }
        let dirs =
            directories::ProjectDirs::from("", "", "aggrega").context("no home directory found")?;
        Self::create(dirs.data_dir(), dirs.cache_dir().join("thumbs"))
    }

    /// A self-contained profile: `aggrega.db` and `thumbs/` inside `dir`.
    pub(crate) fn in_dir(dir: &Path) -> Result<Self> {
        Self::create(dir, dir.join("thumbs"))
    }

    fn create(data: &Path, thumbs: PathBuf) -> Result<Self> {
        for dir in [data, &thumbs] {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(Self {
            db: data.join("aggrega.db"),
            thumbs,
        })
    }
}
