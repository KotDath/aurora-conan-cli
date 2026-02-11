use std::io::{self, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use console::style;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::clear_store::{self, ClearManifest};
use crate::conan::ConanProvider;
use crate::connection::{self, Connection, ConnectionMode};
use crate::files;
use crate::mode::{self, ProjectMode};
use crate::model::{ConanRef, ProjectMetadata};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Connect {
        mode: Option<String>,
        dir: Option<String>,
    },
    Disconnect,
    Init,
    InitClear,
    Add {
        dependency: String,
        version: Option<String>,
    },
    Remove {
        dependency: String,
    },
    Search {
        dependency: String,
    },
    Download {
        dependency: String,
        version: String,
    },
    Deps {
        dependency: String,
        version: String,
    },
}

pub fn run(provider: &dyn ConanProvider, project_root: &Path, command: CliCommand) -> Result<()> {
    match command {
        CliCommand::Connect { mode, dir } => connect(mode, dir)?,
        CliCommand::Disconnect => disconnect()?,
        CliCommand::Init => init_conan_mode(project_root)?,
        CliCommand::InitClear => init_clear_mode(project_root)?,
        CliCommand::Add {
            dependency,
            version,
        } => add_dependency(provider, project_root, &dependency, version.as_deref())?,
        CliCommand::Remove { dependency } => {
            remove_dependency(provider, project_root, &dependency)?
        }
        CliCommand::Search { dependency } => {
            let matches = provider.search_dependencies(&dependency)?;
            for reference in matches {
                println!("{}", reference.to_ref_string());
            }
        }
        CliCommand::Download {
            dependency,
            version,
        } => {
            let downloaded =
                provider.download_dependency_archives(&dependency, &version, project_root)?;
            for artifact in downloaded {
                println!("{} {}", artifact.arch, artifact.path.display());
            }
        }
        CliCommand::Deps {
            dependency,
            version,
        } => {
            let dependencies =
                provider.resolve_dependencies_without_conan(&dependency, &version)?;
            for reference in dependencies {
                println!("{}", reference.to_ref_string());
            }
        }
    }

    Ok(())
}

fn init_conan_mode(project_root: &Path) -> Result<()> {
    ensure_project_files_exist(project_root)?;
    mode::save_mode(project_root, ProjectMode::Conan)?;
    files::write_conanfile(project_root, &[])?;
    apply_conan_changes(
        project_root,
        &ProjectMetadata {
            direct_pkg_modules: Vec::new(),
            shared_lib_patterns: Vec::new(),
        },
    )?;
    Ok(())
}

fn init_clear_mode(project_root: &Path) -> Result<()> {
    ensure_project_files_exist(project_root)?;
    mode::save_mode(project_root, ProjectMode::Clear)?;
    clear_store::ensure_layout(project_root)?;
    clear_store::save_manifest(project_root, &ClearManifest::default())?;

    let conanfile = project_root.join(files::CONANFILE);
    if conanfile.exists() {
        std::fs::remove_file(&conanfile)
            .with_context(|| format!("Не удалось удалить {}", conanfile.display()))?;
    }

    apply_clear_changes(
        project_root,
        &ProjectMetadata {
            direct_pkg_modules: Vec::new(),
            shared_lib_patterns: Vec::new(),
        },
    )?;
    Ok(())
}

fn add_dependency(
    provider: &dyn ConanProvider,
    project_root: &Path,
    dependency: &str,
    version: Option<&str>,
) -> Result<()> {
    let progress = create_progress_bar(4, format!("add {}{}", dependency, version_suffix(version)));
    progress_step(&progress, "Validating project structure");
    ensure_project_files_exist(project_root)?;

    progress_step(&progress, "Detecting project mode");
    let mode = mode::detect_mode(project_root, files::CONANFILE)?;

    progress_step(&progress, "Resolving dependency version");
    let resolved = provider.resolve_direct_dependency(project_root, dependency, version)?;
    log_info(
        Some(&progress),
        &format!(
            "Applying dependency {} in {} mode",
            resolved.to_ref_string(),
            mode_label(mode)
        ),
    );

    progress_step(&progress, "Applying project changes");
    match mode {
        ProjectMode::Conan => {
            let mut current = files::read_requires(project_root)?;
            upsert_reference(&mut current, resolved);
            files::write_conanfile(project_root, &current)?;
            let metadata = provider.resolve_project_metadata(project_root, &current)?;
            apply_conan_changes(project_root, &metadata)?;
        }
        ProjectMode::Clear => {
            let mut manifest = clear_store::load_manifest(project_root)?;
            upsert_reference(&mut manifest.direct_requires, resolved);
            clear_store::save_manifest(project_root, &manifest)?;
            sync_clear_mode(
                provider,
                project_root,
                &manifest.direct_requires,
                Some(&progress),
            )?;
        }
    }

    progress.finish_with_message(format!(
        "{} add completed: {}{}",
        style("✔").green(),
        dependency,
        version_suffix(version)
    ));
    log_success(Some(&progress), "Dependency added successfully");
    Ok(())
}

fn remove_dependency(
    provider: &dyn ConanProvider,
    project_root: &Path,
    dependency: &str,
) -> Result<()> {
    let progress = create_progress_bar(4, format!("remove {}", dependency));
    progress_step(&progress, "Validating project structure");
    ensure_project_files_exist(project_root)?;

    progress_step(&progress, "Detecting project mode");
    let mode = mode::detect_mode(project_root, files::CONANFILE)?;
    log_info(
        Some(&progress),
        &format!(
            "Removing dependency {} in {} mode",
            dependency,
            mode_label(mode)
        ),
    );

    progress_step(&progress, "Updating dependency state");
    match mode {
        ProjectMode::Conan => {
            let mut current = files::read_requires(project_root)?;
            let before = current.len();
            current.retain(|item| item.name != dependency);
            if current.len() == before {
                return Err(anyhow!(
                    "Зависимость {} не найдена в conanfile.py",
                    dependency
                ));
            }

            files::write_conanfile(project_root, &current)?;
            let metadata = if current.is_empty() {
                ProjectMetadata {
                    direct_pkg_modules: Vec::new(),
                    shared_lib_patterns: Vec::new(),
                }
            } else {
                provider.resolve_project_metadata(project_root, &current)?
            };
            apply_conan_changes(project_root, &metadata)?;
        }
        ProjectMode::Clear => {
            let mut manifest = clear_store::load_manifest(project_root)?;
            let before = manifest.direct_requires.len();
            manifest
                .direct_requires
                .retain(|item| item.name != dependency);
            if manifest.direct_requires.len() == before {
                return Err(anyhow!(
                    "Зависимость {} не найдена в thirdparty manifest",
                    dependency
                ));
            }

            clear_store::save_manifest(project_root, &manifest)?;
            sync_clear_mode(
                provider,
                project_root,
                &manifest.direct_requires,
                Some(&progress),
            )?;
        }
    }

    progress_step(&progress, "Finalizing");
    progress.finish_with_message(format!(
        "{} remove completed: {}",
        style("✔").green(),
        dependency
    ));
    log_success(Some(&progress), "Dependency removed successfully");
    Ok(())
}

fn sync_clear_mode(
    provider: &dyn ConanProvider,
    project_root: &Path,
    direct_refs: &[ConanRef],
    main_progress: Option<&ProgressBar>,
) -> Result<()> {
    log_info(main_progress, "Syncing clear package store");
    let (target_arches, strict_arch_mode) = clear_store::resolve_target_arches()?;
    log_info(
        main_progress,
        &format!("Target architectures: {}", target_arches.join(", ")),
    );
    for arch in &target_arches {
        clear_store::reset_arch_layout(project_root, arch)?;
    }

    if direct_refs.is_empty() {
        apply_clear_changes(
            project_root,
            &ProjectMetadata {
                direct_pkg_modules: Vec::new(),
                shared_lib_patterns: Vec::new(),
            },
        )?;
        log_info(
            main_progress,
            "Clear sync completed: no direct dependencies",
        );
        return Ok(());
    }

    log_info(main_progress, "Building full dependency graph");
    let all_refs = build_full_dependency_set(provider, direct_refs)?;
    log_info(
        main_progress,
        &format!(
            "Resolved package count (direct + transitive): {}",
            all_refs.len()
        ),
    );
    let mut lib_patterns = Vec::new();
    let arch_ops_total = (all_refs.len() * target_arches.len()).max(1) as u64;
    let package_progress =
        create_progress_bar(arch_ops_total, "Downloading and extracting packages");

    for reference in &all_refs {
        log_info(
            Some(&package_progress),
            &format!("Processing {}", reference.to_ref_string()),
        );
        let artifacts = provider.download_dependency_archives(
            &reference.name,
            &reference.version,
            project_root,
        )?;
        let mut installed_any = false;
        for arch in &target_arches {
            package_progress.set_message(format!("{} -> {}", reference.to_ref_string(), arch));
            let selected = match clear_store::choose_artifact(&artifacts, arch) {
                Ok(item) => item,
                Err(error) => {
                    if strict_arch_mode {
                        return Err(error).with_context(|| {
                            format!(
                                "Не найден артефакт пакета {}/{} для архитектуры {}",
                                reference.name, reference.version, arch
                            )
                        });
                    }
                    package_progress.inc(1);
                    continue;
                }
            };

            installed_any = true;
            let package_dir =
                clear_store::package_root(project_root, arch, &reference.name, &reference.version);
            clear_store::extract_tgz(&selected.path, &package_dir)?;

            let libs = clear_store::discover_lib_names(&package_dir)?;
            for lib in &libs {
                let pattern = format!("lib{}.*", lib);
                if !lib_patterns.iter().any(|item| item == &pattern) {
                    lib_patterns.push(pattern);
                }
            }
            clear_store::write_pkg_config(project_root, arch, reference, &libs, &[])?;
            package_progress.inc(1);
        }

        if !installed_any {
            return Err(anyhow!(
                "Для пакета '{}' версии '{}' не найдено ни одного подходящего архива",
                reference.name,
                reference.version
            ));
        }
    }
    package_progress.finish_with_message(format!("{} packages synced", style("✔").green()));

    let mut pkg_modules = all_refs
        .iter()
        .map(|item| item.name.clone())
        .collect::<Vec<_>>();
    pkg_modules.sort();
    pkg_modules.dedup();

    lib_patterns.sort();
    lib_patterns.dedup();
    let module_count = pkg_modules.len();
    let pattern_count = lib_patterns.len();

    apply_clear_changes(
        project_root,
        &ProjectMetadata {
            direct_pkg_modules: pkg_modules,
            shared_lib_patterns: lib_patterns,
        },
    )?;
    log_info(
        main_progress,
        &format!(
            "Updated CMake/spec: modules={}, shared_lib_patterns={}",
            module_count, pattern_count
        ),
    );
    Ok(())
}

fn build_full_dependency_set(
    provider: &dyn ConanProvider,
    direct_refs: &[ConanRef],
) -> Result<Vec<ConanRef>> {
    let mut all = direct_refs.to_vec();

    for direct in direct_refs {
        let resolved =
            provider.resolve_dependencies_without_conan(&direct.name, &direct.version)?;
        for item in resolved {
            if item.version == "error" {
                return Err(anyhow!(
                    "Не удалось определить версию транзитивной зависимости {}",
                    item.name
                ));
            }

            if let Some(existing) = all.iter().find(|ref_item| ref_item.name == item.name) {
                if existing.version != item.version {
                    return Err(anyhow!(
                        "Конфликт версий зависимости '{}': {} и {}",
                        item.name,
                        existing.version,
                        item.version
                    ));
                }
                continue;
            }
            all.push(item);
        }
    }

    all.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    Ok(all)
}

fn connect(mode: Option<String>, dir: Option<String>) -> Result<()> {
    let mode = match mode {
        Some(value) => parse_mode(&value)?,
        None => prompt_mode()?,
    };

    let path_value = match dir {
        Some(value) => value,
        None => prompt_path()?,
    };

    let path = Path::new(&path_value)
        .canonicalize()
        .with_context(|| format!("Не удалось определить путь {}", path_value))?;
    if !path.is_dir() {
        return Err(anyhow!("Путь {} не является директорией", path.display()));
    }

    match mode {
        ConnectionMode::Sdk => {
            let key = path
                .join("vmshare")
                .join("ssh")
                .join("private_keys")
                .join("sdk");
            if !key.exists() {
                return Err(anyhow!("Для sdk-режима не найден ключ {}", key.display()));
            }
        }
        ConnectionMode::Psdk => {
            let chroot = path.join("sdk-chroot");
            if !chroot.exists() {
                return Err(anyhow!("Для psdk-режима не найден {}", chroot.display()));
            }
        }
    }

    let connection = Connection { mode, path };
    connection::save(&connection)?;
    Ok(())
}

fn disconnect() -> Result<()> {
    connection::clear()
}

fn ensure_project_files_exist(project_root: &Path) -> Result<()> {
    let cmake = project_root.join(files::CMAKE_FILE);
    if !cmake.exists() {
        return Err(anyhow!("Не найден {}", cmake.display()));
    }

    files::find_spec_file(project_root)?;
    Ok(())
}

fn upsert_reference(references: &mut Vec<ConanRef>, new_ref: ConanRef) {
    if let Some(existing) = references.iter_mut().find(|item| item.name == new_ref.name) {
        *existing = new_ref;
    } else {
        references.push(new_ref);
    }

    references.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
}

fn apply_conan_changes(project_root: &Path, metadata: &ProjectMetadata) -> Result<()> {
    files::update_cmake(project_root, metadata)?;
    files::update_spec(project_root, metadata)?;
    Ok(())
}

fn apply_clear_changes(project_root: &Path, metadata: &ProjectMetadata) -> Result<()> {
    files::update_cmake_clear(project_root, metadata)?;
    files::update_spec_clear(project_root, metadata)?;
    Ok(())
}

fn parse_mode(value: &str) -> Result<ConnectionMode> {
    match value.to_lowercase().as_str() {
        "sdk" => Ok(ConnectionMode::Sdk),
        "psdk" => Ok(ConnectionMode::Psdk),
        _ => Err(anyhow!(
            "Неизвестный режим '{}'. Допустимые значения: sdk, psdk",
            value
        )),
    }
}

fn prompt_mode() -> Result<ConnectionMode> {
    println!("Выберите окружение:");
    println!("1) psdk");
    println!("2) sdk");
    print!("Ваш выбор [1/2]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    match input.trim() {
        "1" | "psdk" | "PSDK" => Ok(ConnectionMode::Psdk),
        "2" | "sdk" | "SDK" => Ok(ConnectionMode::Sdk),
        _ => Err(anyhow!("Некорректный выбор окружения")),
    }
}

fn prompt_path() -> Result<String> {
    print!("Введите путь до SDK/PSDK: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("Путь не может быть пустым"));
    }
    Ok(trimmed.to_string())
}

fn create_progress_bar(total: u64, message: impl Into<String>) -> ProgressBar {
    let progress = ProgressBar::new(total);
    if !io::stderr().is_terminal() {
        progress.set_draw_target(ProgressDrawTarget::hidden());
    }
    let style = ProgressStyle::with_template(
        "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>3}/{len:3} {msg}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("=>-");
    progress.set_style(style);
    progress.set_message(message.into());
    progress
}

fn progress_step(progress: &ProgressBar, message: &str) {
    progress.inc(1);
    progress.set_message(message.to_string());
    log_info(Some(progress), message);
}

fn mode_label(mode: ProjectMode) -> &'static str {
    match mode {
        ProjectMode::Conan => "conan",
        ProjectMode::Clear => "clear",
    }
}

fn version_suffix(version: Option<&str>) -> String {
    version.map_or_else(String::new, |value| format!(" {}", value))
}

#[derive(Clone, Copy)]
enum LogLevel {
    Info,
    Success,
}

fn log_info(progress: Option<&ProgressBar>, message: &str) {
    log_with_level(progress, LogLevel::Info, message);
}

fn log_success(progress: Option<&ProgressBar>, message: &str) {
    log_with_level(progress, LogLevel::Success, message);
}

fn log_with_level(progress: Option<&ProgressBar>, level: LogLevel, message: &str) {
    let prefix = style("[aurora-conan-cli]").dim().bold().to_string();
    let level_tag = match level {
        LogLevel::Info => style("INFO").cyan().bold().to_string(),
        LogLevel::Success => style("SUCCESS").green().bold().to_string(),
    };
    let line = format!("{prefix} {level_tag} {message}");

    match progress {
        Some(pb) => pb.suspend(|| eprintln!("{line}")),
        None => eprintln!("{line}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::fs::File;
    use std::io::Cursor;
    use std::path::Path;

    use anyhow::{Context, Result, anyhow};
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};
    use tempfile::TempDir;

    use super::{CliCommand, run};
    use crate::conan::ConanProvider;
    use crate::files;
    use crate::model::{ConanRef, DownloadArtifact, ProjectMetadata};

    struct FakeProvider {
        latest_versions: HashMap<String, String>,
        metadata_by_names: HashMap<String, ProjectMetadata>,
        available_versions_by_name: HashMap<String, Vec<String>>,
        dependencies_by_ref: HashMap<String, Vec<ConanRef>>,
    }

    impl FakeProvider {
        fn key(refs: &[ConanRef]) -> String {
            let mut names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
            names.sort();
            names.join(",")
        }
    }

    impl ConanProvider for FakeProvider {
        fn search_dependencies(&self, query: &str) -> Result<Vec<ConanRef>> {
            let query_norm = query.to_lowercase();
            let mut names: Vec<String> = self
                .available_versions_by_name
                .keys()
                .filter(|name| name.to_lowercase().contains(&query_norm))
                .cloned()
                .collect();
            names.sort();

            if names.is_empty() {
                return Err(anyhow!("Пакеты по запросу '{query}' не найдены"));
            }

            let mut refs = Vec::new();
            for name in names {
                let versions = self
                    .available_versions_by_name
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| anyhow!("Пакет {name} не найден"))?;

                for version in versions {
                    refs.push(ConanRef {
                        name: name.clone(),
                        version,
                        user: "aurora".to_string(),
                    });
                }
            }

            Ok(refs)
        }

        fn list_dependency_versions(&self, name: &str) -> Result<Vec<String>> {
            self.available_versions_by_name
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("Пакет {name} не найден"))
        }

        fn download_dependency_archives(
            &self,
            package_name: &str,
            version: &str,
            destination_root: &Path,
        ) -> Result<Vec<DownloadArtifact>> {
            let versions = self
                .available_versions_by_name
                .get(package_name)
                .ok_or_else(|| anyhow!("Пакет {package_name} не найден"))?;
            if !versions.iter().any(|candidate| candidate == version) {
                return Err(anyhow!("Версия {version} для {package_name} не найдена"));
            }

            let download_dir = destination_root
                .join("downloads")
                .join(package_name)
                .join(version);
            fs::create_dir_all(&download_dir)?;

            let armv8_file = download_dir.join(format!("{package_name}-{version}-armv8.tgz"));
            create_test_tgz(&armv8_file, package_name, true)?;
            let x86_64_file = download_dir.join(format!("{package_name}-{version}-x86_64.tgz"));
            create_test_tgz(&x86_64_file, package_name, true)?;

            let package_file = download_dir.join(format!("{package_name}-{version}-package.tgz"));
            create_test_tgz(&package_file, package_name, false)?;

            Ok(vec![
                DownloadArtifact {
                    arch: "armv8".to_string(),
                    path: armv8_file,
                },
                DownloadArtifact {
                    arch: "x86_64".to_string(),
                    path: x86_64_file,
                },
                DownloadArtifact {
                    arch: "package".to_string(),
                    path: package_file,
                },
            ])
        }

        fn resolve_direct_dependency(
            &self,
            _project_root: &Path,
            name: &str,
            version: Option<&str>,
        ) -> Result<ConanRef> {
            let version = match version {
                Some(v) => v.to_string(),
                None => self
                    .latest_versions
                    .get(name)
                    .cloned()
                    .ok_or_else(|| anyhow!("Не настроена версия для {name}"))?,
            };

            Ok(ConanRef {
                name: name.to_string(),
                version,
                user: "aurora".to_string(),
            })
        }

        fn resolve_dependencies_without_conan(
            &self,
            package_name: &str,
            version: &str,
        ) -> Result<Vec<ConanRef>> {
            let key = format!("{package_name}/{version}");
            self.dependencies_by_ref
                .get(&key)
                .cloned()
                .ok_or_else(|| anyhow!("Зависимости для {key} не настроены"))
        }

        fn resolve_project_metadata(
            &self,
            _project_root: &Path,
            direct_refs: &[ConanRef],
        ) -> Result<ProjectMetadata> {
            let key = Self::key(direct_refs);
            self.metadata_by_names
                .get(&key)
                .cloned()
                .ok_or_else(|| anyhow!("metadata не настроены для ключа {key}"))
        }
    }

    fn setup_project() -> Result<(TempDir, FakeProvider)> {
        let temp = tempfile::tempdir()?;
        fs::create_dir_all(temp.path().join("rpm"))?;
        fs::write(temp.path().join("sdk-chroot"), "#!/bin/sh\nexit 0\n")?;

        fs::write(
            temp.path().join("CMakeLists.txt"),
            r#"cmake_minimum_required(VERSION 3.5)
project(ru.auroraos.TestApp CXX)

add_executable(${PROJECT_NAME} src/main.cpp)
"#,
        )?;

        fs::write(
            temp.path().join("rpm").join("ru.auroraos.TestApp.spec"),
            r#"Name:       ru.auroraos.TestApp
Summary:    Test app
Version:    0.1
Release:    1
License:    BSD-3-Clause
URL:        https://auroraos.ru
Source0:    %{name}-%{version}.tar.bz2

BuildRequires:  pkgconfig(Qt5Core)

%description
Test app

%prep
%autosetup

%build
%cmake
%make_build

%install
%make_install

%files
%{_bindir}/%{name}
"#,
        )?;

        let provider = FakeProvider {
            latest_versions: HashMap::from([
                ("ffmpeg".to_string(), "6.1.1".to_string()),
                ("a".to_string(), "1.0.0".to_string()),
                ("c".to_string(), "1.0.0".to_string()),
            ]),
            metadata_by_names: HashMap::from([
                (
                    "".to_string(),
                    ProjectMetadata {
                        direct_pkg_modules: Vec::new(),
                        shared_lib_patterns: Vec::new(),
                    },
                ),
                (
                    "ffmpeg".to_string(),
                    ProjectMetadata {
                        direct_pkg_modules: vec!["ffmpeg".to_string()],
                        shared_lib_patterns: vec![
                            "libavcodec.*".to_string(),
                            "libavutil.*".to_string(),
                        ],
                    },
                ),
                (
                    "a".to_string(),
                    ProjectMetadata {
                        direct_pkg_modules: vec!["a".to_string()],
                        shared_lib_patterns: vec!["liba.*".to_string(), "libb.*".to_string()],
                    },
                ),
                (
                    "a,c".to_string(),
                    ProjectMetadata {
                        direct_pkg_modules: vec!["a".to_string(), "c".to_string()],
                        shared_lib_patterns: vec![
                            "liba.*".to_string(),
                            "libb.*".to_string(),
                            "libc.*".to_string(),
                        ],
                    },
                ),
            ]),
            available_versions_by_name: HashMap::from([
                (
                    "ffmpeg".to_string(),
                    vec!["6.1.1".to_string(), "6.1.0".to_string()],
                ),
                (
                    "onnx".to_string(),
                    vec!["1.16.0".to_string(), "1.15.0".to_string()],
                ),
                ("onnxruntime".to_string(), vec!["1.18.1".to_string()]),
                ("ms-gsl".to_string(), vec!["4.0.0".to_string()]),
            ]),
            dependencies_by_ref: HashMap::from([(
                "onnxruntime/1.18.1".to_string(),
                vec![
                    ConanRef {
                        name: "onnx".to_string(),
                        version: "1.16.0".to_string(),
                        user: "aurora".to_string(),
                    },
                    ConanRef {
                        name: "ms-gsl".to_string(),
                        version: "4.0.0".to_string(),
                        user: "aurora".to_string(),
                    },
                ],
            )]),
        };

        Ok((temp, provider))
    }

    fn create_test_tgz(path: &Path, package_name: &str, with_lib: bool) -> Result<()> {
        let file = File::create(path)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut tar = Builder::new(encoder);

        append_bytes(
            &mut tar,
            format!("include/{package_name}.h").as_str(),
            b"// test header\n",
        )?;
        if with_lib {
            append_bytes(
                &mut tar,
                format!("lib/lib{package_name}.so").as_str(),
                b"binary-placeholder",
            )?;
        }

        tar.finish()?;
        Ok(())
    }

    fn append_bytes<W: std::io::Write>(
        tar: &mut Builder<W>,
        path: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let mut header = Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, path, Cursor::new(bytes))?;
        Ok(())
    }

    #[test]
    fn init_creates_conanfile_and_patches_templates() -> Result<()> {
        let (project, provider) = setup_project()?;

        run(&provider, project.path(), CliCommand::Init)?;

        let conanfile = fs::read_to_string(project.path().join("conanfile.py"))?;
        assert!(conanfile.contains("requires = ("));

        let cmake = fs::read_to_string(project.path().join("CMakeLists.txt"))?;
        assert!(cmake.contains("find_package(PkgConfig REQUIRED)"));
        assert!(cmake.contains("set(CMAKE_INSTALL_RPATH"));

        let spec = fs::read_to_string(project.path().join("rpm/ru.auroraos.TestApp.spec"))?;
        assert!(spec.contains("%define _cmake_skip_rpath %{nil}"));
        assert!(spec.contains("BuildRequires:  conan"));
        assert!(spec.contains("conan-install-if-modified"));
        assert!(spec.contains("conan-deploy-libraries"));

        Ok(())
    }

    #[test]
    fn init_clear_creates_manifest_and_clear_spec_snippets() -> Result<()> {
        let (project, provider) = setup_project()?;
        run(&provider, project.path(), CliCommand::InitClear)?;

        assert!(
            project
                .path()
                .join("thirdparty/aurora/manifest.lock.json")
                .exists()
        );
        assert!(!project.path().join("conanfile.py").exists());

        let spec = fs::read_to_string(project.path().join("rpm/ru.auroraos.TestApp.spec"))?;
        assert!(!spec.contains("BuildRequires:  conan"));
        assert!(spec.contains("THIRDPARTY_ROOT"));

        let cmake = fs::read_to_string(project.path().join("CMakeLists.txt"))?;
        assert!(cmake.contains("CMAKE_SYSTEM_PROCESSOR"));
        assert!(cmake.contains("AURORA_TP_ARCH"));
        assert!(cmake.contains("AURORA_TP_PKGCONFIG_DIR"));
        Ok(())
    }

    #[test]
    fn add_dependency_updates_conanfile_cmake_and_spec() -> Result<()> {
        let (project, provider) = setup_project()?;
        run(&provider, project.path(), CliCommand::Init)?;

        run(
            &provider,
            project.path(),
            CliCommand::Add {
                dependency: "ffmpeg".to_string(),
                version: None,
            },
        )?;

        let refs = files::read_requires(project.path())?;
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].to_ref_string(), "ffmpeg/6.1.1@aurora");

        let cmake = fs::read_to_string(project.path().join("CMakeLists.txt"))?;
        assert!(cmake.contains("pkg_check_modules(FFMPEG REQUIRED IMPORTED_TARGET ffmpeg)"));
        assert!(cmake.contains("PkgConfig::FFMPEG"));

        let spec = fs::read_to_string(project.path().join("rpm/ru.auroraos.TestApp.spec"))?;
        assert!(spec.contains("libavcodec.*"));
        assert!(spec.contains("libavutil.*"));

        Ok(())
    }

    #[test]
    fn remove_dependency_keeps_shared_transitive_libs_from_remaining_direct_dep() -> Result<()> {
        let (project, provider) = setup_project()?;
        run(&provider, project.path(), CliCommand::Init)?;

        run(
            &provider,
            project.path(),
            CliCommand::Add {
                dependency: "a".to_string(),
                version: None,
            },
        )?;
        run(
            &provider,
            project.path(),
            CliCommand::Add {
                dependency: "c".to_string(),
                version: None,
            },
        )?;

        run(
            &provider,
            project.path(),
            CliCommand::Remove {
                dependency: "c".to_string(),
            },
        )?;

        let refs = files::read_requires(project.path())?;
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "a");

        let spec = fs::read_to_string(project.path().join("rpm/ru.auroraos.TestApp.spec"))?;
        assert!(spec.contains("liba.*"));
        assert!(spec.contains("libb.*"));
        assert!(!spec.contains("libc.*"));

        Ok(())
    }

    #[test]
    fn add_same_dependency_is_idempotent() -> Result<()> {
        let (project, provider) = setup_project()?;
        run(&provider, project.path(), CliCommand::Init)?;

        run(
            &provider,
            project.path(),
            CliCommand::Add {
                dependency: "ffmpeg".to_string(),
                version: Some("6.1.1".to_string()),
            },
        )?;
        run(
            &provider,
            project.path(),
            CliCommand::Add {
                dependency: "ffmpeg".to_string(),
                version: Some("6.1.1".to_string()),
            },
        )?;

        let refs = files::read_requires(project.path())?;
        assert_eq!(refs.len(), 1);

        Ok(())
    }

    #[test]
    fn connect_and_disconnect_manage_global_state_file() -> Result<()> {
        let (project, provider) = setup_project()?;
        let state_root = project.path().join("state-root");
        fs::create_dir_all(&state_root)?;
        // SAFETY: test-only process-local environment override.
        unsafe {
            std::env::set_var(
                "AURORA_CONAN_CLI_STATE_DIR",
                state_root.to_string_lossy().to_string(),
            );
        }

        run(
            &provider,
            project.path(),
            CliCommand::Connect {
                mode: Some("psdk".to_string()),
                dir: Some(project.path().display().to_string()),
            },
        )?;

        let state_path = state_root.join("aurora-conan-cli/connection.json");
        let content = fs::read_to_string(&state_path)
            .with_context(|| format!("Нет {}", state_path.display()))?;
        assert!(content.contains("\"mode\": \"psdk\""));

        run(&provider, project.path(), CliCommand::Disconnect)?;
        assert!(!state_path.exists());

        // SAFETY: rollback environment override set above.
        unsafe {
            std::env::remove_var("AURORA_CONAN_CLI_STATE_DIR");
        }
        Ok(())
    }

    #[test]
    fn search_returns_versions_for_known_package() -> Result<()> {
        let (project, provider) = setup_project()?;
        run(
            &provider,
            project.path(),
            CliCommand::Search {
                dependency: "onnx".to_string(),
            },
        )?;
        Ok(())
    }

    #[test]
    fn search_fails_for_unknown_package() -> Result<()> {
        let (project, provider) = setup_project()?;
        let err = run(
            &provider,
            project.path(),
            CliCommand::Search {
                dependency: "unknown".to_string(),
            },
        )
        .expect_err("expected search to fail for unknown package");

        assert!(err.to_string().contains("не найден"));
        Ok(())
    }

    #[test]
    fn download_creates_artifact_for_selected_version() -> Result<()> {
        let (project, provider) = setup_project()?;
        run(
            &provider,
            project.path(),
            CliCommand::Download {
                dependency: "onnxruntime".to_string(),
                version: "1.18.1".to_string(),
            },
        )?;

        let artifact = project
            .path()
            .join("downloads/onnxruntime/1.18.1/onnxruntime-1.18.1-armv8.tgz");
        assert!(artifact.exists());
        Ok(())
    }

    #[test]
    fn deps_returns_dependencies_for_selected_package_version() -> Result<()> {
        let (project, provider) = setup_project()?;
        run(
            &provider,
            project.path(),
            CliCommand::Deps {
                dependency: "onnxruntime".to_string(),
                version: "1.18.1".to_string(),
            },
        )?;
        Ok(())
    }

    #[test]
    fn deps_fails_for_unknown_package_version() -> Result<()> {
        let (project, provider) = setup_project()?;
        let err = run(
            &provider,
            project.path(),
            CliCommand::Deps {
                dependency: "unknown".to_string(),
                version: "0.0.1".to_string(),
            },
        )
        .expect_err("expected deps to fail for unknown package version");

        assert!(err.to_string().contains("не настроены"));
        Ok(())
    }

    #[test]
    fn add_and_remove_dependency_in_clear_mode_manage_thirdparty_store() -> Result<()> {
        let (project, provider) = setup_project()?;
        run(&provider, project.path(), CliCommand::InitClear)?;

        run(
            &provider,
            project.path(),
            CliCommand::Add {
                dependency: "onnxruntime".to_string(),
                version: Some("1.18.1".to_string()),
            },
        )?;

        let arch_root = project.path().join("thirdparty/aurora/armv8");
        assert!(arch_root.join("pkgconfig/onnxruntime.pc").exists());
        assert!(
            arch_root
                .join("packages/onnxruntime/1.18.1/lib/libonnxruntime.so")
                .exists()
        );

        run(
            &provider,
            project.path(),
            CliCommand::Remove {
                dependency: "onnxruntime".to_string(),
            },
        )?;

        let manifest =
            fs::read_to_string(project.path().join("thirdparty/aurora/manifest.lock.json"))?;
        assert!(manifest.contains("\"direct_requires\": []"));
        Ok(())
    }
}
