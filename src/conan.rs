use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::blocking::Client;
use serde_json::Value;

use crate::model::{ComponentInfo, ConanRef, DownloadArtifact, PackageCppInfo, ProjectMetadata};

const DEFAULT_USER: &str = "aurora";
const ERROR_VERSION: &str = "error";
const AURORA_DEVELOPER_BASE_URL: &str = "https://developer.auroraos.ru/";
const AURORA_DEVELOPER_USER_AGENT: &str = "aurora-conan-cli/0.1 (+https://developer.auroraos.ru)";
const AURORA_ARTIFACTORY_CONAN_STORAGE_URL: &str =
    "https://conan.omp.ru/artifactory/api/storage/public/aurora/";
const AURORA_ARTIFACTORY_PUBLIC_URL: &str = "https://conan.omp.ru/artifactory/public/aurora/";

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageDownloadSource {
    arch: String,
    download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageBinaryRecord {
    arch: String,
    download_url: String,
    requires: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum VersionMatcher {
    Exact(String),
    Prefix(String),
    CciFamily,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DependencyConstraint {
    name: String,
    matcher: VersionMatcher,
    user: Option<String>,
    raw: String,
}

trait DependencyDataSource {
    fn list_versions(&mut self, package_name: &str) -> Result<Vec<String>>;
    fn list_constraints(
        &mut self,
        package_name: &str,
        version: &str,
    ) -> Result<Vec<DependencyConstraint>>;
}

#[derive(Default)]
struct ArtifactoryDependencyDataSource {
    versions_cache: HashMap<String, Vec<String>>,
    constraints_cache: HashMap<(String, String), Vec<DependencyConstraint>>,
}

pub trait ConanProvider {
    fn list_dependency_versions(&self, name: &str) -> Result<Vec<String>>;
    fn search_dependencies(&self, query: &str) -> Result<Vec<ConanRef>>;
    fn download_dependency_archives(
        &self,
        package_name: &str,
        version: &str,
        destination_root: &Path,
    ) -> Result<Vec<DownloadArtifact>>;
    fn resolve_dependencies_without_conan(
        &self,
        package_name: &str,
        version: &str,
    ) -> Result<Vec<ConanRef>>;

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
    fn list_dependency_versions(&self, name: &str) -> Result<Vec<String>> {
        fetch_package_versions_from_artifactory(name)
            .with_context(|| format!("Не удалось получить список версий для {name} в JFrog"))
    }

    fn resolve_direct_dependency(
        &self,
        _project_root: &Path,
        name: &str,
        requested_version: Option<&str>,
    ) -> Result<ConanRef> {
        let available_versions = self.list_dependency_versions(name)?;

        let version = select_dependency_version(name, &available_versions, requested_version)?;

        Ok(ConanRef {
            name: name.to_string(),
            version,
            user: DEFAULT_USER.to_string(),
        })
    }

    fn search_dependencies(&self, query: &str) -> Result<Vec<ConanRef>> {
        let all_packages = fetch_all_package_names_from_artifactory()?;
        let matched_packages = filter_package_names_by_query(&all_packages, query);

        if matched_packages.is_empty() {
            return Err(anyhow!("По запросу '{}' пакеты не найдены в JFrog", query));
        }

        let mut refs = Vec::new();
        for package_name in matched_packages {
            let versions = self.list_dependency_versions(&package_name)?;
            for version in versions {
                refs.push(ConanRef {
                    name: package_name.clone(),
                    version,
                    user: DEFAULT_USER.to_string(),
                });
            }
        }

        Ok(refs)
    }

    fn download_dependency_archives(
        &self,
        package_name: &str,
        version: &str,
        destination_root: &Path,
    ) -> Result<Vec<DownloadArtifact>> {
        let sources = fetch_package_download_sources_from_artifactory(package_name, version)?;

        let download_dir = destination_root
            .join("downloads")
            .join(package_name)
            .join(version);
        fs::create_dir_all(&download_dir)
            .with_context(|| format!("Не удалось создать {}", download_dir.display()))?;

        let client = Client::builder()
            .user_agent(AURORA_DEVELOPER_USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(300))
            .build()
            .context("Не удалось инициализировать HTTP-клиент для загрузки архивов")?;

        let mut artifacts = Vec::new();
        for source in sources {
            let file_name = format!(
                "{}-{}-{}.tgz",
                package_name,
                version,
                sanitize_arch_for_filename(&source.arch)
            );
            let file_path = download_dir.join(file_name);

            let response = client
                .get(source.download_url.clone())
                .send()
                .with_context(|| format!("Не удалось скачать {}", source.download_url))?;

            let status = response.status();
            if !status.is_success() {
                return Err(anyhow!(
                    "Не удалось скачать {}: HTTP {}",
                    source.download_url,
                    status.as_u16()
                ));
            }

            let payload = response
                .bytes()
                .with_context(|| format!("Не удалось прочитать тело {}", source.download_url))?;
            fs::write(&file_path, payload.as_ref())
                .with_context(|| format!("Не удалось записать {}", file_path.display()))?;

            artifacts.push(DownloadArtifact {
                arch: source.arch,
                path: file_path,
            });
        }

        Ok(artifacts)
    }

    fn resolve_dependencies_without_conan(
        &self,
        package_name: &str,
        version: &str,
    ) -> Result<Vec<ConanRef>> {
        let mut source = ArtifactoryDependencyDataSource::default();
        resolve_dependency_graph(package_name, version, &mut source)
    }

    fn resolve_project_metadata(
        &self,
        _project_root: &Path,
        direct_refs: &[ConanRef],
    ) -> Result<ProjectMetadata> {
        if direct_refs.is_empty() {
            return Ok(ProjectMetadata {
                direct_pkg_modules: Vec::new(),
                shared_lib_patterns: Vec::new(),
                system_libs: Vec::new(),
            });
        }

        let mut direct_modules: Vec<String> = direct_refs.iter().map(|r| r.name.clone()).collect();
        direct_modules.sort();
        direct_modules.dedup();
        let mut all_packages = BTreeSet::new();
        for reference in direct_refs {
            all_packages.insert(reference.name.clone());
            let transitives =
                self.resolve_dependencies_without_conan(&reference.name, &reference.version)?;
            for dep in transitives {
                if dep.version != ERROR_VERSION {
                    all_packages.insert(dep.name);
                }
            }
        }

        let mut patterns = all_packages
            .into_iter()
            .map(|name| format!("lib{}.*", name))
            .collect::<Vec<_>>();
        patterns.sort();
        patterns.dedup();

        Ok(ProjectMetadata {
            direct_pkg_modules: direct_modules,
            shared_lib_patterns: patterns,
            system_libs: Vec::new(),
        })
    }
}

impl DependencyDataSource for ArtifactoryDependencyDataSource {
    fn list_versions(&mut self, package_name: &str) -> Result<Vec<String>> {
        if let Some(cached) = self.versions_cache.get(package_name) {
            return Ok(cached.clone());
        }

        let versions = fetch_package_versions_from_artifactory(package_name)?;
        if versions.is_empty() {
            return Err(anyhow!(
                "Для зависимости '{}' не найдено доступных версий",
                package_name
            ));
        }
        self.versions_cache
            .insert(package_name.to_string(), versions.clone());
        Ok(versions)
    }

    fn list_constraints(
        &mut self,
        package_name: &str,
        version: &str,
    ) -> Result<Vec<DependencyConstraint>> {
        let key = (package_name.to_string(), version.to_string());
        if let Some(cached) = self.constraints_cache.get(&key) {
            return Ok(cached.clone());
        }

        let parsed = fetch_dependency_constraints_from_artifactory(package_name, version)?;
        self.constraints_cache.insert(key, parsed.clone());
        Ok(parsed)
    }
}

fn fetch_package_page_html(package_name: &str) -> Result<String> {
    let client = Client::builder()
        .user_agent(AURORA_DEVELOPER_USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .build()
        .context("Не удалось инициализировать HTTP-клиент")?;

    let mut url = Url::parse(AURORA_DEVELOPER_BASE_URL)
        .context("Не удалось подготовить URL developer.auroraos.ru")?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("Некорректный базовый URL developer.auroraos.ru"))?
        .push("conan")
        .push(package_name);

    let response = send_get_with_retries(&client, &url)
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

    response
        .text()
        .context("Не удалось прочитать HTML-ответ страницы пакета")
}

fn fetch_package_versions_from_portal(package_name: &str) -> Result<Vec<String>> {
    let html = fetch_package_page_html(package_name)?;
    parse_package_versions_html(&html).with_context(|| {
        format!(
            "Не удалось извлечь список версий пакета '{}' со страницы /conan/{}",
            package_name, package_name
        )
    })
}

fn fetch_package_versions_from_artifactory(package_name: &str) -> Result<Vec<String>> {
    let payload = fetch_artifactory_storage_payload(&[package_name])?;
    parse_artifactory_storage_versions(&payload).with_context(|| {
        format!(
            "Не удалось извлечь список версий пакета '{}' из Artifactory storage API",
            package_name
        )
    })
}

fn fetch_all_package_names_from_artifactory() -> Result<Vec<String>> {
    let payload = fetch_artifactory_storage_payload(&[])?;
    let children = payload
        .get("children")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("В ответе Artifactory storage API не найден массив children"))?;

    let mut names = Vec::new();
    for child in children {
        let is_folder = child
            .get("folder")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_folder {
            continue;
        }

        let uri = child.get("uri").and_then(Value::as_str).unwrap_or_default();
        let normalized = uri.trim().trim_start_matches('/').trim();
        if normalized.is_empty() || normalized.starts_with('.') {
            continue;
        }

        if !names.iter().any(|existing| existing == normalized) {
            names.push(normalized.to_string());
        }
    }

    if names.is_empty() {
        return Err(anyhow!(
            "В Artifactory storage API не найдено ни одного пакета (children[].uri)"
        ));
    }

    names.sort();
    Ok(names)
}

fn fetch_artifactory_storage_payload(segments: &[&str]) -> Result<Value> {
    let client = artifactory_http_client()?;

    let mut url = Url::parse(AURORA_ARTIFACTORY_CONAN_STORAGE_URL)
        .context("Не удалось подготовить URL Artifactory storage API")?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow!("Некорректный базовый URL Artifactory storage API"))?;
        for segment in segments {
            if !segment.is_empty() {
                path.push(segment);
            }
        }
    }

    let response = client
        .get(url.clone())
        .send()
        .with_context(|| format!("Не удалось запросить {}", url.as_str()))?;

    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Err(anyhow!(
            "Ресурс Artifactory storage API не найден: {} (404)",
            url.as_str()
        ));
    }
    if !status.is_success() {
        return Err(anyhow!(
            "Не удалось получить данные из Artifactory storage API {}: HTTP {}",
            url.as_str(),
            status.as_u16()
        ));
    }

    let body = response
        .text()
        .context("Не удалось прочитать тело ответа Artifactory storage API")?;
    serde_json::from_str(&body).context("Не удалось разобрать JSON-ответ Artifactory storage API")
}

fn parse_artifactory_storage_versions(payload: &Value) -> Result<Vec<String>> {
    let children = payload
        .get("children")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("В ответе Artifactory storage API не найден массив children"))?;

    let mut versions = Vec::new();
    for child in children {
        let is_folder = child
            .get("folder")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_folder {
            continue;
        }

        let uri = child.get("uri").and_then(Value::as_str).unwrap_or_default();
        let normalized = uri.trim().trim_start_matches('/').trim();
        if normalized.is_empty() {
            continue;
        }
        if normalized.starts_with('.') {
            continue;
        }

        if !versions.iter().any(|existing| existing == normalized) {
            versions.push(normalized.to_string());
        }
    }

    if versions.is_empty() {
        return Err(anyhow!(
            "В Artifactory storage API не найдено ни одной версии (children[].uri)"
        ));
    }

    versions.sort_by(|a, b| b.cmp(a));
    Ok(versions)
}

fn fetch_package_download_sources_from_artifactory(
    package_name: &str,
    version: &str,
) -> Result<Vec<PackageDownloadSource>> {
    let binaries = fetch_package_binaries_from_artifactory(package_name, version)?;
    let mut sources = Vec::new();
    for item in binaries {
        sources.push(PackageDownloadSource {
            arch: item.arch,
            download_url: item.download_url,
        });
    }

    if sources.is_empty() {
        return Err(anyhow!(
            "Для пакета '{}' версии '{}' не найдено ни одного бинарного пакета в JFrog",
            package_name,
            version
        ));
    }
    Ok(sources)
}

fn fetch_dependency_constraints_from_artifactory(
    package_name: &str,
    version: &str,
) -> Result<Vec<DependencyConstraint>> {
    match try_parse_constraints_from_conanfile(package_name, version) {
        Ok(Some(parsed)) => return Ok(parsed),
        Ok(None) => {}
        Err(_) => {
            // Если recipe получить не удалось (сеть/таймаут), продолжаем через бинарные метаданные.
        }
    }

    let binaries = fetch_package_binaries_from_artifactory(package_name, version)?;
    let mut raw_refs = BTreeSet::new();

    for binary in binaries {
        for req in binary.requires {
            let normalized = normalize_dependency_ref(req.as_str());
            if !normalized.is_empty() {
                raw_refs.insert(normalized);
            }
        }
    }

    if raw_refs.is_empty() {
        let conanfile = fetch_conanfile_from_artifactory(package_name, version)?;
        for req in collect_requires_from_conanfile_text(&conanfile) {
            raw_refs.insert(req);
        }
    }

    let mut parsed = Vec::new();
    for raw in raw_refs {
        parsed.push(parse_dependency_constraint(&raw).with_context(|| {
            format!(
                "Не удалось разобрать зависимость '{}' пакета '{}' из JFrog",
                raw, package_name
            )
        })?);
    }

    parsed.sort_by(|a, b| a.name.cmp(&b.name).then(a.raw.cmp(&b.raw)));
    Ok(parsed)
}

fn try_parse_constraints_from_conanfile(
    package_name: &str,
    version: &str,
) -> Result<Option<Vec<DependencyConstraint>>> {
    let conanfile = fetch_conanfile_from_artifactory(package_name, version)?;
    let raw_refs = collect_requires_from_conanfile_text(&conanfile);
    if raw_refs.is_empty() {
        return Ok(None);
    }

    let mut parsed = Vec::new();
    for raw in raw_refs {
        match parse_dependency_constraint(&raw) {
            Ok(constraint) => parsed.push(constraint),
            Err(_) => return Ok(None),
        }
    }

    let mut by_package = HashSet::new();
    for item in &parsed {
        // Повторный requires одного пакета обычно означает условную логику рецепта,
        // которую без Conan корректнее брать из бинарного conaninfo.txt.
        if !by_package.insert(item.name.clone()) {
            return Ok(None);
        }
    }

    parsed.sort_by(|a, b| a.name.cmp(&b.name).then(a.raw.cmp(&b.raw)));
    Ok(Some(parsed))
}

fn fetch_package_binaries_from_artifactory(
    package_name: &str,
    version: &str,
) -> Result<Vec<PackageBinaryRecord>> {
    let rrev = fetch_latest_recipe_revision(package_name, version)?;
    let payload =
        fetch_artifactory_storage_payload(&[package_name, version, "_", &rrev, "package"])
            .with_context(|| {
                format!(
                    "Не удалось получить список бинарных пакетов для {}/{}#{}",
                    package_name, version, rrev
                )
            })?;
    let package_ids = parse_folder_children_uris(&payload);
    if package_ids.is_empty() {
        return Err(anyhow!(
            "Для пакета '{}' версии '{}' отсутствуют бинарные пакеты в JFrog",
            package_name,
            version
        ));
    }

    let client = artifactory_http_client()?;

    let mut result = Vec::new();
    for package_id in package_ids {
        let prev = fetch_latest_package_revision(package_name, version, &rrev, &package_id)?;
        let info_url = build_artifactory_public_url(&[
            package_name,
            version,
            "_",
            &rrev,
            "package",
            &package_id,
            &prev,
            "conaninfo.txt",
        ])?;
        let info_text = send_get_with_retries(client, &info_url)
            .with_context(|| format!("Не удалось запросить {}", info_url.as_str()))?
            .error_for_status()
            .with_context(|| format!("HTTP ошибка при чтении {}", info_url.as_str()))?
            .text()
            .with_context(|| format!("Не удалось прочитать {}", info_url.as_str()))?;

        let (arch, requires) = parse_conaninfo_text(&info_text);
        let download_url = build_artifactory_public_url(&[
            package_name,
            version,
            "_",
            &rrev,
            "package",
            &package_id,
            &prev,
            "conan_package.tgz",
        ])?
        .to_string();

        result.push(PackageBinaryRecord {
            arch,
            download_url,
            requires,
        });
    }

    Ok(result)
}

fn fetch_latest_recipe_revision(package_name: &str, version: &str) -> Result<String> {
    let payload =
        fetch_artifactory_storage_payload(&[package_name, version, "_"]).with_context(|| {
            format!(
                "Не удалось получить список recipe revisions для пакета '{}' версии '{}'",
                package_name, version
            )
        })?;
    let revisions = parse_folder_children_uris(&payload);
    let latest = revisions.first().cloned().ok_or_else(|| {
        anyhow!(
            "Не удалось выбрать recipe revision для пакета '{}' версии '{}'",
            package_name,
            version
        )
    })?;
    Ok(latest)
}

fn fetch_latest_package_revision(
    package_name: &str,
    version: &str,
    recipe_revision: &str,
    package_id: &str,
) -> Result<String> {
    let payload = fetch_artifactory_storage_payload(&[
        package_name,
        version,
        "_",
        recipe_revision,
        "package",
        package_id,
    ])
    .with_context(|| {
        format!(
            "Не удалось получить package revisions для пакета '{}' версии '{}'",
            package_name, version
        )
    })?;
    let revisions = parse_folder_children_uris(&payload);
    let latest = revisions.first().cloned().ok_or_else(|| {
        anyhow!(
            "Не удалось выбрать package revision для пакета '{}' версии '{}'",
            package_name,
            version
        )
    })?;
    Ok(latest)
}

fn parse_latest_revision_from_index(payload: &Value) -> Result<String> {
    let revisions = payload
        .get("revisions")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("В index.json не найден массив revisions"))?;

    let latest = revisions
        .first()
        .and_then(|item| item.get("revision"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if latest.is_empty() {
        return Err(anyhow!("В index.json отсутствует revisions[0].revision"));
    }
    Ok(latest.to_string())
}

fn parse_folder_children_uris(payload: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let children = payload
        .get("children")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for child in children {
        let is_folder = child
            .get("folder")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_folder {
            continue;
        }

        let uri = child.get("uri").and_then(Value::as_str).unwrap_or_default();
        let normalized = uri.trim().trim_start_matches('/').trim();
        if normalized.is_empty() || normalized.starts_with('.') {
            continue;
        }
        if !out.iter().any(|item| item == normalized) {
            out.push(normalized.to_string());
        }
    }
    out
}

fn build_artifactory_public_url(segments: &[&str]) -> Result<Url> {
    let mut url = Url::parse(AURORA_ARTIFACTORY_PUBLIC_URL)
        .context("Не удалось подготовить URL JFrog public repository")?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow!("Некорректный базовый URL JFrog public repository"))?;
        for segment in segments {
            if !segment.is_empty() {
                path.push(segment);
            }
        }
    }
    Ok(url)
}

fn fetch_json_by_url(url: &Url) -> Result<Value> {
    let client = artifactory_http_client()?;

    let body = send_get_with_retries(client, url)?
        .error_for_status()
        .with_context(|| format!("HTTP ошибка при чтении {}", url.as_str()))?
        .text()
        .with_context(|| format!("Не удалось прочитать {}", url.as_str()))?;

    serde_json::from_str(&body).with_context(|| format!("Некорректный JSON в {}", url.as_str()))
}

fn fetch_conanfile_from_artifactory(package_name: &str, version: &str) -> Result<String> {
    let rrev = fetch_latest_recipe_revision(package_name, version)?;
    let url = build_artifactory_public_url(&[
        package_name,
        version,
        "_",
        &rrev,
        "export",
        "conanfile.py",
    ])?;
    let client = artifactory_http_client()?;

    send_get_with_retries(client, &url)
        .with_context(|| format!("Не удалось запросить {}", url.as_str()))?
        .error_for_status()
        .with_context(|| format!("HTTP ошибка при чтении {}", url.as_str()))?
        .text()
        .with_context(|| format!("Не удалось прочитать {}", url.as_str()))
}

/// Извлекает cpp_info из conanfile.py пакета
pub fn fetch_cpp_info_from_artifactory(package_name: &str, version: &str) -> Result<PackageCppInfo> {
    let conanfile = fetch_conanfile_from_artifactory(package_name, version)?;
    Ok(parse_cpp_info_from_text(package_name, &conanfile))
}

/// Парсит cpp_info метаданные из текста conanfile.py
pub fn parse_cpp_info_from_text(package_name: &str, conanfile: &str) -> PackageCppInfo {
    let mut info = PackageCppInfo {
        package_name: package_name.to_string(),
        ..Default::default()
    };

    // Проверяем header-library
    let header_only_re = Regex::new(r#"package_type\s*=\s*["']header-library["']"#)
        .unwrap_or_else(|e| {
            eprintln!("Warning: failed to compile header_only regex: {e}");
            Regex::new(r"^$").unwrap()
        });
    info.is_header_only = header_only_re.is_match(conanfile);

    // Парсим корневые libs
    if let Some(libs) = parse_string_list(conanfile, r#"cpp_info\.libs\s*=\s*\[([^\]]*)\]"#) {
        info.libs = libs;
    }

    // Парсим корневые system_libs (= [...])
    if let Some(system_libs) = parse_string_list(conanfile, r#"cpp_info\.system_libs\s*=\s*\[([^\]]*)\]"#) {
        info.system_libs = system_libs;
    }

    // Парсим корневые system_libs.append("...")
    let append_re = Regex::new(r#"cpp_info\.system_libs\.append\(\s*["']([^"']+)["']\s*\)"#)
        .unwrap_or_else(|e| {
            eprintln!("Warning: failed to compile system_libs.append regex: {e}");
            Regex::new(r"^$").unwrap()
        });
    for caps in append_re.captures_iter(conanfile) {
        let lib = caps[1].to_string();
        if !info.system_libs.contains(&lib) {
            info.system_libs.push(lib);
        }
    }

    // Парсим корневые system_libs.extend([...])
    if let Some(system_libs) = parse_string_list(conanfile, r#"cpp_info\.system_libs\.extend\(\s*\[([^\]]*)\]\s*\)"#) {
        for lib in system_libs {
            if !info.system_libs.contains(&lib) {
                info.system_libs.push(lib);
            }
        }
    }

    // Парсим pkg_config_name для корня
    let pkg_name_re = Regex::new(r#"cpp_info\.set_property\(\s*["']pkg_config_name["']\s*,\s*["']([^"']+)["']\s*\)"#)
        .unwrap_or_else(|e| {
            eprintln!("Warning: failed to compile pkg_config_name regex: {e}");
            Regex::new(r"^$").unwrap()
        });
    if let Some(caps) = pkg_name_re.captures(conanfile) {
        info.pkg_config_name = Some(caps[1].to_string());
    }

    // Парсим компоненты
    info.components = parse_components(conanfile);

    info
}

/// Парсит список строк из Python-массива в conanfile
fn parse_string_list(content: &str, pattern: &str) -> Option<Vec<String>> {
    let re = Regex::new(pattern).ok()?;
    let caps = re.captures(content)?;
    let array_content = caps.get(1)?.as_str();

    let mut result = Vec::new();
    // Парсим строки в одинарных или двойных кавычках
    let string_re = Regex::new(r#"["']([^"']*)["']"#).ok()?;
    for str_caps in string_re.captures_iter(array_content) {
        let s = str_caps[1].trim().to_string();
        if !s.is_empty() && !result.contains(&s) {
            result.push(s);
        }
    }
    Some(result)
}

/// Парсит компоненты из conanfile.py
fn parse_components(conanfile: &str) -> Vec<ComponentInfo> {
    let mut components: Vec<ComponentInfo> = Vec::new();

    // Находим все объявления компонентов: cpp_info.components["name"]
    let component_decl_re = Regex::new(r#"cpp_info\.components\["([^"]+)"\]"#)
        .unwrap_or_else(|e| {
            eprintln!("Warning: failed to compile component_decl regex: {e}");
            Regex::new(r"^$").unwrap()
        });

    let mut component_names: Vec<String> = Vec::new();
    for caps in component_decl_re.captures_iter(conanfile) {
        let name = caps[1].to_string();
        if !component_names.contains(&name) {
            component_names.push(name);
        }
    }

    for name in component_names {
        let mut component = ComponentInfo {
            name: name.clone(),
            ..Default::default()
        };

        // Парсим libs компонента
        let libs_pattern = format!(
            r#"cpp_info\.components\["{}\"]\.libs\s*=\s*\[([^\]]*)\]"#,
            regex::escape(&name)
        );
        if let Some(libs) = parse_string_list(conanfile, &libs_pattern) {
            component.libs = libs;
        }

        // Парсим system_libs компонента (= [...])
        let sys_libs_pattern = format!(
            r#"cpp_info\.components\["{}\"]\.system_libs\s*=\s*\[([^\]]*)\]"#,
            regex::escape(&name)
        );
        if let Some(system_libs) = parse_string_list(conanfile, &sys_libs_pattern) {
            component.system_libs = system_libs;
        }

        // Парсим system_libs.append компонента
        let append_pattern = format!(
            r#"cpp_info\.components\["{}\"]\.system_libs\.append\(\s*["']([^"']+)["']\s*\)"#,
            regex::escape(&name)
        );
        let append_re = Regex::new(&append_pattern).unwrap_or_else(|e| {
            eprintln!("Warning: failed to compile component system_libs.append regex: {e}");
            Regex::new(r"^$").unwrap()
        });
        for caps in append_re.captures_iter(conanfile) {
            let lib = caps[1].to_string();
            if !component.system_libs.contains(&lib) {
                component.system_libs.push(lib);
            }
        }

        // Парсим system_libs.extend компонента
        let extend_pattern = format!(
            r#"cpp_info\.components\["{}\"]\.system_libs\.extend\(\s*\[([^\]]*)\]\s*\)"#,
            regex::escape(&name)
        );
        if let Some(system_libs) = parse_string_list(conanfile, &extend_pattern) {
            for lib in system_libs {
                if !component.system_libs.contains(&lib) {
                    component.system_libs.push(lib);
                }
            }
        }

        // Парсим pkg_config_name компонента
        let pkg_name_pattern = format!(
            r#"cpp_info\.components\["{}\"]\.set_property\(\s*["']pkg_config_name["']\s*,\s*["']([^"']+)["']\s*\)"#,
            regex::escape(&name)
        );
        let pkg_name_re = Regex::new(&pkg_name_pattern).unwrap_or_else(|e| {
            eprintln!("Warning: failed to compile component pkg_config_name regex: {e}");
            Regex::new(r"^$").unwrap()
        });
        if let Some(caps) = pkg_name_re.captures(conanfile) {
            component.pkg_config_name = Some(caps[1].to_string());
        }

        // Парсим requires компонента (= [...])
        let requires_pattern = format!(
            r#"cpp_info\.components\["{}\"]\.requires\s*=\s*\[([^\]]*)\]"#,
            regex::escape(&name)
        );
        if let Some(requires) = parse_string_list(conanfile, &requires_pattern) {
            component.requires = requires;
        }

        // Парсим requires.append компонента
        let req_append_pattern = format!(
            r#"cpp_info\.components\["{}\"]\.requires\.append\(\s*["']([^"']+)["']\s*\)"#,
            regex::escape(&name)
        );
        let req_append_re = Regex::new(&req_append_pattern).unwrap_or_else(|e| {
            eprintln!("Warning: failed to compile component requires.append regex: {e}");
            Regex::new(r"^$").unwrap()
        });
        for caps in req_append_re.captures_iter(conanfile) {
            let req = caps[1].to_string();
            if !component.requires.contains(&req) {
                component.requires.push(req);
            }
        }

        components.push(component);
    }

    components
}

fn parse_conaninfo_text(content: &str) -> (String, Vec<String>) {
    let mut current_section = "";
    let mut arch = String::new();
    let mut requires = BTreeSet::new();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && line.len() >= 3 {
            current_section = &line[1..line.len() - 1];
            continue;
        }

        if current_section == "settings"
            && line.starts_with("arch=")
            && arch.is_empty()
            && line.len() > "arch=".len()
        {
            arch = line["arch=".len()..].trim().to_string();
            continue;
        }

        if current_section == "requires" {
            let normalized = normalize_dependency_ref(line);
            if !normalized.is_empty() {
                requires.insert(normalized);
            }
        }
    }

    let arch = if arch.is_empty() {
        "package".to_string()
    } else {
        arch
    };
    (arch, requires.into_iter().collect())
}

fn artifactory_http_client() -> Result<&'static Client> {
    static CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();
    let init = CLIENT.get_or_init(|| {
        Client::builder()
            .user_agent(AURORA_DEVELOPER_USER_AGENT)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| error.to_string())
    });

    match init {
        Ok(client) => Ok(client),
        Err(error) => Err(anyhow!(
            "Не удалось инициализировать HTTP-клиент Artifactory: {error}"
        )),
    }
}

fn send_get_with_retries(client: &Client, url: &Url) -> Result<reqwest::blocking::Response> {
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 1..=3 {
        match client.get(url.clone()).send() {
            Ok(response) => {
                if response.status().is_server_error() && attempt < 3 {
                    thread::sleep(Duration::from_millis(200 * attempt as u64));
                    continue;
                }
                return Ok(response);
            }
            Err(error) => {
                last_error = Some(anyhow!(error));
                if attempt < 3 {
                    thread::sleep(Duration::from_millis(200 * attempt as u64));
                    continue;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        anyhow!(
            "Не удалось выполнить HTTP GET {} после нескольких попыток",
            url.as_str()
        )
    }))
}

fn normalize_dependency_ref(raw: &str) -> String {
    raw.trim()
        .split('#')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn fetch_all_package_names_from_portal() -> Result<Vec<String>> {
    let client = Client::builder()
        .user_agent(AURORA_DEVELOPER_USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .build()
        .context("Не удалось инициализировать HTTP-клиент")?;

    let mut url = Url::parse(AURORA_DEVELOPER_BASE_URL)
        .context("Не удалось подготовить URL developer.auroraos.ru")?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("Некорректный базовый URL developer.auroraos.ru"))?
        .push("conan");

    let response = client
        .get(url.clone())
        .send()
        .with_context(|| format!("Не удалось запросить {}", url.as_str()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!(
            "Не удалось получить список пакетов из {}: HTTP {}",
            url.as_str(),
            status.as_u16()
        ));
    }

    let html = response
        .text()
        .context("Не удалось прочитать HTML-ответ страницы со списком пакетов")?;

    parse_package_names_html(&html).with_context(|| {
        format!(
            "Не удалось извлечь список пакетов со страницы {}",
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

fn parse_package_download_sources(
    html: &str,
    package_name: &str,
    requested_version: &str,
) -> Result<Vec<PackageDownloadSource>> {
    let next_data = parse_next_data_json(html)?;
    let versions = parse_versions_from_next_data(&next_data)?;

    let version_node = versions
        .iter()
        .find(|item| {
            item.get("version")
                .and_then(Value::as_str)
                .is_some_and(|version| version == requested_version)
        })
        .ok_or_else(|| {
            let available: Vec<String> = versions
                .iter()
                .filter_map(|item| item.get("version").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect();
            anyhow!(
                "Для пакета '{}' не найдена версия '{}'. Доступные версии: {}",
                package_name,
                requested_version,
                available.join(", ")
            )
        })?;

    let packages = version_node
        .get("data")
        .and_then(|item| item.get("packages"))
        .and_then(Value::as_array)
        .or_else(|| version_node.get("packages").and_then(Value::as_array))
        .ok_or_else(|| {
            anyhow!(
                "Для пакета '{}' версии '{}' не найден раздел packages",
                package_name,
                requested_version
            )
        })?;

    let mut result = Vec::new();
    for package in packages {
        let download_url = package
            .get("downloadLink")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if download_url.is_empty() {
            continue;
        }

        let arch = package
            .get("conanInfo")
            .and_then(|value| value.get("settings"))
            .and_then(|value| value.get("arch"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("package")
            .to_string();

        result.push(PackageDownloadSource {
            arch,
            download_url: normalize_download_url(download_url),
        });
    }

    if result.is_empty() {
        return Err(anyhow!(
            "Для пакета '{}' версии '{}' не найдено ни одной ссылки для скачивания",
            package_name,
            requested_version
        ));
    }

    Ok(result)
}

fn parse_next_data_json(html: &str) -> Result<Value> {
    let next_data_re =
        Regex::new(r#"(?s)<script[^>]*id=["']__NEXT_DATA__["'][^>]*>(.*?)</script>"#)
            .context("Не удалось подготовить regex для __NEXT_DATA__")?;

    let payload = next_data_re
        .captures(html)
        .and_then(|caps| caps.get(1).map(|m| m.as_str()))
        .ok_or_else(|| anyhow!("На странице пакета не найден JSON __NEXT_DATA__"))?;

    serde_json::from_str(payload).context("Не удалось разобрать JSON из __NEXT_DATA__")
}

fn parse_versions_from_next_data(next_data: &Value) -> Result<Vec<Value>> {
    let page_props = next_data
        .get("props")
        .and_then(|value| value.get("pageProps"))
        .ok_or_else(|| anyhow!("В __NEXT_DATA__ не найден объект props.pageProps"))?;

    if let Some(versions) = page_props
        .get("data")
        .and_then(|value| value.get("versions"))
        .and_then(Value::as_array)
    {
        return Ok(versions.clone());
    }

    if let Some(versions) = page_props
        .get("currentPackage")
        .and_then(|value| value.get("packageVersions"))
        .and_then(Value::as_array)
    {
        return Ok(versions.clone());
    }

    Err(anyhow!(
        "В __NEXT_DATA__ не найден список версий (data.versions/currentPackage.packageVersions)"
    ))
}

fn sanitize_arch_for_filename(arch: &str) -> String {
    let sanitized: String = arch
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn normalize_download_url(raw_url: &str) -> String {
    let Ok(mut url) = Url::parse(raw_url) else {
        return raw_url.to_string();
    };

    if url.scheme() == "http" && url.host_str() == Some("conan.omp.ru") {
        if url.set_scheme("https").is_err() {
            return raw_url.to_string();
        }
        let _ = url.set_port(None);
        return url.to_string();
    }

    raw_url.to_string()
}

fn parse_package_names_html(html: &str) -> Result<Vec<String>> {
    let package_re = Regex::new(r#"href="/conan/([^"/?#]+)""#)
        .context("Не удалось подготовить regex для ссылок пакетов")?;

    let mut names: Vec<String> = Vec::new();
    for captures in package_re.captures_iter(html) {
        let name = captures
            .get(1)
            .map(|m| m.as_str().trim())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }

        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }

    if names.is_empty() {
        return Err(anyhow!(
            "На странице со списком пакетов не найдено ссылок /conan/<name>"
        ));
    }

    Ok(names)
}

fn filter_package_names_by_query(package_names: &[String], query: &str) -> Vec<String> {
    let query_norm = query.to_lowercase();

    let mut filtered: Vec<String> = package_names
        .iter()
        .filter(|name| name.to_lowercase().contains(&query_norm))
        .cloned()
        .collect();
    filtered.sort();
    filtered
}

fn resolve_dependency_graph(
    root_package: &str,
    root_version: &str,
    source: &mut dyn DependencyDataSource,
) -> Result<Vec<ConanRef>> {
    let debug_deps = std::env::var_os("AURORA_CONAN_DEBUG_DEPS").is_some();
    let root_versions = match source.list_versions(root_package) {
        Ok(versions) => versions,
        Err(error) => {
            if debug_deps {
                eprintln!(
                    "list_versions failed for root {}/{}: {error:#}",
                    root_package, root_version
                );
            }
            return Ok(vec![ConanRef {
                name: root_package.to_string(),
                version: ERROR_VERSION.to_string(),
                user: DEFAULT_USER.to_string(),
            }]);
        }
    };
    if !root_versions.iter().any(|item| item == root_version) {
        return Err(anyhow!(
            "Для пакета '{}' не найдена версия '{}'. Доступные версии: {}",
            root_package,
            root_version,
            root_versions.join(", ")
        ));
    }

    let mut constraints: HashMap<String, Vec<DependencyConstraint>> = HashMap::new();
    let mut selected: HashMap<String, ConanRef> = HashMap::new();
    let mut queue = VecDeque::new();
    let mut visited: HashSet<(String, String)> = HashSet::new();

    queue.push_back(ConanRef {
        name: root_package.to_string(),
        version: root_version.to_string(),
        user: DEFAULT_USER.to_string(),
    });

    while let Some(current) = queue.pop_front() {
        if !visited.insert((current.name.clone(), current.version.clone())) {
            continue;
        }

        if current.version == ERROR_VERSION {
            continue;
        }

        let dependency_constraints = match source.list_constraints(&current.name, &current.version)
        {
            Ok(constraints) => constraints,
            Err(error) => {
                if debug_deps {
                    eprintln!(
                        "list_constraints failed for {}/{}: {error:#}",
                        current.name, current.version
                    );
                }
                if !(current.name == root_package && current.version == root_version) {
                    // Если версия пакета уже определена, но не удалось раскрыть его транзитивы,
                    // сохраняем найденную версию и продолжаем резолв без углубления.
                    continue;
                }
                return Ok(vec![ConanRef {
                    name: root_package.to_string(),
                    version: ERROR_VERSION.to_string(),
                    user: DEFAULT_USER.to_string(),
                }]);
            }
        };
        for constraint in dependency_constraints {
            let package_name = constraint.name.clone();
            let package_constraints = constraints.entry(constraint.name.clone()).or_default();
            if !package_constraints.contains(&constraint) {
                package_constraints.push(constraint);
            }

            let resolved_user = resolve_user_for_constraints(&package_name, package_constraints)?;
            let resolved_version = if let Some(exact) =
                resolve_exact_without_remote_lookup(&package_name, package_constraints)?
            {
                exact
            } else {
                match source.list_versions(&package_name) {
                    Ok(available_versions) => select_version_for_constraints(
                        &package_name,
                        &available_versions,
                        package_constraints,
                    )?,
                    Err(_) => ERROR_VERSION.to_string(),
                }
            };

            let resolved_ref = ConanRef {
                name: package_name,
                version: resolved_version,
                user: resolved_user,
            };

            let should_enqueue = resolved_ref.version != ERROR_VERSION
                && selected
                    .get(&resolved_ref.name)
                    .is_none_or(|existing| existing.version != resolved_ref.version);
            selected.insert(resolved_ref.name.clone(), resolved_ref.clone());

            if should_enqueue {
                queue.push_back(resolved_ref);
            }
        }
    }

    let mut refs: Vec<ConanRef> = selected.into_values().collect();
    refs.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    Ok(refs)
}

fn resolve_user_for_constraints(
    package_name: &str,
    constraints: &[DependencyConstraint],
) -> Result<String> {
    let users: HashSet<&str> = constraints
        .iter()
        .filter_map(|item| item.user.as_deref())
        .collect();

    if users.len() > 1 {
        let mut sorted: Vec<&str> = users.into_iter().collect();
        sorted.sort();
        return Err(anyhow!(
            "Для пакета '{}' найдены конфликтующие users: {}",
            package_name,
            sorted.join(", ")
        ));
    }

    Ok(constraints
        .iter()
        .find_map(|item| item.user.clone())
        .unwrap_or_else(|| DEFAULT_USER.to_string()))
}

fn resolve_exact_without_remote_lookup(
    package_name: &str,
    constraints: &[DependencyConstraint],
) -> Result<Option<String>> {
    let exact_versions: HashSet<String> = constraints
        .iter()
        .filter_map(|item| match &item.matcher {
            VersionMatcher::Exact(version) => Some(version.clone()),
            _ => None,
        })
        .collect();

    if exact_versions.len() > 1 {
        let mut versions: Vec<String> = exact_versions.into_iter().collect();
        versions.sort();
        return Err(anyhow!(
            "Для пакета '{}' найдены конфликтующие точные версии: {}",
            package_name,
            versions.join(", ")
        ));
    }

    let Some(exact) = exact_versions.into_iter().next() else {
        return Ok(None);
    };

    if constraints
        .iter()
        .all(|item| matcher_satisfies(&item.matcher, &exact))
    {
        return Ok(Some(exact));
    }

    Err(anyhow!(
        "Для пакета '{}' нет пересечения ограничений для версии '{}'",
        package_name,
        exact
    ))
}

fn select_version_for_constraints(
    package_name: &str,
    available_versions: &[String],
    constraints: &[DependencyConstraint],
) -> Result<String> {
    for candidate in available_versions {
        if constraints
            .iter()
            .all(|constraint| matcher_satisfies(&constraint.matcher, candidate))
        {
            return Ok(candidate.to_string());
        }
    }

    let mut raw_constraints: Vec<String> = constraints.iter().map(|c| c.raw.clone()).collect();
    raw_constraints.sort();
    raw_constraints.dedup();
    Err(anyhow!(
        "Для пакета '{}' не удалось подобрать версию. Доступные версии: {}. Ограничения: {}",
        package_name,
        available_versions.join(", "),
        raw_constraints.join(", ")
    ))
}

fn matcher_satisfies(matcher: &VersionMatcher, candidate: &str) -> bool {
    match matcher {
        VersionMatcher::Exact(version) => candidate == version,
        VersionMatcher::Prefix(prefix) => {
            if candidate.starts_with(prefix) {
                return true;
            }

            let Some(mut base) = prefix.strip_suffix('.') else {
                return false;
            };
            if candidate == base {
                return true;
            }

            while let Some(stripped) = base.strip_suffix(".0") {
                if candidate == stripped {
                    return true;
                }
                base = stripped;
            }

            false
        }
        VersionMatcher::CciFamily => candidate == "cci" || candidate.starts_with("cci."),
    }
}

fn fetch_package_version_nodes_from_portal(package_name: &str) -> Result<Vec<Value>> {
    let html = fetch_package_page_html(package_name)?;
    let next_data = parse_next_data_json(&html)?;
    parse_versions_from_next_data(&next_data).with_context(|| {
        format!(
            "Не удалось извлечь версии для пакета '{}' из __NEXT_DATA__",
            package_name
        )
    })
}

fn extract_versions_from_nodes(version_nodes: &[Value]) -> Vec<String> {
    let mut versions = Vec::new();
    for item in version_nodes {
        if let Some(version) = item.get("version").and_then(Value::as_str) {
            let normalized = version.trim();
            if !normalized.is_empty() && !versions.iter().any(|v| v == normalized) {
                versions.push(normalized.to_string());
            }
        }
    }
    versions
}

fn find_version_node<'a>(
    version_nodes: &'a [Value],
    package_name: &str,
    requested_version: &str,
) -> Result<&'a Value> {
    version_nodes
        .iter()
        .find(|item| {
            item.get("version")
                .and_then(Value::as_str)
                .is_some_and(|version| version == requested_version)
        })
        .ok_or_else(|| {
            let available = extract_versions_from_nodes(version_nodes);
            anyhow!(
                "Для пакета '{}' не найдена версия '{}'. Доступные версии: {}",
                package_name,
                requested_version,
                available.join(", ")
            )
        })
}

fn parse_dependency_constraints_from_version_node(
    version_node: &Value,
    package_name: &str,
) -> Result<Vec<DependencyConstraint>> {
    let mut raw_refs = collect_requires_from_conan_info(version_node);
    if raw_refs.is_empty() {
        raw_refs = collect_requires_from_conanfile(version_node);
    }

    let mut parsed = Vec::new();
    for raw in raw_refs {
        parsed.push(parse_dependency_constraint(&raw).with_context(|| {
            format!(
                "Не удалось разобрать зависимость '{}' пакета '{}'",
                raw, package_name
            )
        })?);
    }

    parsed.sort_by(|a, b| a.name.cmp(&b.name).then(a.raw.cmp(&b.raw)));
    Ok(parsed)
}

fn collect_requires_from_conan_info(version_node: &Value) -> Vec<String> {
    let packages = version_node
        .get("data")
        .and_then(|item| item.get("packages"))
        .and_then(Value::as_array)
        .or_else(|| version_node.get("packages").and_then(Value::as_array));

    let mut refs = BTreeSet::new();
    if let Some(packages) = packages {
        for package in packages {
            if let Some(map) = package
                .get("conanInfo")
                .and_then(|item| item.get("requires"))
                .and_then(Value::as_object)
            {
                for key in map.keys() {
                    let normalized = key.trim();
                    if !normalized.is_empty() {
                        refs.insert(normalized.to_string());
                    }
                }
            }
        }
    }

    refs.into_iter().collect()
}

fn collect_requires_from_conanfile(version_node: &Value) -> Vec<String> {
    let conanfile = version_node
        .get("data")
        .and_then(|item| item.get("conanfile"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    collect_requires_from_conanfile_text(conanfile)
}

fn collect_requires_from_conanfile_text(conanfile: &str) -> Vec<String> {
    if conanfile.is_empty() {
        return Vec::new();
    }

    let requires_re = Regex::new(r#"self\.requires\(\s*f?["']([^"']+)["']"#)
        .expect("regex for self.requires must be valid");

    let mut refs = BTreeSet::new();
    for captures in requires_re.captures_iter(conanfile) {
        let candidate = captures
            .get(1)
            .map(|m| normalize_dependency_ref(m.as_str()))
            .unwrap_or_default();
        if !candidate.is_empty() {
            refs.insert(candidate);
        }
    }

    refs.into_iter().collect()
}

fn parse_dependency_constraint(raw_ref: &str) -> Result<DependencyConstraint> {
    let normalized = raw_ref.trim();
    let (name, tail) = normalized.split_once('/').ok_or_else(|| {
        anyhow!(
            "Ожидался формат name/version[@user], получено '{}'",
            raw_ref
        )
    })?;
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("Пустое имя пакета в '{}'", raw_ref));
    }

    let (version, user) = match tail.split_once('@') {
        Some((version, user)) => (version.trim(), Some(user.trim().to_string())),
        None => (tail.trim(), None),
    };

    if version.is_empty() {
        return Err(anyhow!("Пустая версия в '{}'", raw_ref));
    }

    let matcher = parse_version_matcher(version).with_context(|| {
        format!(
            "Не поддерживается формат версии '{}' без Conan resolver",
            version
        )
    })?;

    Ok(DependencyConstraint {
        name: name.to_string(),
        matcher,
        user: user.filter(|item| !item.is_empty()),
        raw: normalized.to_string(),
    })
}

fn parse_version_matcher(version: &str) -> Result<VersionMatcher> {
    if version == "cci" {
        return Ok(VersionMatcher::CciFamily);
    }

    if version.ends_with(".Z") {
        let prefix = version[..version.len() - 1].to_string();
        return Ok(VersionMatcher::Prefix(prefix));
    }

    let unsupported_markers = ['[', ']', '<', '>', '^', '~', '*', '{', '}', '(', ')', ' '];
    if version.chars().any(|ch| unsupported_markers.contains(&ch)) {
        return Err(anyhow!(
            "Сложные version ranges не поддерживаются: {}",
            version
        ));
    }

    Ok(VersionMatcher::Exact(version.to_string()))
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::{Result, anyhow};
    use serde_json::Value;

    use super::{
        DependencyConstraint, DependencyDataSource, VersionMatcher, filter_package_names_by_query,
        normalize_download_url, parse_artifactory_storage_versions, parse_dependency_constraint,
        parse_dependency_constraints_from_version_node, parse_latest_revision_from_index,
        parse_package_download_sources, parse_package_names_html, parse_package_versions_html,
        parse_version_matcher, parse_versions_from_next_data, resolve_dependency_graph,
        resolve_exact_without_remote_lookup, sanitize_arch_for_filename, select_dependency_version,
        select_version_for_constraints,
    };

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

    #[test]
    fn parses_package_names_from_conan_index_links() -> Result<()> {
        let html = r#"
            <a href="/conan/onnx">onnx</a>
            <a href="/conan/onnxruntime">onnxruntime</a>
            <a href="/conan/onnxruntime">onnxruntime duplicate</a>
            <a href="/conan">root link</a>
        "#;

        let names = parse_package_names_html(html)?;
        assert_eq!(names, vec!["onnx".to_string(), "onnxruntime".to_string()]);
        Ok(())
    }

    #[test]
    fn filters_package_names_by_substring_case_insensitive() {
        let packages = vec![
            "onnx".to_string(),
            "onnxruntime".to_string(),
            "ffmpeg".to_string(),
        ];

        let filtered = filter_package_names_by_query(&packages, "ONNX");
        assert_eq!(
            filtered,
            vec!["onnx".to_string(), "onnxruntime".to_string()]
        );
    }

    #[test]
    fn parses_versions_from_next_data_data_versions_layout() -> Result<()> {
        let json = serde_json::json!({
            "props": {
                "pageProps": {
                    "data": {
                        "versions": [
                            {"version": "1.0.0", "data": {"packages": []}},
                            {"version": "0.9.0", "data": {"packages": []}}
                        ]
                    }
                }
            }
        });

        let versions = parse_versions_from_next_data(&json)?;
        let got: Vec<String> = versions
            .iter()
            .filter_map(|item| item.get("version").and_then(Value::as_str))
            .map(ToString::to_string)
            .collect();

        assert_eq!(got, vec!["1.0.0".to_string(), "0.9.0".to_string()]);
        Ok(())
    }

    #[test]
    fn parses_download_sources_for_selected_version() -> Result<()> {
        let html = r#"
            <script id="__NEXT_DATA__" type="application/json">
            {"props":{"pageProps":{"data":{"versions":[
                {"version":"1.2.3","data":{"packages":[
                    {"downloadLink":"https://example.com/a.tgz","conanInfo":{"settings":{"arch":"armv7"}}},
                    {"downloadLink":"https://example.com/b.tgz","conanInfo":{"settings":{"arch":"x86_64"}}}
                ]}},
                {"version":"1.2.2","data":{"packages":[]}}
            ]}}}}
            </script>
        "#;

        let downloads = parse_package_download_sources(html, "demo", "1.2.3")?;
        assert_eq!(downloads.len(), 2);
        assert_eq!(downloads[0].arch, "armv7");
        assert_eq!(downloads[1].arch, "x86_64");
        Ok(())
    }

    #[test]
    fn parses_download_sources_uses_package_for_header_only_without_arch() -> Result<()> {
        let html = r#"
            <script id="__NEXT_DATA__" type="application/json">
            {"props":{"pageProps":{"data":{"versions":[
                {"version":"4.1.0","data":{"packages":[
                    {"downloadLink":"https://example.com/header-only.tgz","conanInfo":{"settings":{}}}
                ]}}
            ]}}}}
            </script>
        "#;

        let downloads = parse_package_download_sources(html, "ms-gsl", "4.1.0")?;
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].arch, "package");
        Ok(())
    }

    #[test]
    fn parses_dependency_constraints_from_conan_info_requires() -> Result<()> {
        let version_node = serde_json::json!({
            "version": "1.0.0",
            "data": {
                "packages": [
                    {
                        "conanInfo": {
                            "requires": {
                                "a/1.2.3@aurora": true,
                                "b/0.3.Z@aurora": true,
                                "c/cci@aurora": true
                            }
                        }
                    }
                ],
                "conanfile": "self.requires(\"ignored/9.9.9@aurora\")"
            }
        });

        let constraints = parse_dependency_constraints_from_version_node(&version_node, "demo")?;
        assert_eq!(constraints.len(), 3);
        assert_eq!(constraints[0].name, "a");
        assert_eq!(constraints[1].name, "b");
        assert_eq!(constraints[2].name, "c");
        Ok(())
    }

    #[test]
    fn parses_dependency_constraints_from_conanfile_fallback() -> Result<()> {
        let version_node = serde_json::json!({
            "version": "1.0.0",
            "data": {
                "packages": [
                    { "conanInfo": {} }
                ],
                "conanfile": "def requirements(self):\\n    self.requires(\"x/1.0.0@aurora\")\\n    self.requires('y/2.1.Z@aurora')\\n"
            }
        });

        let constraints = parse_dependency_constraints_from_version_node(&version_node, "demo")?;
        assert_eq!(constraints.len(), 2);
        assert_eq!(constraints[0].name, "x");
        assert_eq!(constraints[1].name, "y");
        Ok(())
    }

    #[test]
    fn parse_version_matcher_supports_exact_prefix_and_cci() -> Result<()> {
        assert_eq!(
            parse_version_matcher("1.2.3")?,
            VersionMatcher::Exact("1.2.3".to_string())
        );
        assert_eq!(
            parse_version_matcher("1.2.Z")?,
            VersionMatcher::Prefix("1.2.".to_string())
        );
        assert_eq!(parse_version_matcher("cci")?, VersionMatcher::CciFamily);
        Ok(())
    }

    #[test]
    fn select_version_for_constraints_intersects_constraints() -> Result<()> {
        let available = vec![
            "1.3.0".to_string(),
            "1.2.7".to_string(),
            "1.2.5".to_string(),
            "1.1.0".to_string(),
        ];

        let c1 = parse_dependency_constraint("demo/1.2.Z@aurora")?;
        let c2 = parse_dependency_constraint("demo/1.2.5@aurora")?;
        let selected = select_version_for_constraints("demo", &available, &[c1, c2])?;
        assert_eq!(selected, "1.2.5");
        Ok(())
    }

    #[test]
    fn select_version_for_constraints_allows_z_pattern_without_patch_tail() -> Result<()> {
        let available = vec!["20240116.2".to_string(), "20240116.1".to_string()];
        let constraint = parse_dependency_constraint("abseil/20240116.1.Z@aurora")?;
        let selected = select_version_for_constraints("abseil", &available, &[constraint])?;
        assert_eq!(selected, "20240116.1");
        Ok(())
    }

    #[test]
    fn select_version_for_constraints_allows_z_pattern_with_zero_segment() -> Result<()> {
        let available = vec!["20240702".to_string(), "20231101".to_string()];
        let constraint = parse_dependency_constraint("re2/20231101.0.Z@aurora")?;
        let selected = select_version_for_constraints("re2", &available, &[constraint])?;
        assert_eq!(selected, "20231101");
        Ok(())
    }

    #[test]
    fn resolve_exact_without_remote_lookup_uses_exact_if_compatible() -> Result<()> {
        let constraints = vec![
            parse_dependency_constraint("abseil/20240116.1@aurora")?,
            parse_dependency_constraint("abseil/20240116.1.Z@aurora")?,
        ];
        let selected = resolve_exact_without_remote_lookup("abseil", &constraints)?;
        assert_eq!(selected.as_deref(), Some("20240116.1"));
        Ok(())
    }

    #[test]
    fn resolve_exact_without_remote_lookup_fails_for_conflicting_exact_versions() -> Result<()> {
        let constraints = vec![
            parse_dependency_constraint("abseil/20240116.1@aurora")?,
            parse_dependency_constraint("abseil/20240116.2@aurora")?,
        ];
        let err = resolve_exact_without_remote_lookup("abseil", &constraints)
            .expect_err("expected conflicting exact versions");
        assert!(err.to_string().contains("конфликтующие точные версии"));
        Ok(())
    }

    #[test]
    fn select_version_for_constraints_fails_when_no_intersection() -> Result<()> {
        let available = vec!["1.3.0".to_string(), "1.2.7".to_string()];
        let c1 = parse_dependency_constraint("demo/1.2.Z@aurora")?;
        let c2 = parse_dependency_constraint("demo/1.1.0@aurora")?;
        let err = select_version_for_constraints("demo", &available, &[c1, c2])
            .expect_err("expected constraint conflict");
        assert!(err.to_string().contains("не удалось подобрать версию"));
        Ok(())
    }

    struct FakeDependencyDataSource {
        versions_by_package: HashMap<String, Vec<String>>,
        constraints_by_ref: HashMap<(String, String), Vec<DependencyConstraint>>,
    }

    impl DependencyDataSource for FakeDependencyDataSource {
        fn list_versions(&mut self, package_name: &str) -> Result<Vec<String>> {
            self.versions_by_package
                .get(package_name)
                .cloned()
                .ok_or_else(|| anyhow!("unknown package {}", package_name))
        }

        fn list_constraints(
            &mut self,
            package_name: &str,
            version: &str,
        ) -> Result<Vec<DependencyConstraint>> {
            self.constraints_by_ref
                .get(&(package_name.to_string(), version.to_string()))
                .cloned()
                .ok_or_else(|| anyhow!("unknown package ref {}/{}", package_name, version))
        }
    }

    #[test]
    fn resolve_dependency_graph_resolves_transitives_and_shared_constraints() -> Result<()> {
        let mut source = FakeDependencyDataSource {
            versions_by_package: HashMap::from([
                (
                    "root".to_string(),
                    vec!["1.0.0".to_string(), "0.9.0".to_string()],
                ),
                (
                    "a".to_string(),
                    vec![
                        "1.4.0".to_string(),
                        "1.3.2".to_string(),
                        "1.2.0".to_string(),
                    ],
                ),
                (
                    "b".to_string(),
                    vec!["2.5.1".to_string(), "2.5.0".to_string()],
                ),
            ]),
            constraints_by_ref: HashMap::from([
                (
                    ("root".to_string(), "1.0.0".to_string()),
                    vec![
                        parse_dependency_constraint("a/1.3.Z@aurora")?,
                        parse_dependency_constraint("b/2.5.Z@aurora")?,
                    ],
                ),
                (
                    ("a".to_string(), "1.3.2".to_string()),
                    vec![parse_dependency_constraint("b/2.5.0@aurora")?],
                ),
                (("a".to_string(), "1.4.0".to_string()), Vec::new()),
                (("b".to_string(), "2.5.1".to_string()), Vec::new()),
                (("b".to_string(), "2.5.0".to_string()), Vec::new()),
            ]),
        };

        let resolved = resolve_dependency_graph("root", "1.0.0", &mut source)?;
        let got: Vec<String> = resolved.into_iter().map(|r| r.to_ref_string()).collect();
        assert_eq!(
            got,
            vec!["a/1.3.2@aurora".to_string(), "b/2.5.0@aurora".to_string(),]
        );
        Ok(())
    }

    #[test]
    fn resolve_dependency_graph_reports_conflicts() -> Result<()> {
        let mut source = FakeDependencyDataSource {
            versions_by_package: HashMap::from([
                ("root".to_string(), vec!["1.0.0".to_string()]),
                ("a".to_string(), vec!["1.0.0".to_string()]),
                (
                    "b".to_string(),
                    vec!["2.0.0".to_string(), "1.0.0".to_string()],
                ),
            ]),
            constraints_by_ref: HashMap::from([
                (
                    ("root".to_string(), "1.0.0".to_string()),
                    vec![
                        parse_dependency_constraint("a/1.0.0@aurora")?,
                        parse_dependency_constraint("b/2.0.0@aurora")?,
                    ],
                ),
                (
                    ("a".to_string(), "1.0.0".to_string()),
                    vec![parse_dependency_constraint("b/1.0.0@aurora")?],
                ),
                (("b".to_string(), "2.0.0".to_string()), Vec::new()),
                (("b".to_string(), "1.0.0".to_string()), Vec::new()),
            ]),
        };

        let err = resolve_dependency_graph("root", "1.0.0", &mut source)
            .expect_err("expected conflict for b");
        assert!(err.to_string().contains("конфликтующие точные версии"));
        Ok(())
    }

    #[test]
    fn resolve_dependency_graph_marks_unavailable_dependency_as_error() -> Result<()> {
        let mut source = FakeDependencyDataSource {
            versions_by_package: HashMap::from([("root".to_string(), vec!["1.0.0".to_string()])]),
            constraints_by_ref: HashMap::from([(
                ("root".to_string(), "1.0.0".to_string()),
                vec![parse_dependency_constraint("blocked/1.2.Z@aurora")?],
            )]),
        };

        let resolved = resolve_dependency_graph("root", "1.0.0", &mut source)?;
        let got: Vec<String> = resolved.into_iter().map(|r| r.to_ref_string()).collect();
        assert_eq!(got, vec!["blocked/error@aurora".to_string()]);
        Ok(())
    }

    #[test]
    fn resolve_dependency_graph_marks_failed_transitive_expansion_as_error() -> Result<()> {
        let mut source = FakeDependencyDataSource {
            versions_by_package: HashMap::from([
                ("root".to_string(), vec!["1.0.0".to_string()]),
                ("blocked".to_string(), vec!["1.2.3".to_string()]),
            ]),
            constraints_by_ref: HashMap::from([(
                ("root".to_string(), "1.0.0".to_string()),
                vec![parse_dependency_constraint("blocked/1.2.3@aurora")?],
            )]),
        };

        let resolved = resolve_dependency_graph("root", "1.0.0", &mut source)?;
        let got: Vec<String> = resolved.into_iter().map(|r| r.to_ref_string()).collect();
        assert_eq!(got, vec!["blocked/1.2.3@aurora".to_string()]);
        Ok(())
    }

    #[test]
    fn resolve_dependency_graph_returns_root_error_when_root_unavailable() -> Result<()> {
        let mut source = FakeDependencyDataSource {
            versions_by_package: HashMap::new(),
            constraints_by_ref: HashMap::new(),
        };

        let resolved = resolve_dependency_graph("root", "1.0.0", &mut source)?;
        let got: Vec<String> = resolved.into_iter().map(|r| r.to_ref_string()).collect();
        assert_eq!(got, vec!["root/error@aurora".to_string()]);
        Ok(())
    }

    #[test]
    fn sanitize_arch_for_filename_replaces_invalid_chars() {
        assert_eq!(sanitize_arch_for_filename("armv8"), "armv8");
        assert_eq!(sanitize_arch_for_filename("arm/v8"), "arm_v8");
    }

    #[test]
    fn normalize_download_url_switches_conan_host_to_https() {
        let http = "http://conan.omp.ru:80/artifactory/public/aurora/pkg/1.0/_/r/package/p/r/conan_package.tgz";
        let normalized = normalize_download_url(http);
        assert!(normalized.starts_with("https://conan.omp.ru/"));
    }

    #[test]
    fn parses_artifactory_storage_versions() -> Result<()> {
        let payload = serde_json::json!({
            "children": [
                {"uri": "/3.3.3", "folder": true},
                {"uri": "/3.2.3", "folder": true},
                {"uri": "/.timestamp", "folder": false},
                {"uri": "/1.1.1w", "folder": true}
            ]
        });

        let versions = parse_artifactory_storage_versions(&payload)?;
        assert_eq!(
            versions,
            vec![
                "3.3.3".to_string(),
                "3.2.3".to_string(),
                "1.1.1w".to_string()
            ]
        );
        Ok(())
    }

    #[test]
    fn parses_latest_revision_from_index() -> Result<()> {
        let payload = serde_json::json!({
            "revisions": [
                {"revision": "newest", "time": "2026-01-01T00:00:00.000+0000"},
                {"revision": "older", "time": "2025-01-01T00:00:00.000+0000"}
            ]
        });

        let revision = parse_latest_revision_from_index(&payload)?;
        assert_eq!(revision, "newest");
        Ok(())
    }

    #[test]
    fn test_parse_simple_cpp_info() {
        let conanfile = r#"
from conan import ConanFile

class SomePackage(ConanFile):
    name = "testpkg"
    version = "1.0.0"

    def package_info(self):
        self.cpp_info.libs = ["testpkg"]
"#;
        let info = super::parse_cpp_info_from_text("testpkg", conanfile);
        assert_eq!(info.package_name, "testpkg");
        assert_eq!(info.libs, vec!["testpkg"]);
        assert!(!info.is_header_only);
        assert!(info.components.is_empty());
    }

    #[test]
    fn test_parse_system_libs() {
        let conanfile = r#"
from conan import ConanFile

class SomePackage(ConanFile):
    def package_info(self):
        self.cpp_info.libs = ["mylib"]
        self.cpp_info.system_libs = ["pthread", "dl"]
        self.cpp_info.system_libs.append("m")
        self.cpp_info.system_libs.extend(["rt", "dl"])
"#;
        let info = super::parse_cpp_info_from_text("mylib", conanfile);
        assert_eq!(info.libs, vec!["mylib"]);
        // dl appears twice but should only be once
        assert!(info.system_libs.contains(&"pthread".to_string()));
        assert!(info.system_libs.contains(&"dl".to_string()));
        assert!(info.system_libs.contains(&"m".to_string()));
        assert!(info.system_libs.contains(&"rt".to_string()));
    }

    #[test]
    fn test_parse_components() {
        let conanfile = r#"
from conan import ConanFile

class OpensslConan(ConanFile):
    def package_info(self):
        self.cpp_info.components["ssl"].libs = ["ssl"]
        self.cpp_info.components["ssl"].requires = ["crypto"]
        self.cpp_info.components["ssl"].set_property("pkg_config_name", "libssl")

        self.cpp_info.components["crypto"].libs = ["crypto"]
        self.cpp_info.components["crypto"].system_libs = ["pthread", "dl", "rt"]
        self.cpp_info.components["crypto"].set_property("pkg_config_name", "libcrypto")
"#;
        let info = super::parse_cpp_info_from_text("openssl", conanfile);
        assert_eq!(info.components.len(), 2);

        let crypto = info.components.iter().find(|c| c.name == "crypto").expect("crypto component");
        assert_eq!(crypto.libs, vec!["crypto"]);
        assert_eq!(crypto.system_libs, vec!["pthread", "dl", "rt"]);
        assert_eq!(crypto.pkg_config_name, Some("libcrypto".to_string()));

        let ssl = info.components.iter().find(|c| c.name == "ssl").expect("ssl component");
        assert_eq!(ssl.libs, vec!["ssl"]);
        assert_eq!(ssl.requires, vec!["crypto"]);
        assert_eq!(ssl.pkg_config_name, Some("libssl".to_string()));
    }

    #[test]
    fn test_parse_pkg_config_name() {
        let conanfile = r#"
from conan import ConanFile

class CurlConan(ConanFile):
    def package_info(self):
        self.cpp_info.libs = ["curl"]
        self.cpp_info.set_property("pkg_config_name", "libcurl")
"#;
        let info = super::parse_cpp_info_from_text("libcurl", conanfile);
        assert_eq!(info.pkg_config_name, Some("libcurl".to_string()));
    }

    #[test]
    fn test_parse_header_only() {
        let conanfile = r#"
from conan import ConanFile

class HeaderOnlyConan(ConanFile):
    package_type = "header-library"

    def package_info(self):
        self.cpp_info.bindirs = []
        self.cpp_info.libdirs = []
"#;
        let info = super::parse_cpp_info_from_text("header_only", conanfile);
        assert!(info.is_header_only);
        assert!(info.libs.is_empty());
    }

    #[test]
    fn test_parse_component_system_libs_append() {
        let conanfile = r#"
from conan import ConanFile

class SomeConan(ConanFile):
    def package_info(self):
        self.cpp_info.components["main"].libs = ["main"]
        self.cpp_info.components["main"].system_libs.append("pthread")
        self.cpp_info.components["main"].system_libs.append("dl")
"#;
        let info = super::parse_cpp_info_from_text("some", conanfile);
        assert_eq!(info.components.len(), 1);
        let comp = &info.components[0];
        assert!(comp.system_libs.contains(&"pthread".to_string()));
        assert!(comp.system_libs.contains(&"dl".to_string()));
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
