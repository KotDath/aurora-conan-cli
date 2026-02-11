use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const MODE_FILE: &str = ".aurora-conan-cli-mode.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProjectMode {
    Conan,
    Clear,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModeState {
    pub version: u32,
    pub mode: ProjectMode,
}

impl ModeState {
    pub fn new(mode: ProjectMode) -> Self {
        Self { version: 1, mode }
    }
}

pub fn mode_path(project_root: &Path) -> PathBuf {
    project_root.join(MODE_FILE)
}

pub fn save_mode(project_root: &Path, mode: ProjectMode) -> Result<()> {
    let path = mode_path(project_root);
    let payload = serde_json::to_string_pretty(&ModeState::new(mode))
        .context("Не удалось сериализовать mode-state")?;
    fs::write(&path, payload).with_context(|| format!("Не удалось записать {}", path.display()))
}

pub fn load_mode(project_root: &Path) -> Result<ProjectMode> {
    let path = mode_path(project_root);
    let payload = fs::read_to_string(&path)
        .with_context(|| format!("Не найден mode-state {}", path.display()))?;
    let state: ModeState = serde_json::from_str(&payload)
        .with_context(|| format!("Повреждён mode-state {}", path.display()))?;
    Ok(state.mode)
}

pub fn detect_mode(project_root: &Path, conanfile_name: &str) -> Result<ProjectMode> {
    if let Ok(mode) = load_mode(project_root) {
        return Ok(mode);
    }

    if project_root.join(conanfile_name).exists() {
        return Ok(ProjectMode::Conan);
    }
    if project_root.join("thirdparty").join("aurora").exists() {
        return Ok(ProjectMode::Clear);
    }

    Err(anyhow!(
        "Не удалось определить режим проекта. Выполните `aurora-conan-cli init` или `aurora-conan-cli init-clear`"
    ))
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use tempfile::tempdir;

    use super::{ProjectMode, detect_mode, load_mode, save_mode};

    #[test]
    fn saves_and_loads_mode_state() -> Result<()> {
        let dir = tempdir()?;
        save_mode(dir.path(), ProjectMode::Clear)?;
        let mode = load_mode(dir.path())?;
        assert_eq!(mode, ProjectMode::Clear);
        Ok(())
    }

    #[test]
    fn detects_conan_mode_by_conanfile() -> Result<()> {
        let dir = tempdir()?;
        std::fs::write(dir.path().join("conanfile.py"), "# test\n")?;

        let mode = detect_mode(dir.path(), "conanfile.py")?;
        assert_eq!(mode, ProjectMode::Conan);
        Ok(())
    }
}
