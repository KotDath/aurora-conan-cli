use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::blocking::Client;
use serde_json::Value;

use crate::connection::{self, Connection, ConnectionMode};
use crate::model::{ConanRef, ProjectMetadata};

const DEFAULT_USER: &str = "aurora";
const AURORA_DEVELOPER_BASE_URL: &str = "https://developer.auroraos.ru/";
const AURORA_DEVELOPER_USER_AGENT: &str = "aurora-conan-cli/0.1 (+https://developer.auroraos.ru)";

pub trait ConanProvider {
    fn resolve_direct_dependency(
        &self,
        project_root: &Path,
        name: &str,
        version: Option<&str>,
    ) -> Result<ConanRef>;

    fn resolve_project_metadata(
        &self,
        project_root: &Path,
        direct_refs: &[ConanRef],
    ) -> Result<ProjectMetadata>;
}

pub struct CliConanProvider;

impl ConanProvider for CliConanProvider {
    fn resolve_direct_dependency(
        &self,
        _project_root: &Path,
        name: &str,
        requested_version: Option<&str>,
    ) -> Result<ConanRef> {
        let available_versions = fetch_package_versions_from_portal(name).with_context(|| {
            format!("Не удалось получить список версий для {name} на developer.auroraos.ru")
        })?;

        let version = select_dependency_version(name, &available_versions, requested_version)?;

        Ok(ConanRef {
            name: name.to_string(),
            version,
            user: DEFAULT_USER.to_string(),
        })
    }

    fn resolve_project_metadata(
        &self,
        project_root: &Path,
        direct_refs: &[ConanRef],
    ) -> Result<ProjectMetadata> {
        if direct_refs.is_empty() {
            return Ok(ProjectMetadata {
                direct_pkg_modules: Vec::new(),
                shared_lib_patterns: Vec::new(),
            });
        }

        let mut direct_modules: Vec<String> = direct_refs.iter().map(|r| r.name.clone()).collect();
        direct_modules.sort();
        direct_modules.dedup();

        let graph_output = run_conan_with_connection(
            project_root,
            &[
                "graph".to_string(),
                "info".to_string(),
                project_root.display().to_string(),
                "--format".to_string(),
                "json".to_string(),
            ],
        )
        .context("Не удалось получить conan graph info")?;

        let graph_json: Value = serde_json::from_str(&graph_output)
            .context("Conan graph JSON имеет некорректный формат")?;

        let mut libs = BTreeSet::new();
        collect_libs(&graph_json, &mut libs);

        let mut patterns: Vec<String> = libs
            .into_iter()
            .map(|lib| {
                if lib.starts_with("lib") {
                    format!("{lib}.*")
                } else {
                    format!("lib{lib}.*")
                }
            })
            .collect();

        if patterns.is_empty() {
            patterns = direct_modules
                .iter()
                .map(|module| format!("lib{module}.*"))
                .collect();
        }

        patterns.sort();
        patterns.dedup();

        Ok(ProjectMetadata {
            direct_pkg_modules: direct_modules,
            shared_lib_patterns: patterns,
        })
    }
}

fn fetch_package_versions_from_portal(package_name: &str) -> Result<Vec<String>> {
    let client = Client::builder()
        .user_agent(AURORA_DEVELOPER_USER_AGENT)
        .build()
        .context("Не удалось инициализировать HTTP-клиент")?;

    let mut url = Url::parse(AURORA_DEVELOPER_BASE_URL)
        .context("Не удалось подготовить URL developer.auroraos.ru")?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("Некорректный базовый URL developer.auroraos.ru"))?
        .push("conan")
        .push(package_name);

    let response = client
        .get(url.clone())
        .send()
        .with_context(|| format!("Не удалось запросить {}", url.as_str()))?;

    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Err(anyhow!(
            "Пакет '{}' не найден: {} вернул 404",
            package_name,
            url.as_str()
        ));
    }

    if !status.is_success() {
        return Err(anyhow!(
            "Не удалось получить пакет '{}' из {}: HTTP {}",
            package_name,
            url.as_str(),
            status.as_u16()
        ));
    }

    let html = response
        .text()
        .context("Не удалось прочитать HTML-ответ страницы пакета")?;

    parse_package_versions_html(&html).with_context(|| {
        format!(
            "Не удалось извлечь список версий пакета '{}' из {}",
            package_name,
            url.as_str()
        )
    })
}

fn parse_package_versions_html(html: &str) -> Result<Vec<String>> {
    let section_re = Regex::new(r#"(?s)<h5>\s*Версия\s*</h5>\s*<select[^>]*>(.*?)</select>"#)
        .context("Не удалось подготовить regex для блока версий")?;

    let select_inner = section_re
        .captures(html)
        .and_then(|caps| caps.get(1).map(|m| m.as_str()))
        .ok_or_else(|| anyhow!("На странице пакета не найден блок 'Версия'"))?;

    let option_re = Regex::new(r#"<option[^>]*value=(?:"([^"]+)"|'([^']+)')"#)
        .context("Не удалось подготовить regex для option[value]")?;
    let option_text_re = Regex::new(r#"(?s)<option[^>]*>\s*([^<]+?)\s*</option>"#)
        .context("Не удалось подготовить regex для option-текста")?;

    let mut versions = Vec::new();

    for captures in option_re.captures_iter(select_inner) {
        let value = captures
            .get(1)
            .or_else(|| captures.get(2))
            .map(|m| m.as_str().trim())
            .unwrap_or_default();
        if !value.is_empty() && !versions.iter().any(|v| v == value) {
            versions.push(value.to_string());
        }
    }

    if versions.is_empty() {
        for captures in option_text_re.captures_iter(select_inner) {
            let value = captures
                .get(1)
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            if !value.is_empty() && !versions.iter().any(|v| v == value) {
                versions.push(value.to_string());
            }
        }
    }

    if versions.is_empty() {
        return Err(anyhow!("На странице пакета не найдено ни одной версии"));
    }

    Ok(versions)
}

fn select_dependency_version(
    package_name: &str,
    available_versions: &[String],
    requested_version: Option<&str>,
) -> Result<String> {
    if available_versions.is_empty() {
        return Err(anyhow!(
            "Для зависимости {} не найдено доступных версий",
            package_name
        ));
    }

    if let Some(version) = requested_version {
        if available_versions
            .iter()
            .any(|candidate| candidate == version)
        {
            return Ok(version.to_string());
        }

        return Err(anyhow!(
            "Для зависимости '{}' не найдена версия '{}'. Доступные версии: {}",
            package_name,
            version,
            available_versions.join(", ")
        ));
    }

    Ok(available_versions[0].clone())
}

fn run_conan_with_connection(_project_root: &Path, conan_args: &[String]) -> Result<String> {
    let connection = connection::load()?;
    let target = pick_aarch64_target(&connection)?;
    let conan_exec = resolve_conan_exec(&connection, &target)?;

    let mut cmd = vec!["sb2".to_string(), "-t".to_string(), target, conan_exec];
    cmd.extend(conan_args.iter().cloned());

    run_in_connected_env(&connection, &cmd)
}

fn pick_aarch64_target(connection: &Connection) -> Result<String> {
    let output = run_in_connected_env(
        connection,
        &["sdk-assistant".to_string(), "list".to_string()],
    )
    .context("Не удалось вызвать sdk-assistant list")?;

    let target_re = Regex::new(r"AuroraOS-\S*-aarch64")
        .context("Не удалось подготовить regex для поиска aarch64 target")?;

    target_re
        .find(&output)
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| {
            anyhow!("В выводе sdk-assistant list не найден target вида AuroraOS-*-aarch64")
        })
}

fn resolve_conan_exec(connection: &Connection, target: &str) -> Result<String> {
    let output = run_in_connected_env(
        connection,
        &[
            "sb2".to_string(),
            "-t".to_string(),
            target.to_string(),
            "bash".to_string(),
            "-lc".to_string(),
            "if command -v conan-with-aurora-profile >/dev/null 2>&1; then echo conan-with-aurora-profile; elif command -v conan >/dev/null 2>&1; then echo conan; else echo none; fi".to_string(),
        ],
    )
    .context("Не удалось определить исполняемый файл conan")?;

    let exec = output.trim();
    match exec {
        "conan-with-aurora-profile" | "conan" => Ok(exec.to_string()),
        _ => Err(anyhow!(
            "Не найден conan-with-aurora-profile/conan в подключенном окружении"
        )),
    }
}

fn run_in_connected_env(connection: &Connection, args: &[String]) -> Result<String> {
    match connection.mode {
        ConnectionMode::Sdk => run_in_sdk_over_ssh(connection, args),
        ConnectionMode::Psdk => run_in_psdk_chroot(connection, args),
    }
}

fn run_in_sdk_over_ssh(connection: &Connection, args: &[String]) -> Result<String> {
    let remote_cmd = args
        .iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ");
    run_ssh_script(connection, &format!("set -euo pipefail\n{}", remote_cmd))
}

fn run_ssh_script(connection: &Connection, script: &str) -> Result<String> {
    let key_path = connection
        .path
        .join("vmshare")
        .join("ssh")
        .join("private_keys")
        .join("sdk");
    if !key_path.exists() {
        return Err(anyhow!("Не найден SSH ключ {}", key_path.display()));
    }

    let remote = format!("bash -lc {}", shell_quote(script));
    let output = Command::new("ssh")
        .arg("-p")
        .arg("2232")
        .arg("-i")
        .arg(&key_path)
        .arg("mersdk@localhost")
        .arg(remote)
        .output()
        .context("Не удалось выполнить ssh-подключение к SDK")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "Команда в SDK завершилась с кодом {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_in_psdk_chroot(connection: &Connection, args: &[String]) -> Result<String> {
    let chroot = connection.path.join("sdk-chroot");
    if !chroot.exists() {
        return Err(anyhow!("Не найден {}", chroot.display()));
    }

    let output = Command::new(&chroot)
        .args(args)
        .output()
        .with_context(|| format!("Не удалось запустить {}", chroot.display()))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "Команда в PSDK завершилась с кодом {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{
        parse_package_versions_html, pick_aarch64_target, resolve_conan_exec,
        select_dependency_version,
    };
    use crate::connection::{Connection, ConnectionMode};

    #[test]
    fn resolves_conan_exec_inside_sb2_target() -> Result<()> {
        let tmp = tempdir()?;
        let chroot = tmp.path().join("sdk-chroot");

        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "sdk-assistant" && "${2:-}" == "list" ]]; then
  echo "AuroraOS-5.1.5.105-MB2-aarch64"
  exit 0
fi

if [[ "${1:-}" == "sb2" && "${2:-}" == "-t" ]]; then
  if [[ "${4:-}" == "bash" && "${5:-}" == "-lc" ]]; then
    if [[ "${6:-}" == *"command -v conan-with-aurora-profile"* ]]; then
      echo "conan-with-aurora-profile"
      exit 0
    fi
  fi
fi

echo "unexpected call: $*" >&2
exit 1
"#;
        fs::write(&chroot, script)?;
        let mut perms = fs::metadata(&chroot)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&chroot, perms)?;

        let connection = Connection {
            mode: ConnectionMode::Psdk,
            path: tmp.path().to_path_buf(),
        };

        let target = pick_aarch64_target(&connection)?;
        let exec = resolve_conan_exec(&connection, &target)?;
        assert_eq!(exec, "conan-with-aurora-profile");
        Ok(())
    }

    #[test]
    fn parses_versions_from_version_select_block() -> Result<()> {
        let html = r#"
            <div>
              <h5>Версия</h5>
              <select>
                <option value="1.18.1" selected>1.18.1</option>
                <option value="1.17.3">1.17.3</option>
              </select>
            </div>
        "#;

        let versions = parse_package_versions_html(html)?;
        assert_eq!(versions, vec!["1.18.1".to_string(), "1.17.3".to_string()]);
        Ok(())
    }

    #[test]
    fn fails_when_version_block_absent() {
        let html = "<html><body><h5>Лицензия</h5></body></html>";
        let error = parse_package_versions_html(html)
            .expect_err("ожидалась ошибка при отсутствии блока Версия");
        assert!(error.to_string().contains("блок 'Версия'"));
    }

    #[test]
    fn selects_requested_version_or_returns_error() {
        let versions = vec!["1.18.1".to_string(), "1.17.3".to_string()];

        let auto = select_dependency_version("onnxruntime", &versions, None)
            .expect("должна выбираться первая версия");
        assert_eq!(auto, "1.18.1");

        let exact = select_dependency_version("onnxruntime", &versions, Some("1.17.3"))
            .expect("должна выбираться явно указанная версия");
        assert_eq!(exact, "1.17.3");

        let missing = select_dependency_version("onnxruntime", &versions, Some("9.9.9"))
            .expect_err("должна быть ошибка на отсутствующую версию");
        assert!(missing.to_string().contains("не найдена версия '9.9.9'"));
    }
}

fn collect_libs(value: &Value, libs: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                if key == "libs" {
                    if let Value::Array(arr) = nested {
                        for item in arr {
                            if let Value::String(lib_name) = item {
                                let normalized = lib_name.trim();
                                if !normalized.is_empty() {
                                    libs.insert(normalized.to_string());
                                }
                            }
                        }
                    }
                }
                collect_libs(nested, libs);
            }
        }
        Value::Array(arr) => {
            for nested in arr {
                collect_libs(nested, libs);
            }
        }
        _ => {}
    }
}
