//! Checks an image against CONTRACT.md: its layout, its fixtures and its examples.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::executor::{ExecError, Executor, Outcome, Phase, RunResult};
use crate::image::ImageInfo;
use crate::job::Job;
use crate::playground::{self, Expect, ExtractError};

/// Fixtures every image must provide, see "Required fixtures" in CONTRACT.md.
const REQUIRED_FIXTURES: &[&str] = &["hello", "compile_error", "exit_code", "infinite_loop"];
/// Fixtures required by a capability.
const CAPABILITY_FIXTURES: &[(&str, &str)] =
    &[("stdin", "stdin_echo"), ("multi-file", "multi_file")];

/// A program from `fixtures.toml` or `examples.toml`, ready to run.
struct Case {
    /// e.g. `fixtures/hello`
    name: String,
    job: Job,
    expect: Expect,
}

#[derive(Default)]
struct Report {
    passed: usize,
    failed: usize,
    warnings: usize,
}

impl Report {
    fn pass(&mut self, name: &str, duration_ms: u64) {
        println!("PASS  {name}  ({duration_ms} ms)");
        self.passed += 1;
    }

    fn fail(&mut self, name: &str, reason: &str) {
        println!("FAIL  {name}  {}", indent(reason));
        self.failed += 1;
    }

    fn warn(&mut self, message: &str) {
        println!("WARN  {message}");
        self.warnings += 1;
    }
}

/// Check `image` against the contract, running up to `parallel` programs at once.
/// Returns whether the image passed.
pub async fn check(
    executor: Arc<Executor>,
    image: ImageInfo,
    parallel: usize,
) -> Result<bool, ExtractError> {
    println!(
        "{} image {} (revision {}, contract {})",
        image.language, image.id, image.revision, image.contract
    );
    let dir = playground::extract(&executor.config().docker, &image.id).await?;
    let mut report = Report::default();

    if !dir.path().join("linter/index.js").is_file() {
        report.warn("linter/index.js is missing");
    }

    let mut cases = Vec::new();
    let mut fixture_ids = HashSet::new();
    for set in ["fixtures", "examples"] {
        for result in load(dir.path(), set, &image.language) {
            match result {
                Ok(case) => {
                    if set == "fixtures" {
                        fixture_ids.insert(case.name["fixtures/".len()..].to_string());
                    }
                    cases.push(case);
                }
                Err((name, reason)) => report.fail(&name, &reason),
            }
        }
    }

    let required = REQUIRED_FIXTURES.iter().copied().chain(
        CAPABILITY_FIXTURES
            .iter()
            .filter(|(capability, _)| image.capabilities.iter().any(|c| c == capability))
            .map(|(_, fixture)| *fixture),
    );
    for id in required {
        if !fixture_ids.contains(id) {
            report.fail(&format!("fixtures/{id}"), "required fixture is missing");
        }
    }

    let image = Arc::new(image);
    let permits = Arc::new(Semaphore::new(parallel.max(1)));
    let mut tasks = JoinSet::new();
    for case in cases {
        let (executor, image, permits) = (executor.clone(), image.clone(), permits.clone());
        tasks.spawn(async move {
            let _permit = permits
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            let result = executor.run_on(&image, &case.job).await;
            (case.name, evaluate(&case.expect, result))
        });
    }
    while let Some(task) = tasks.join_next().await {
        let (name, outcome) = task.expect("contract task panicked");
        match outcome {
            Ok(duration_ms) => report.pass(&name, duration_ms),
            Err(reason) => report.fail(&name, &reason),
        }
    }

    println!(
        "\n{} passed, {} failed, {} warnings",
        report.passed, report.failed, report.warnings
    );
    Ok(report.failed == 0)
}

/// Load the programs listed in `<set>/<set>.toml`. Problems with single programs are returned
/// as `(name, reason)` so the other programs still run.
fn load(root: &Path, set: &str, language: &str) -> Vec<Result<Case, (String, String)>> {
    let programs = match playground::load_index(root, set) {
        Ok(programs) => programs,
        Err(err) => return vec![Err((format!("{set}/{set}.toml"), err))],
    };

    let mut ids = HashSet::new();
    programs
        .into_iter()
        .map(|program| {
            let name = format!("{set}/{}", program.id);
            if !playground::is_valid_id(&program.id) {
                return Err((name, "id must match [a-z0-9_-]+".to_string()));
            }
            if !ids.insert(program.id.clone()) {
                return Err((name, "duplicate id".to_string()));
            }
            let files = playground::read_files(root, set, &program)
                .map_err(|reason| (name.clone(), reason))?;

            Ok(Case {
                job: Job {
                    language: language.to_string(),
                    files,
                    entry: program.entry,
                    stdin: program.stdin,
                },
                expect: program.expect,
                name,
            })
        })
        .collect()
}

/// Compare a run with the expectation. Returns the total duration on success.
fn evaluate(expect: &Expect, result: Result<RunResult, ExecError>) -> Result<u64, String> {
    let result = result.map_err(|err| err.to_string())?;
    let phase = expect.phase.unwrap_or(Phase::Run);

    if expect.timeout {
        if result.outcome != Outcome::TimedOut {
            return Err(format!("expected a timeout, got {}", describe(&result)));
        }
        if result.phase != phase {
            return Err(format!(
                "timed out in the {} phase, expected the {} phase",
                phase_name(result.phase),
                phase_name(phase)
            ));
        }
        return Ok(duration_ms(&result));
    }

    match (phase, result.outcome) {
        (Phase::Compile, Outcome::CompileError) => {}
        (Phase::Run, Outcome::Exited) => {
            let expected = expect.exit_code.unwrap_or(0);
            if result.exit_code != Some(expected) {
                return Err(format!(
                    "expected exit code {expected}, got {}",
                    describe(&result)
                ));
            }
            let actual = result.run.as_ref().map_or("", |run| run.stdout.as_str());
            if let Some(stdout) = &expect.stdout
                && actual != stdout
            {
                return Err(format!(
                    "stdout differs\nexpected: {stdout:?}\nactual:   {actual:?}"
                ));
            }
        }
        (Phase::Compile, _) => {
            return Err(format!(
                "expected a compile error, got {}",
                describe(&result)
            ));
        }
        (Phase::Run, _) => {
            return Err(format!(
                "expected the program to run, got {}",
                describe(&result)
            ));
        }
    }
    Ok(duration_ms(&result))
}

/// One line about the outcome, followed by the compile output when it explains a failure.
fn describe(result: &RunResult) -> String {
    let mut text = match result.outcome {
        Outcome::Exited => format!("exit code {}", result.exit_code.unwrap_or_default()),
        Outcome::CompileError => "a compile error".to_string(),
        Outcome::ToolchainError => format!(
            "a toolchain error (exit code {})",
            result.exit_code.unwrap_or_default()
        ),
        Outcome::TimedOut => format!("a timeout in the {} phase", phase_name(result.phase)),
        Outcome::OutOfMemory => "out of memory".to_string(),
        Outcome::OutputLimit => "too much output".to_string(),
    };
    if matches!(
        result.outcome,
        Outcome::CompileError | Outcome::ToolchainError
    ) {
        let output = format!("{}{}", result.compile.stdout, result.compile.stderr);
        if !output.trim().is_empty() {
            text.push('\n');
            text.push_str(output.trim_end());
        }
    }
    text
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Compile => "compile",
        Phase::Run => "run",
    }
}

fn duration_ms(result: &RunResult) -> u64 {
    result.compile.duration_ms + result.run.as_ref().map_or(0, |run| run.duration_ms)
}

/// Indent continuation lines so multi-line reasons stay readable under the FAIL line.
fn indent(text: &str) -> String {
    text.replace('\n', "\n      ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::Output;

    fn result(outcome: Outcome, phase: Phase, exit_code: Option<i32>, stdout: &str) -> RunResult {
        let output = |stdout: &str| Output {
            stdout: stdout.into(),
            stderr: String::new(),
            duration_ms: 1,
        };
        RunResult {
            outcome,
            phase,
            exit_code,
            compile: output(""),
            run: (phase == Phase::Run).then(|| output(stdout)),
            revision: "test".into(),
        }
    }

    fn expect(toml: &str) -> Expect {
        toml::from_str::<playground::Program>(&format!(
            "id = \"x\"\nfiles = []\nentry = \"x\"\n{toml}"
        ))
        .unwrap()
        .expect
    }

    #[test]
    fn run_defaults_to_exit_code_zero() {
        let expect = expect("");
        assert!(
            evaluate(
                &expect,
                Ok(result(Outcome::Exited, Phase::Run, Some(0), ""))
            )
            .is_ok()
        );
        assert!(
            evaluate(
                &expect,
                Ok(result(Outcome::Exited, Phase::Run, Some(1), ""))
            )
            .is_err()
        );
        assert!(
            evaluate(
                &expect,
                Ok(result(Outcome::CompileError, Phase::Compile, Some(100), ""))
            )
            .is_err()
        );
    }

    #[test]
    fn compares_stdout_and_exit_code() {
        let expect = expect("expect.stdout = \"hi\\n\"\nexpect.exit_code = 3");
        assert!(
            evaluate(
                &expect,
                Ok(result(Outcome::Exited, Phase::Run, Some(3), "hi\n"))
            )
            .is_ok()
        );
        assert!(
            evaluate(
                &expect,
                Ok(result(Outcome::Exited, Phase::Run, Some(3), "hi"))
            )
            .is_err()
        );
        assert!(
            evaluate(
                &expect,
                Ok(result(Outcome::Exited, Phase::Run, Some(0), "hi\n"))
            )
            .is_err()
        );
    }

    #[test]
    fn compile_phase_expects_compile_error() {
        let expect = expect("expect.phase = \"compile\"");
        assert!(
            evaluate(
                &expect,
                Ok(result(Outcome::CompileError, Phase::Compile, Some(100), ""))
            )
            .is_ok()
        );
        assert!(
            evaluate(
                &expect,
                Ok(result(
                    Outcome::ToolchainError,
                    Phase::Compile,
                    Some(101),
                    ""
                ))
            )
            .is_err()
        );
    }

    #[test]
    fn timeout_checks_phase() {
        let expect = expect("expect.timeout = true");
        assert!(evaluate(&expect, Ok(result(Outcome::TimedOut, Phase::Run, None, ""))).is_ok());
        assert!(
            evaluate(
                &expect,
                Ok(result(Outcome::TimedOut, Phase::Compile, None, ""))
            )
            .is_err()
        );
        assert!(
            evaluate(
                &expect,
                Ok(result(Outcome::Exited, Phase::Run, Some(0), ""))
            )
            .is_err()
        );
    }
}
