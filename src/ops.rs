//! Blocking operations on a profile, shared by every frontend. Never touches a UI and never calls `tr!`.

use anyhow::Result;

use crate::db::Store;
use crate::fetch;
use crate::profile::Paths;

#[derive(Clone)]
pub struct Core {
    pub paths: Paths,
    agent: ureq::Agent,
}

impl Core {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            agent: fetch::agent(),
        }
    }

    pub fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    /// A new connection to the profile's database.
    #[allow(dead_code)] // Nothing outside the tests calls it yet (phase 2, step 1).
    pub fn store(&self) -> Result<Store> {
        Store::open(&self.paths.db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_opens_the_store_repeatedly() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        core.store().unwrap();
        core.store().unwrap();
        assert!(core.paths.db.exists());
    }
}
