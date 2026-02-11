use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use tar::Archive;

use crate::model::{ConanRef, DownloadArtifact};

const ROOT_DIR: &str = "thirdparty/aurora";
const MANIFEST_FILE: &str = "manifest.lock.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClearManifest {
    pub version: u32,
    pub direct_requires: Vec<ConanRef>,
}

impl Default for ClearManifest {
    fn default() -> Self {
        Self {
            version: 1,
            direct_requires: Vec::new(),
        }
    }
}

pub fn thirdparty_root(project_root: &Path) -> PathBuf {
    project_root.join(ROOT_DIR)
}

pub fn manifest_path(project_root: &Path) -> PathBuf {
    thirdparty_root(project_root).join(MANIFEST_FILE)
}

pub fn ensure_layout(project_root: &Path) -> Result<()> {
    fs::create_dir_all(thirdparty_root(project_root)).with_context(|| {
        format!(
            "Не удалось создать {}",
            thirdparty_root(project_root).display()
        )
    })
}

pub fn load_manifest(project_root: &Path) -> Result<ClearManifest> {
    let path = manifest_path(project_root);
    if !path.exists() {
        return Ok(ClearManifest::default());
    }

    let payload = fs::read_to_string(&path)
        .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
    serde_json::from_str(&payload)
        .with_context(|| format!("Не удалось разобрать {}", path.display()))
}

pub fn save_manifest(project_root: &Path, manifest: &ClearManifest) -> Result<()> {
    ensure_layout(project_root)?;
    let path = manifest_path(project_root);
    let payload = serde_json::to_string_pretty(manifest)
        .context("Не удалось сериализовать clear manifest")?;
    fs::write(&path, payload).with_context(|| format!("Не удалось записать {}", path.display()))
}

pub fn normalize_arch(input: &str) -> Result<String> {
    let arch = input.trim().to_ascii_lowercase();
    match arch.as_str() {
        "armv8" | "aarch64" => Ok("armv8".to_string()),
        "armv7" | "armv7hl" => Ok("armv7".to_string()),
        "x86_64" | "amd64" => Ok("x86_64".to_string()),
        "package" => Ok("package".to_string()),
        _ => Err(anyhow!("Неподдерживаемая архитектура: {}", input)),
    }
}

pub fn supported_arches() -> &'static [&'static str] {
    &["armv7", "armv8", "x86_64"]
}

pub fn resolve_target_arches() -> Result<(Vec<String>, bool)> {
    if let Ok(value) = std::env::var("AURORA_CONAN_ARCH") {
        if !value.trim().is_empty() {
            return Ok((vec![normalize_arch(&value)?], true));
        }
    }
    if let Ok(value) = std::env::var("RPM_ARCH") {
        if !value.trim().is_empty() {
            return Ok((vec![normalize_arch(&value)?], true));
        }
    }

    Ok((
        supported_arches()
            .iter()
            .map(|item| item.to_string())
            .collect(),
        false,
    ))
}

pub fn arch_root(project_root: &Path, arch: &str) -> PathBuf {
    thirdparty_root(project_root).join(arch)
}

pub fn package_root(project_root: &Path, arch: &str, package: &str, version: &str) -> PathBuf {
    arch_root(project_root, arch)
        .join("packages")
        .join(package)
        .join(version)
}

pub fn pkgconfig_dir(project_root: &Path, arch: &str) -> PathBuf {
    arch_root(project_root, arch).join("pkgconfig")
}

pub fn reset_arch_layout(project_root: &Path, arch: &str) -> Result<()> {
    let root = arch_root(project_root, arch);
    if root.exists() {
        fs::remove_dir_all(&root)
            .with_context(|| format!("Не удалось удалить {}", root.display()))?;
    }

    fs::create_dir_all(root.join("packages"))
        .with_context(|| format!("Не удалось создать {}", root.join("packages").display()))?;
    fs::create_dir_all(root.join("pkgconfig"))
        .with_context(|| format!("Не удалось создать {}", root.join("pkgconfig").display()))?;
    Ok(())
}

pub fn choose_artifact<'a>(
    artifacts: &'a [DownloadArtifact],
    target_arch: &str,
) -> Result<&'a DownloadArtifact> {
    let target_norm = normalize_arch(target_arch)?;

    if let Some(exact) = artifacts.iter().find(|item| {
        normalize_arch(&item.arch)
            .map(|value| value == target_norm)
            .unwrap_or(false)
    }) {
        return Ok(exact);
    }

    if let Some(header_only) = artifacts.iter().find(|item| {
        normalize_arch(&item.arch)
            .map(|value| value == "package")
            .unwrap_or(false)
    }) {
        return Ok(header_only);
    }

    let available = artifacts
        .iter()
        .map(|item| item.arch.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(anyhow!(
        "Не найден артефакт для архитектуры '{}'. Доступные: {}",
        target_norm,
        available
    ))
}

pub fn extract_tgz(archive_path: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        fs::remove_dir_all(destination)
            .with_context(|| format!("Не удалось очистить {}", destination.display()))?;
    }
    fs::create_dir_all(destination)
        .with_context(|| format!("Не удалось создать {}", destination.display()))?;

    let bytes = fs::read(archive_path)
        .with_context(|| format!("Не удалось прочитать {}", archive_path.display()))?;
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(decoder);
    archive
        .unpack(destination)
        .with_context(|| format!("Не удалось распаковать {}", archive_path.display()))
}

pub fn discover_lib_names(package_prefix: &Path) -> Result<Vec<String>> {
    let lib_dir = package_prefix.join("lib");
    if !lib_dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for entry in fs::read_dir(&lib_dir)
        .with_context(|| format!("Не удалось прочитать {}", lib_dir.display()))?
    {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("lib") {
            continue;
        }
        if !name.contains(".so") {
            continue;
        }

        let stem = name.split(".so").next().unwrap_or(name);
        if stem.len() <= 3 {
            continue;
        }
        let short = stem[3..].to_string();
        if !out.iter().any(|v| v == &short) {
            out.push(short);
        }
    }

    out.sort();
    Ok(out)
}

pub fn write_pkg_config(
    project_root: &Path,
    arch: &str,
    package: &ConanRef,
    libs: &[String],
    requires: &[String],
) -> Result<()> {
    let pkg_dir = pkgconfig_dir(project_root, arch);
    fs::create_dir_all(&pkg_dir)
        .with_context(|| format!("Не удалось создать {}", pkg_dir.display()))?;

    let path = pkg_dir.join(format!("{}.pc", package.name));
    let prefix_rel = format!(
        "${{pcfiledir}}/../packages/{}/{}",
        package.name, package.version
    );

    let mut body = String::new();
    body.push_str(&format!("prefix={}\n", prefix_rel));
    body.push_str("includedir=${prefix}/include\n");
    body.push_str("libdir=${prefix}/lib\n\n");
    body.push_str(&format!("Name: {}\n", package.name));
    body.push_str(&format!(
        "Description: {} {} (vendored by aurora-conan-cli)\n",
        package.name, package.version
    ));
    body.push_str(&format!("Version: {}\n", package.version));

    if !requires.is_empty() {
        body.push_str(&format!("Requires: {}\n", requires.join(", ")));
    }

    body.push_str("Cflags: -I${includedir}\n");
    if libs.is_empty() {
        body.push_str("Libs:\n");
    } else {
        let flags = libs
            .iter()
            .map(|lib| format!("-l{}", lib))
            .collect::<Vec<_>>()
            .join(" ");
        body.push_str(&format!("Libs: -L${{libdir}} {}\n", flags));
    }

    fs::write(&path, body).with_context(|| format!("Не удалось записать {}", path.display()))
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use tempfile::tempdir;

    use super::{ClearManifest, choose_artifact, load_manifest, normalize_arch, save_manifest};
    use crate::model::DownloadArtifact;

    #[test]
    fn normalizes_arch_values() -> Result<()> {
        assert_eq!(normalize_arch("aarch64")?, "armv8");
        assert_eq!(normalize_arch("armv7hl")?, "armv7");
        assert_eq!(normalize_arch("x86_64")?, "x86_64");
        Ok(())
    }

    #[test]
    fn chooses_matching_or_header_only_artifact() -> Result<()> {
        let items = vec![
            DownloadArtifact {
                arch: "package".to_string(),
                path: "/tmp/header.tgz".into(),
            },
            DownloadArtifact {
                arch: "armv8".to_string(),
                path: "/tmp/armv8.tgz".into(),
            },
        ];

        let chosen = choose_artifact(&items, "aarch64")?;
        assert!(chosen.path.to_string_lossy().contains("armv8"));
        Ok(())
    }

    #[test]
    fn persists_manifest() -> Result<()> {
        let dir = tempdir()?;
        let manifest = ClearManifest::default();
        save_manifest(dir.path(), &manifest)?;
        let loaded = load_manifest(dir.path())?;
        assert_eq!(loaded.version, 1);
        Ok(())
    }
}
