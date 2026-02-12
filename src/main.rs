mod app;
mod clear_store;
mod conan;
mod files;
mod mode;
mod model;

use std::env;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::app::CliCommand;
use crate::conan::CliConanProvider;

#[derive(Parser)]
#[command(name = "aurora-conan-cli")]
#[command(about = "CLI для управления Conan зависимостями в AuroraOS Qt проектах")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Подготавливает структуру Conan-интеграции в проекте.
    Init,

    /// Подготавливает структуру clear-интеграции (без использования Conan CLI).
    InitClear,

    /// Добавляет зависимость в conanfile.py и обновляет CMake/.spec.
    Add {
        dependency: String,
        version: Option<String>,
    },

    /// Удаляет зависимость из conanfile.py и пересчитывает CMake/.spec.
    Remove { dependency: String },

    /// Показывает список доступных версий пакета.
    Search { dependency: String },

    /// Скачивает архивы пакета указанной версии.
    Download { dependency: String, version: String },

    /// Показывает итоговый список зависимостей пакета без использования conan.
    Deps { dependency: String, version: String },
}

fn main() {
    if let Err(error) = run_main() {
        eprintln!("Ошибка: {error:#}");
        std::process::exit(1);
    }
}

fn run_main() -> Result<()> {
    let cli = Cli::parse();
    let project_root = env::current_dir()?;
    let provider = CliConanProvider;

    let command = match cli.command {
        Commands::Init => CliCommand::Init,
        Commands::InitClear => CliCommand::InitClear,
        Commands::Add {
            dependency,
            version,
        } => CliCommand::Add {
            dependency,
            version,
        },
        Commands::Remove { dependency } => CliCommand::Remove { dependency },
        Commands::Search { dependency } => CliCommand::Search { dependency },
        Commands::Download {
            dependency,
            version,
        } => CliCommand::Download {
            dependency,
            version,
        },
        Commands::Deps {
            dependency,
            version,
        } => CliCommand::Deps {
            dependency,
            version,
        },
    };

    app::run(&provider, &project_root, command)
}
