mod config;
mod contract;
mod executor;
mod image;
mod job;
mod playground;
mod registry;
mod server;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, io};

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::executor::{ExecError, Executor};
use crate::job::{Job, SourceFile};
use crate::registry::Registry;
use crate::server::AppState;

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "runner.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP server.
    Serve {
        /// Address to listen on [default: `server.listen` from the config]
        #[arg(long)]
        listen: Option<String>,
    },
    /// Compile and run the program in DIR and print the result as JSON.
    Exec {
        #[arg(long)]
        lang: String,
        /// Entry file, relative to DIR.
        #[arg(long)]
        entry: String,
        /// File to use as the program's stdin.
        #[arg(long)]
        stdin: Option<PathBuf>,
        dir: PathBuf,
    },
    /// Check an image against the contract by running its fixtures and examples.
    Contract {
        /// Check the image configured for this language.
        #[arg(long, required_unless_present = "image", conflicts_with = "image")]
        lang: Option<String>,
        /// Check this image instead, e.g. one freshly built in CI.
        #[arg(long)]
        image: Option<String>,
        /// How many programs to run at once.
        #[arg(long, default_value_t = 4)]
        parallel: usize,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = match Config::load(&cli.config) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    match cli.command {
        Command::Serve { listen } => serve(config, listen).await,
        Command::Exec {
            lang,
            entry,
            stdin,
            dir,
        } => exec(config, lang, entry, stdin, &dir).await,
        Command::Contract {
            lang,
            image,
            parallel,
        } => contract(config, lang, image, parallel).await,
    }
}

async fn contract(
    config: Config,
    language: Option<String>,
    image: Option<String>,
    parallel: usize,
) -> ExitCode {
    let executor = Arc::new(Executor::new(config));
    let info = match (language, image) {
        (_, Some(image)) => image::inspect(&executor.config().docker, &image)
            .await
            .map_err(ExecError::from),
        (Some(language), None) => executor.image(&language).await,
        (None, None) => unreachable!("clap requires --lang or --image"),
    };
    let info = match info {
        Ok(info) => info,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    match contract::check(executor, info, parallel).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(config: Config, listen: Option<String>) -> ExitCode {
    let listen = listen.unwrap_or_else(|| config.server.listen.clone());
    let refresh_interval = Duration::from_secs(config.server.refresh_interval_secs);

    let registry = Arc::new(Registry::new(&config));
    registry.refresh_all().await;
    registry.clone().spawn_refresh(refresh_interval);

    let executor = Arc::new(Executor::new(config));
    let app = server::router(Arc::new(AppState::new(executor, registry)));

    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("error: unable to listen on {listen}: {err}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("listening on {listen}");
    match axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Ctrl+C, or SIGTERM from systemd or docker.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

async fn exec(
    config: Config,
    language: String,
    entry: String,
    stdin: Option<PathBuf>,
    dir: &Path,
) -> ExitCode {
    let job = match read_job(language, entry, stdin.as_deref(), dir) {
        Ok(job) => job,
        Err(err) => {
            eprintln!("error: unable to read program: {err}");
            return ExitCode::FAILURE;
        }
    };

    match Executor::new(config).run(&job).await {
        Ok(result) => {
            let json = serde_json::to_string_pretty(&result).expect("result is serializable");
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn read_job(language: String, entry: String, stdin: Option<&Path>, dir: &Path) -> io::Result<Job> {
    let mut files = Vec::new();
    read_sources(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let stdin = match stdin {
        Some(path) => fs::read_to_string(path)?,
        None => String::new(),
    };
    Ok(Job {
        language,
        files,
        entry,
        stdin,
    })
}

/// Collect all files below `dir` with paths relative to `root`, separated by `/`.
fn read_sources(root: &Path, dir: &Path, files: &mut Vec<SourceFile>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            read_sources(root, &path, files)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("path is below root")
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        files.push(SourceFile {
            path: relative,
            content: fs::read_to_string(&path)?,
        });
    }
    Ok(())
}
