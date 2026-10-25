//! Generate the typed Rust client from the 0xinsider OpenAPI document.
//!
//! ```text
//! cargo codegen                       # fetch https://0xinsider.com/api/v1/openapi.json
//! cargo codegen path/to/openapi.json
//! cargo codegen --check               # exit 1 when the generated files are stale
//! cargo codegen --app-commit SHA
//! ```
//!
//! Writes, and never hand-edit any of them:
//!
//! - `openapi.json`: the document exactly as fetched (the tests read its examples)
//! - `src/models.rs`: one Rust type per schema the document names or nests
//! - `src/operations.rs`: the operation table, a params struct and a Client method per operation
//! - `src/provenance.rs`: which document this release was generated from
//! - `tests/support/generated.rs`: operation ids and schema names mapped to Rust types, for the tests
//!
//! The app commit is `--app-commit`, the OXINSIDER_APP_COMMIT environment
//! variable, or a lookup of the GitHub commits API (GH_TOKEN is used when set).
//! One that cannot be resolved is recorded as `None` and reported on stderr,
//! never guessed.
//!
//! Both downloads go through `curl`, which every runner and workstation that
//! builds this crate has, rather than an HTTP client crate compiled for two GETs.
//! This program replaced `scripts/generate.py` (0xinsider/0xinsider#24564) and
//! writes the files that script wrote, byte for byte, apart from the header line
//! that names the generator.

mod generator;
mod names;
mod py;
mod schema;

use std::env;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::generator::{Generator, render_provenance};
use crate::schema::Result;

const DEFAULT_SOURCE: &str = "https://0xinsider.com/api/v1/openapi.json";
pub const APP_REPOSITORY: &str = "0xinsider/0xinsider";
pub const APP_SPEC_PATH: &str = "web/public/api/v1/openapi.json";
const USER_AGENT: &str = "0xinsider-rust-generator";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("generate: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The repository root: the parent of this crate.
fn root() -> Result<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let parent = manifest.parent().ok_or("the generator crate has no parent directory")?;
    parent
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", parent.display()))
}

fn is_remote(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// A GET through curl. A transfer that stalls for the timeout fails, as a
/// socket timeout would.
fn curl(url: &str, timeout: u32, headers: &[&str], secret_header: Option<&str>) -> Result<Vec<u8>> {
    let timeout = timeout.to_string();
    let mut command = Command::new("curl");
    command.args([
        "-fsSL",
        "--connect-timeout",
        &timeout,
        "--speed-limit",
        "1",
        "--speed-time",
        &timeout,
    ]);
    command.args(["-A", USER_AGENT]);
    for header in headers {
        command.args(["-H", header]);
    }
    // A credential goes in on stdin, never in argv where `ps` shows it.
    if secret_header.is_some() {
        command.args(["-H", "@-"]);
    }
    command
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| format!("cannot run curl: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("curl has no stdin")?;
    if let Some(header) = secret_header {
        stdin
            .write_all(format!("{header}\n").as_bytes())
            .map_err(|error| format!("curl stdin: {error}"))?;
    }
    drop(stdin);
    let output = child.wait_with_output().map_err(|error| format!("curl: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

fn load(source: &str) -> Result<Vec<u8>> {
    if is_remote(source) {
        return curl(source, 60, &[], None).map_err(|error| format!("cannot fetch {source}: {error}"));
    }
    fs::read(source).map_err(|error| format!("cannot read {source}: {error}"))
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn resolve_app_commit(explicit: Option<String>) -> Result<Option<String>> {
    let candidate = explicit
        .filter(|commit| !commit.is_empty())
        .or_else(|| non_empty_env("OXINSIDER_APP_COMMIT"));
    if candidate.is_some() {
        return Ok(candidate);
    }
    let url = format!("https://api.github.com/repos/{APP_REPOSITORY}/commits?path={APP_SPEC_PATH}&per_page=1");
    let authorization = non_empty_env("GH_TOKEN")
        .or_else(|| non_empty_env("GITHUB_TOKEN"))
        .map(|token| format!("Authorization: Bearer {token}"));
    let commits = curl(
        &url,
        30,
        &["Accept: application/vnd.github+json"],
        authorization.as_deref(),
    )
    .and_then(|body| serde_json::from_slice::<Value>(&body).map_err(|error| error.to_string()));
    let commits = match commits {
        Ok(commits) => commits,
        Err(error) => {
            eprintln!("warning: app commit not resolved from the GitHub API ({error}); recording None");
            return Ok(None);
        }
    };
    let Some(first) = commits.as_array().and_then(|commits| commits.first()) else {
        eprintln!("warning: the GitHub API listed no commit for the document; recording None");
        return Ok(None);
    };
    let sha = first.get("sha").ok_or("the GitHub API listed a commit with no sha")?;
    Ok(Some(py::str_of(Some(sha))))
}

/// Format generated Rust the way `cargo fmt` would, so the files never drift from it.
fn rustfmt(root: &Path, text: String) -> Result<String> {
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run rustfmt: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("rustfmt has no stdin")?;
    // Written from a thread: a file larger than the pipe buffer would otherwise
    // block on stdin while rustfmt blocks on a full stdout.
    let writer = thread::spawn(move || stdin.write_all(text.as_bytes()));
    let output = child.wait_with_output().map_err(|error| format!("rustfmt: {error}"))?;
    let written = writer.join().map_err(|_| "the rustfmt input writer panicked")?;
    if !output.status.success() {
        return Err(format!(
            "rustfmt failed:\n{}",
            py::universal_newlines(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    written.map_err(|error| format!("rustfmt stdin: {error}"))?;
    let formatted = String::from_utf8(output.stdout).map_err(|error| format!("rustfmt wrote non-UTF-8: {error}"))?;
    Ok(py::universal_newlines(&formatted))
}

/// A generated file is stale when it is missing or its text differs.
fn is_stale(path: &Path, text: &str) -> Result<bool> {
    match fs::read_to_string(path) {
        Ok(current) => Ok(py::universal_newlines(&current) != text),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

fn run() -> Result<ExitCode> {
    let root = root()?;
    let mut argv: Vec<String> = env::args().skip(1).collect();
    let check = argv.iter().any(|arg| arg == "--check");
    argv.retain(|arg| arg != "--check");
    let mut app_commit = None;
    if let Some(index) = argv.iter().position(|arg| arg == "--app-commit") {
        let value = argv.get(index + 1).cloned().ok_or("--app-commit needs a value")?;
        argv.drain(index..index + 2);
        app_commit = Some(value);
    }
    let source = match argv.first() {
        Some(source) => source.clone(),
        None if check => root.join("openapi.json").to_string_lossy().into_owned(),
        None => DEFAULT_SOURCE.to_string(),
    };
    let raw = load(&source)?;
    let doc: Value = serde_json::from_slice(&raw).map_err(|error| format!("{source} is not JSON: {error}"))?;
    let doc = doc.as_object().ok_or("the document is not a JSON object")?;
    let mut generator = Generator::new(doc)?;
    let (models, operations, count) = generator.generate()?;
    let source_label = if is_remote(&source) || check {
        DEFAULT_SOURCE.to_string()
    } else {
        format!("{APP_REPOSITORY}:{APP_SPEC_PATH}")
    };
    let mut outputs: Vec<(PathBuf, String)> = vec![
        (root.join("src").join("models.rs"), rustfmt(&root, models)?),
        (root.join("src").join("operations.rs"), rustfmt(&root, operations)?),
        (
            root.join("tests").join("support").join("generated.rs"),
            rustfmt(&root, generator.render_test_support())?,
        ),
    ];
    let schemas = generator.components.len();
    if check {
        let mut stale = Vec::new();
        for (path, text) in &outputs {
            if is_stale(path, text)? {
                stale.push(path.strip_prefix(&root).unwrap_or(path).display().to_string());
            }
        }
        if !stale.is_empty() {
            eprintln!("generate --check: stale {}; run cargo codegen", stale.join(", "));
            return Ok(ExitCode::FAILURE);
        }
        println!("generate --check: OK ({schemas} schemas, {count} operations)");
        return Ok(ExitCode::SUCCESS);
    }
    let commit = resolve_app_commit(app_commit)?;
    let sha256 = format!("{:x}", Sha256::digest(&raw));
    let provenance = render_provenance(&sha256, doc, &source_label, commit.as_deref(), count)?;
    outputs.push((root.join("src").join("provenance.rs"), rustfmt(&root, provenance)?));
    let spec = root.join("openapi.json");
    fs::write(&spec, &raw).map_err(|error| format!("cannot write {}: {error}", spec.display()))?;
    for (path, text) in &outputs {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        fs::write(path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    println!(
        "generated {count} operations and {schemas} schemas from {source_label} (sha256 {sha256}, app commit {})",
        commit.as_deref().unwrap_or("unknown")
    );
    Ok(ExitCode::SUCCESS)
}
