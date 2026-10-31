mod config;
mod contract;
mod executor;
mod image;
mod job;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::{fs, io};

use axum::{Router, routing::get};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::executor::{ExecError, Executor};
use crate::job::{Job, SourceFile};

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
        #[arg(long, default_value = "0.0.0.0:3000")]
        listen: String,
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
        Command::Serve { listen } => serve(&listen).await,
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

async fn serve(listen: &str) -> ExitCode {
    let app = Router::new().route("/", get(hello));
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("error: unable to listen on {listen}: {err}");
            return ExitCode::FAILURE;
        }
    };
    match axum::serve(listener, app).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn hello() -> &'static str {
    "Hello, world!"
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
