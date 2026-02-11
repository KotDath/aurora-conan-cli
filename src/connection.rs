use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const CONNECTION_DIR: &str = "aurora-conan-cli";
const CONNECTION_FILE: &str = "connection.json";
const STATE_DIR_ENV: &str = "AURORA_CONAN_CLI_STATE_DIR";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionMode {
    Sdk,
    Psdk,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Connection {
    pub mode: ConnectionMode,
    pub path: PathBuf,
}

pub fn connection_file() -> Result<PathBuf> {
    Ok(base_dir()?.join(CONNECTION_FILE))
}

pub fn save(connection: &Connection) -> Result<()> {
    let path = connection_file()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Не удалось создать {}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(connection)
        .context("Не удалось сериализовать параметры connect")?;
    fs::write(&path, json).with_context(|| format!("Не удалось записать {}", path.display()))?;
    Ok(())
}

pub fn load() -> Result<Connection> {
    let path = connection_file()?;
    let content = fs::read_to_string(&path).with_context(|| {
        format!(
            "Не найден connect state. Выполните aurora-conan-cli connect ({})",
            path.display()
        )
    })?;

    serde_json::from_str(&content).with_context(|| {
        format!(
            "Повреждён connect state в {}. Выполните connect заново",
            path.display()
        )
    })
}

pub fn clear() -> Result<()> {
    let path = connection_file()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow!("Не удалось удалить {}: {}", path.display(), err)),
    }
}

fn base_dir() -> Result<PathBuf> {
    if let Ok(override_dir) = env::var(STATE_DIR_ENV) {
        if !override_dir.trim().is_empty() {
            return Ok(Path::new(&override_dir).join(CONNECTION_DIR));
        }
    }

    let home = env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow!("Не удалось определить HOME для хранения connect state"))?;
    Ok(home.join(".config").join(CONNECTION_DIR))
}
