use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConanRef {
    pub name: String,
    pub version: String,
    pub user: String,
}

impl ConanRef {
    pub fn to_ref_string(&self) -> String {
        format!("{}/{}@{}", self.name, self.version, self.user)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMetadata {
    pub direct_pkg_modules: Vec<String>,
    pub shared_lib_patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadArtifact {
    pub arch: String,
    pub path: PathBuf,
}
