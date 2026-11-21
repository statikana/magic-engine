//! CLI subprocess wrappers (`mtg-cli` & Node TS bridge scripts) and RAII `/tmp/e2e_*` scratch dir.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);
static MTG_CLI_DEBUG_BIN: OnceLock<PathBuf> = OnceLock::new();
static MTG_CLI_RELEASE_BIN: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct CliOutput {
    pub status_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// RAII temporary directory grounded in `std::env::temp_dir()` (`/tmp/e2e_<pid>_<seq>_<tag>`).
/// Cleaned up automatically on `Drop`; never writes inside the repository.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "e2e_{}_{}_{}",
            sanitize_tag(tag),
            std::process::id(),
            seq
        ));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("failed to create temp dir {}: {e}", dir.display()));
        Self { path: dir }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn sanitize_tag(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("e2e crate must live directly under repo root")
        .to_path_buf()
}

pub fn peckhamr_root() -> Option<PathBuf> {
    if let Ok(val) = std::env::var("PECKHAMR_DIR") {
        let p = PathBuf::from(val);
        if p.exists() {
            return Some(p);
        }
    }
    let sibling = repo_root().parent()?.join("peckhamr.com");
    if sibling.exists() {
        Some(sibling)
    } else {
        None
    }
}

pub fn mtg_cli_binary() -> &'static PathBuf {
    MTG_CLI_DEBUG_BIN.get_or_init(|| {
        if let Ok(explicit) = std::env::var("MTG_CLI_BIN") {
            return PathBuf::from(explicit);
        }
        let root = repo_root();
        let manifest = root.join("Cargo.toml");
        let output = Command::new("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("--bin")
            .arg("mtg-cli")
            .output()
            .expect("failed to invoke cargo build for mtg-cli");
        assert!(
            output.status.success(),
            "cargo build --bin mtg-cli failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        root.join("target").join("debug").join("mtg-cli")
    })
}

pub fn mtg_cli_release_binary() -> &'static PathBuf {
    MTG_CLI_RELEASE_BIN.get_or_init(|| {
        let root = repo_root();
        let manifest = root.join("Cargo.toml");
        let output = Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("--bin")
            .arg("mtg-cli")
            .output()
            .expect("failed to invoke cargo build --release for mtg-cli");
        assert!(
            output.status.success(),
            "cargo build --release --bin mtg-cli failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        root.join("target").join("release").join("mtg-cli")
    })
}

fn run_bin(bin: &Path, args: &[&str]) -> CliOutput {
    let out = Command::new(bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute {}: {e}", bin.display()));
    CliOutput {
        status_code: out.status.code(),
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

pub fn run_build_db(input_json: &Path, out_bin: &Path, raw: bool, level: Option<i32>) -> CliOutput {
    let bin = mtg_cli_binary();
    let in_s = input_json.to_string_lossy().into_owned();
    let out_s = out_bin.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec!["build-db".into(), in_s, "--out".into(), out_s];
    if raw {
        args.push("--raw".into());
    }
    if let Some(lvl) = level {
        args.push("--level".into());
        args.push(lvl.to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_bin(bin, &refs)
}

pub fn run_inspect_db(db_bin: &Path, verify_precons: bool) -> CliOutput {
    run_inspect_db_with_bin(mtg_cli_binary(), db_bin, verify_precons)
}

pub fn run_inspect_db_release(db_bin: &Path, verify_precons: bool) -> CliOutput {
    run_inspect_db_with_bin(mtg_cli_release_binary(), db_bin, verify_precons)
}

fn run_inspect_db_with_bin(bin: &Path, db_bin: &Path, verify_precons: bool) -> CliOutput {
    let db_s = db_bin.to_string_lossy().into_owned();
    let mut args = vec!["inspect-db", db_s.as_str()];
    if verify_precons {
        args.push("--verify-precons");
    }
    run_bin(bin, &args)
}

pub fn run_diff_test(db_bin: &Path, replay_json: &Path) -> CliOutput {
    let bin = mtg_cli_binary();
    let db_s = db_bin.to_string_lossy().into_owned();
    let rep_s = replay_json.to_string_lossy().into_owned();
    run_bin(bin, &["diff-test", &db_s, &rep_s])
}

pub fn run_node_script(script_rel_path: &str, args: &[&str]) -> CliOutput {
    let root = peckhamr_root().expect("peckhamr.com repository must exist on disk for TS bridge");
    let script = root.join(script_rel_path);
    let out = Command::new("node")
        .current_dir(&root)
        .arg(&script)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run node {}: {e}", script.display()));
    CliOutput {
        status_code: out.status.code(),
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Verifies the target Node bridge script exists on disk and did not fail due to
/// `MODULE_NOT_FOUND` (preventing negative CLI tests from passing vacuously when absent).
pub fn assert_node_script_present(script_rel_path: &str, res: &CliOutput) {
    let root = peckhamr_root().expect("peckhamr.com repository must exist on disk");
    let script = root.join(script_rel_path);
    assert!(
        script.exists(),
        "TS bridge script {} does not exist yet",
        script.display()
    );
    assert!(
        !res.stderr.contains("MODULE_NOT_FOUND") && !res.stderr.contains("Cannot find module"),
        "Node failed to load {}:\n{}",
        script.display(),
        res.stderr
    );
}

/// Writes a synthetic UTF-8 JSONL string as a `.jsonl.gz` file inside `TempDir` using Node's
/// built-in `zlib`, avoiding extra Rust dependencies in `e2e/Cargo.toml`.
pub fn write_synthetic_jsonl_gz(out_gz: &Path, jsonl_content: &str) {
    let out_s = out_gz.to_string_lossy().into_owned();
    let out = Command::new("node")
        .args([
            "-e",
            "const fs=require('fs'),zlib=require('zlib'); const data=fs.readFileSync(0); fs.writeFileSync(process.argv[1], zlib.gzipSync(data));",
            &out_s,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(jsonl_content.as_bytes())?;
            }
            child.wait_with_output()
        })
        .expect("failed to run node gzip helper");
    assert!(
        out.status.success(),
        "node gzip helper failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
