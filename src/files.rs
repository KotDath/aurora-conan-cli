use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use regex::Regex;

use crate::model::{ConanRef, ProjectMetadata};

pub const CMAKE_FILE: &str = "CMakeLists.txt";
pub const CONANFILE: &str = "conanfile.py";

pub fn read_requires(project_root: &Path) -> Result<Vec<ConanRef>> {
    let conanfile_path = project_root.join(CONANFILE);
    if !conanfile_path.exists() {
        return Ok(Vec::new());
    }

    let content = read_text(&conanfile_path)?;
    let re = Regex::new(r#"\"([^/\"\s]+)\/([^@\"\s]+)@([^\"\s]+)\""#)
        .context("Не удалось подготовить regex для чтения requires")?;

    let refs = re
        .captures_iter(&content)
        .map(|caps| ConanRef {
            name: caps[1].to_string(),
            version: caps[2].to_string(),
            user: caps[3].to_string(),
        })
        .collect();

    Ok(refs)
}

pub fn write_conanfile(project_root: &Path, refs: &[ConanRef]) -> Result<()> {
    let mut sorted = refs.to_vec();
    sorted.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));

    let mut content = String::from(
        "from conan import ConanFile\n\nclass Application(ConanFile):\n    settings = \"os\", \"compiler\", \"arch\", \"build_type\"\n    generators = \"PkgConfigDeps\"\n\n    requires = (\n",
    );

    for reference in &sorted {
        content.push_str(&format!("        \"{}\",\n", reference.to_ref_string()));
    }

    content.push_str("    )\n");

    let path = project_root.join(CONANFILE);
    write_text(&path, &content)
}

pub fn update_cmake(project_root: &Path, metadata: &ProjectMetadata) -> Result<()> {
    update_cmake_impl(project_root, metadata, false)
}

pub fn update_cmake_clear(project_root: &Path, metadata: &ProjectMetadata) -> Result<()> {
    update_cmake_impl(project_root, metadata, true)
}

fn update_cmake_impl(
    project_root: &Path,
    metadata: &ProjectMetadata,
    clear_mode: bool,
) -> Result<()> {
    let path = project_root.join(CMAKE_FILE);
    let mut content = read_text(&path)?;

    content = ensure_find_package_pkgconfig(&content)?;
    if clear_mode {
        content = upsert_block_after_project(&content, "clear-arch", &clear_arch_block())?;
    } else {
        content = remove_managed_block(&content, "clear-arch")?;
    }

    let rpath_body = [
        "set(CMAKE_SKIP_RPATH FALSE)",
        "set(CMAKE_BUILD_WITH_INSTALL_RPATH TRUE)",
        "set(CMAKE_INSTALL_RPATH \"${CMAKE_INSTALL_PREFIX}/share/${PROJECT_NAME}/lib\")",
    ]
    .join("\n");
    content = upsert_block_after_project(&content, "rpath", &rpath_body)?;

    let pkg_body = if metadata.direct_pkg_modules.is_empty() {
        "# No Conan dependencies configured.".to_string()
    } else {
        let mut modules = metadata.direct_pkg_modules.clone();
        modules.sort();
        modules.dedup();

        modules
            .into_iter()
            .map(|module| {
                let alias = module_to_alias(&module);
                format!(
                    "pkg_check_modules({alias} REQUIRED IMPORTED_TARGET {module})",
                    alias = alias,
                    module = module
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    content = upsert_managed_block(&content, "pkgconfig", &pkg_body)?;

    let targets_body = if metadata.direct_pkg_modules.is_empty() {
        "# No Conan dependencies configured.".to_string()
    } else {
        let mut modules = metadata.direct_pkg_modules.clone();
        modules.sort();
        modules.dedup();

        let target_entries = modules
            .iter()
            .map(|module| format!("      PkgConfig::{}", module_to_alias(module)))
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            "if(TARGET ${{PROJECT_NAME}})\n  target_include_directories(${{PROJECT_NAME}} PRIVATE\n    $<BUILD_INTERFACE:\n{target_entries}\n    >\n  )\n\n  target_link_libraries(${{PROJECT_NAME}} PRIVATE\n{target_entries}\n  )\nendif()"
        )
    };
    content = upsert_managed_block(&content, "targets", &targets_body)?;

    write_text(&path, &content)
}

fn clear_arch_block() -> String {
    [
        "if(CMAKE_SYSTEM_PROCESSOR MATCHES \"^(aarch64|arm64)$\")",
        "  set(AURORA_TP_ARCH \"armv8\")",
        "elseif(CMAKE_SYSTEM_PROCESSOR MATCHES \"^(armv7|armv7hl)$\")",
        "  set(AURORA_TP_ARCH \"armv7\")",
        "elseif(CMAKE_SYSTEM_PROCESSOR MATCHES \"^(x86_64|amd64)$\")",
        "  set(AURORA_TP_ARCH \"x86_64\")",
        "else()",
        "  message(FATAL_ERROR \"Unsupported architecture: ${CMAKE_SYSTEM_PROCESSOR}\")",
        "endif()",
        "",
        "set(AURORA_TP_ROOT \"${CMAKE_CURRENT_SOURCE_DIR}/thirdparty/aurora\")",
        "set(AURORA_TP_PKGCONFIG_DIR \"${AURORA_TP_ROOT}/${AURORA_TP_ARCH}/pkgconfig\")",
        "if(DEFINED ENV{PKG_CONFIG_PATH} AND NOT \"$ENV{PKG_CONFIG_PATH}\" STREQUAL \"\")",
        "  set(ENV{PKG_CONFIG_PATH} \"${AURORA_TP_PKGCONFIG_DIR}:$ENV{PKG_CONFIG_PATH}\")",
        "else()",
        "  set(ENV{PKG_CONFIG_PATH} \"${AURORA_TP_PKGCONFIG_DIR}\")",
        "endif()",
    ]
    .join("\n")
}

pub fn update_spec(project_root: &Path, metadata: &ProjectMetadata) -> Result<()> {
    let spec_path = find_spec_file(project_root)?;
    let mut content = read_text(&spec_path)?;

    content = upsert_define(&content, "_cmake_skip_rpath", "%{nil}")?;
    content = upsert_define(
        &content,
        "__provides_exclude_from",
        "^%{_datadir}/%{name}/lib/.*$",
    )?;

    let mut patterns = metadata.shared_lib_patterns.clone();
    patterns.sort();
    patterns.dedup();

    let requires_pattern = if patterns.is_empty() {
        "^$".to_string()
    } else {
        format!("^({})$", patterns.join("|"))
    };

    content = upsert_define(&content, "__requires_exclude", &requires_pattern)?;
    content = ensure_buildrequires_conan(&content)?;

    let build_body = [
        "CONAN_LIB_DIR=\"%{_builddir}/conan-libs/\"",
        "%{set_build_flags}",
        "conan-install-if-modified --source-folder=\"%{_sourcedir}/..\" --output-folder=\"$CONAN_LIB_DIR\" -vwarning",
        "PKG_CONFIG_PATH=\"$CONAN_LIB_DIR:$PKG_CONFIG_PATH\"",
        "export PKG_CONFIG_PATH",
    ]
    .join("\n");
    content = upsert_block_in_section(&content, "build", "build-snippet", &build_body)?;

    let install_body = [
        "EXECUTABLE=\"%{buildroot}/%{_bindir}/%{name}\"",
        "CONAN_LIB_DIR=\"%{_builddir}/conan-libs/\"",
        "SHARED_LIBRARIES=\"%{buildroot}/%{_datadir}/%{name}/lib\"",
        "mkdir -p \"$SHARED_LIBRARIES\"",
        "conan-deploy-libraries \"$EXECUTABLE\" \"$CONAN_LIB_DIR\" \"$SHARED_LIBRARIES\"",
    ]
    .join("\n");
    content = upsert_block_in_section(&content, "install", "install-snippet", &install_body)?;

    write_text(&spec_path, &content)
}

pub fn update_spec_clear(project_root: &Path, metadata: &ProjectMetadata) -> Result<()> {
    let spec_path = find_spec_file(project_root)?;
    let mut content = read_text(&spec_path)?;

    content = upsert_define(&content, "_cmake_skip_rpath", "%{nil}")?;
    content = upsert_define(
        &content,
        "__provides_exclude_from",
        "^%{_datadir}/%{name}/lib/.*$",
    )?;

    let mut patterns = metadata.shared_lib_patterns.clone();
    patterns.sort();
    patterns.dedup();
    let requires_pattern = if patterns.is_empty() {
        "^$".to_string()
    } else {
        format!("^({})$", patterns.join("|"))
    };
    content = upsert_define(&content, "__requires_exclude", &requires_pattern)?;
    content = remove_buildrequires_conan(&content)?;

    let build_body = [
        "THIRDPARTY_ROOT=\"%{_sourcedir}/../thirdparty/aurora\"",
        "case \"%{_arch}\" in",
        "  aarch64) AURORA_TP_ARCH=\"armv8\" ;;",
        "  armv7hl) AURORA_TP_ARCH=\"armv7\" ;;",
        "  x86_64) AURORA_TP_ARCH=\"x86_64\" ;;",
        "  *) echo \"Unsupported arch: %{_arch}\" >&2; exit 1 ;;",
        "esac",
        "THIRDPARTY_ARCH_DIR=\"$THIRDPARTY_ROOT/$AURORA_TP_ARCH\"",
        "PKG_CONFIG_PATH=\"$THIRDPARTY_ARCH_DIR/pkgconfig:$PKG_CONFIG_PATH\"",
        "export PKG_CONFIG_PATH",
    ]
    .join("\n");
    content = upsert_block_in_section(&content, "build", "build-snippet", &build_body)?;

    let install_body = [
        "THIRDPARTY_ROOT=\"%{_sourcedir}/../thirdparty/aurora\"",
        "case \"%{_arch}\" in",
        "  aarch64) AURORA_TP_ARCH=\"armv8\" ;;",
        "  armv7hl) AURORA_TP_ARCH=\"armv7\" ;;",
        "  x86_64) AURORA_TP_ARCH=\"x86_64\" ;;",
        "  *) echo \"Unsupported arch: %{_arch}\" >&2; exit 1 ;;",
        "esac",
        "THIRDPARTY_ARCH_DIR=\"$THIRDPARTY_ROOT/$AURORA_TP_ARCH\"",
        "SHARED_LIBRARIES=\"%{buildroot}/%{_datadir}/%{name}/lib\"",
        "mkdir -p \"$SHARED_LIBRARIES\"",
        "find \"$THIRDPARTY_ARCH_DIR/packages\" -type f -name 'lib*.so*' -exec cp -P {} \"$SHARED_LIBRARIES\" \\; 2>/dev/null || true",
    ]
    .join("\n");
    content = upsert_block_in_section(&content, "install", "install-snippet", &install_body)?;

    write_text(&spec_path, &content)
}

pub fn find_spec_file(project_root: &Path) -> Result<PathBuf> {
    let rpm_dir = project_root.join("rpm");
    if !rpm_dir.exists() {
        return Err(anyhow!("Не найден каталог rpm/"));
    }

    let mut spec_paths: Vec<PathBuf> = fs::read_dir(&rpm_dir)
        .with_context(|| format!("Не удалось прочитать {}", rpm_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "spec"))
        .collect();

    spec_paths.sort();

    match spec_paths.as_slice() {
        [single] => Ok(single.clone()),
        [] => Err(anyhow!("В rpm/ не найден .spec файл")),
        _ => Err(anyhow!(
            "Найдено несколько .spec файлов в rpm/. Ожидается ровно один"
        )),
    }
}

fn read_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("Не удалось прочитать {}", path.display()))
}

fn write_text(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content).with_context(|| format!("Не удалось записать {}", path.display()))
}

fn module_to_alias(module: &str) -> String {
    module
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn managed_markers(key: &str) -> (String, String) {
    (
        format!("# >>> aurora-conan-cli:{key}:begin"),
        format!("# <<< aurora-conan-cli:{key}:end"),
    )
}

fn upsert_managed_block(content: &str, key: &str, body: &str) -> Result<String> {
    let (start, end) = managed_markers(key);
    let block = format!("{start}\n{body}\n{end}");

    let pattern = Regex::new(&format!(
        r"(?s){}\n.*?\n{}",
        regex::escape(&start),
        regex::escape(&end)
    ))
    .context("Не удалось подготовить regex для управляемого блока")?;

    if pattern.is_match(content) {
        Ok(pattern
            .replace(content, |_caps: &regex::Captures| block.clone())
            .to_string())
    } else {
        let trimmed = content.trim_end();
        Ok(format!("{trimmed}\n\n{block}\n"))
    }
}

fn remove_managed_block(content: &str, key: &str) -> Result<String> {
    let (start, end) = managed_markers(key);
    let pattern = Regex::new(&format!(
        r"(?s)\n?{}\n.*?\n{}\n?",
        regex::escape(&start),
        regex::escape(&end)
    ))
    .context("Не удалось подготовить regex для удаления блока")?;

    Ok(pattern.replace(content, "\n").to_string())
}

fn upsert_block_after_project(content: &str, key: &str, body: &str) -> Result<String> {
    let without_block = remove_managed_block(content, key)?;
    let (start, end) = managed_markers(key);
    let block = format!("{start}\n{body}\n{end}\n");

    let re_project = Regex::new(r"(?m)^project\s*\(.*\)\s*$")
        .context("Не удалось подготовить regex для project()")?;

    if let Some(project_match) = re_project.find(&without_block) {
        let insert_at = project_match.end();
        let mut out = String::new();
        out.push_str(&without_block[..insert_at]);
        out.push_str("\n\n");
        out.push_str(&block);
        out.push_str(&without_block[insert_at..]);
        Ok(out)
    } else {
        Ok(format!("{}\n{}", without_block.trim_end(), block))
    }
}

fn ensure_find_package_pkgconfig(content: &str) -> Result<String> {
    if content.contains("find_package(PkgConfig") || content.contains("include(FindPkgConfig)") {
        return Ok(content.to_string());
    }

    let re_project = Regex::new(r"(?m)^project\s*\(.*\)\s*$")
        .context("Не удалось подготовить regex для project()")?;

    if let Some(project_match) = re_project.find(content) {
        let insert_at = project_match.end();
        let mut out = String::new();
        out.push_str(&content[..insert_at]);
        out.push_str("\nfind_package(PkgConfig REQUIRED)");
        out.push_str(&content[insert_at..]);
        Ok(out)
    } else {
        Ok(format!("find_package(PkgConfig REQUIRED)\n{content}"))
    }
}

fn upsert_define(content: &str, key: &str, value: &str) -> Result<String> {
    let line = format!("%define {key} {value}");
    let re = Regex::new(&format!(r"(?m)^%define\s+{}\s+.*$", regex::escape(key)))
        .context("Не удалось подготовить regex для %define")?;

    if re.is_match(content) {
        return Ok(re.replace(content, line.as_str()).to_string());
    }

    let re_name = Regex::new(r"(?m)^Name:\s+").context("Не удалось подготовить regex для Name")?;
    if let Some(m) = re_name.find(content) {
        let mut out = String::new();
        out.push_str(&content[..m.start()]);
        out.push_str(&line);
        out.push('\n');
        out.push_str(&content[m.start()..]);
        Ok(out)
    } else {
        Ok(format!("{line}\n{content}"))
    }
}

fn ensure_buildrequires_conan(content: &str) -> Result<String> {
    let has_conan = Regex::new(r"(?m)^BuildRequires:.*\bconan\b")
        .context("Не удалось подготовить regex для BuildRequires conan")?
        .is_match(content);

    if has_conan {
        return Ok(content.to_string());
    }

    let build_requires_re = Regex::new(r"(?m)^BuildRequires:.*$")
        .context("Не удалось подготовить regex для BuildRequires")?;
    let all: Vec<_> = build_requires_re.find_iter(content).collect();

    if let Some(last) = all.last() {
        let mut out = String::new();
        out.push_str(&content[..last.end()]);
        out.push_str("\nBuildRequires:  conan");
        out.push_str(&content[last.end()..]);
        return Ok(out);
    }

    let desc_re = Regex::new(r"(?m)^%description\b")
        .context("Не удалось подготовить regex для %description")?;
    if let Some(desc) = desc_re.find(content) {
        let mut out = String::new();
        out.push_str(&content[..desc.start()]);
        out.push_str("BuildRequires:  conan\n");
        out.push_str(&content[desc.start()..]);
        Ok(out)
    } else {
        Ok(format!("{content}\nBuildRequires:  conan\n"))
    }
}

fn remove_buildrequires_conan(content: &str) -> Result<String> {
    let re = Regex::new(r"(?m)^BuildRequires:.*\bconan\b.*\n?")
        .context("Не удалось подготовить regex для удаления BuildRequires conan")?;
    Ok(re.replace_all(content, "").to_string())
}

fn upsert_block_in_section(content: &str, section: &str, key: &str, body: &str) -> Result<String> {
    let section_re = Regex::new(&format!(r"(?m)^%{}\s*$", regex::escape(section)))
        .with_context(|| format!("Не удалось подготовить regex для секции %{section}"))?;
    let section_start = section_re
        .find(content)
        .ok_or_else(|| anyhow!("Не найдена секция %{section} в .spec"))?;

    let next_section_re = Regex::new(r"(?m)^%[A-Za-z]")
        .context("Не удалось подготовить regex для поиска следующей секции")?;

    let section_end = next_section_re
        .find_at(content, section_start.end())
        .map(|m| m.start())
        .unwrap_or(content.len());

    let section_content = &content[section_start.start()..section_end];
    let updated_section = upsert_managed_block(section_content, key, body)?;

    let mut out = String::new();
    out.push_str(&content[..section_start.start()]);
    out.push_str(&updated_section);
    out.push_str(&content[section_end..]);
    Ok(out)
}
