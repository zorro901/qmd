//! Thin wrapper around the `qmd` CLI. The TUI never touches the SQLite index
//! or the markdown files directly; every read goes through `qmd <cmd> --format
//! json` and every write goes through `qmd update --path <abs>` after we write
//! the file ourselves. This keeps qmd the single source of truth.

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Note {
    pub file: String,     // "collection/path.md"
    pub title: String,
    pub mtime: String,
    pub abs_path: Option<PathBuf>, // resolved from --full-path when available
}

#[derive(Debug, Clone, Deserialize)]
struct NotesJson {
    file: String,
    title: String,
    #[serde(default)]
    mtime: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MultiGetJson {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "fsPath")]
    fs_path: Option<String>,
}

/// Resolve the `qmd` binary. Prefer `qmd` on PATH; fall back to the repo's
/// `bin/qmd` so the TUI works from a fresh checkout too.
fn qmd_bin() -> String {
    if let Ok(p) = std::env::var("QMD_BIN") {
        return p;
    }
    "qmd".to_string()
}

/// Run `qmd <args...>` and return stdout, or an error string.
fn run_qmd(args: &[&str]) -> Result<String, String> {
    let out = Command::new(qmd_bin())
        .args(args)
        .output()
        .map_err(|e| format!("failed to spawn qmd: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "qmd {} exited {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// List all notes: `qmd notes --format json`.
pub fn list_notes() -> Result<Vec<Note>, String> {
    let raw = run_qmd(&["notes", "--format", "json"])?;
    let parsed: Vec<NotesJson> = serde_json::from_str(raw.trim())
        .map_err(|e| format!("notes json parse error: {e}\nraw: {raw}"))?;
    Ok(parsed
        .into_iter()
        .map(|n| Note {
            file: n.file,
            title: n.title,
            mtime: n.mtime,
            abs_path: None,
        })
        .collect())
}

/// Search notes: `qmd search --format json "<query>"`.
pub fn search(query: &str) -> Result<Vec<Note>, String> {
    let raw = run_qmd(&["search", "--format", "json", query])?;
    let parsed: Vec<NotesJson> = match serde_json::from_str(raw.trim()) {
        Ok(v) => v,
        // `qmd search` returns "[]" for no hits; tolerate non-JSON gracefully.
        Err(_) => return Ok(Vec::new()),
    };
    Ok(parsed
        .into_iter()
        .map(|n| Note {
            file: n.file,
            title: n.title,
            mtime: n.mtime,
            abs_path: None,
        })
        .collect())
}

/// Fetch a note's body. Uses `qmd multi-get --full-path` so we also learn the
/// on-disk absolute path (needed for saving + reindex).
pub fn get_body(file: &str) -> Result<(String, Option<PathBuf>), String> {
    let raw = run_qmd(&[
        "multi-get",
        file,
        "--format",
        "json",
        "--full-path",
        "--no-line-numbers",
    ])?;
    let parsed: Vec<MultiGetJson> = serde_json::from_str(raw.trim())
        .map_err(|e| format!("multi-get json parse error: {e}\nraw: {raw}"))?;
    let first = parsed
        .into_iter()
        .next()
        .ok_or_else(|| format!("note not found: {file}"))?;
    let abs = first.fs_path.as_ref().map(PathBuf::from);
    Ok((first.body, abs))
}

/// Write `content` to `abs_path`, then reindex just that file via
/// `qmd update --path <abs>` (O(changed), no full rescan).
pub fn save(abs_path: &PathBuf, content: &str) -> Result<(), String> {
    std::fs::write(abs_path, content).map_err(|e| format!("write failed: {e}"))?;
    let abs_str = abs_path.to_string_lossy();
    run_qmd(&["update", "--path", &abs_str])?;
    Ok(())
}
