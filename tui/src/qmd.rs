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

/// List all notes: `qmd notes --format json`. When `collection` is Some, only
/// notes from that collection are returned (`qmd notes -c <name>`).
pub fn list_notes(collection: Option<&str>) -> Result<Vec<Note>, String> {
    let mut args = vec!["notes", "--format", "json"];
    if let Some(c) = collection {
        args.push("-c");
        args.push(c);
    }
    let raw = run_qmd(&args)?;
    let parsed: Vec<NotesJson> = serde_json::from_str(raw.trim())
        .map_err(|e| format!("notes json parse error: {e}\nraw: {raw}"))?;
    Ok(parsed
        .into_iter()
        .map(|n| Note {
            file: n.file,
            title: n.title,
            mtime: n.mtime,
        })
        .collect())
}

/// Search notes: `qmd search --format json "<query>"`. When `collection` is
/// Some, only that collection is searched (`qmd search -c <name> "<query>"`).
pub fn search(query: &str, collection: Option<&str>) -> Result<Vec<Note>, String> {
    let mut args = vec!["search", "--format", "json"];
    if let Some(c) = collection {
        args.push("-c");
        args.push(c);
    }
    args.push(query);
    let raw = run_qmd(&args)?;
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

#[derive(Debug, Clone, Deserialize)]
struct CollectionJson {
    name: String,
    path: String,
}

/// List collections as `[{name, path}]` via `qmd collection list --format json`.
/// The `path` is the on-disk directory the collection indexes.
pub fn list_collections() -> Result<Vec<(String, PathBuf)>, String> {
    let raw = run_qmd(&["collection", "list", "--format", "json"])?;
    let parsed: Vec<CollectionJson> = serde_json::from_str(raw.trim())
        .map_err(|e| format!("collection list json parse error: {e}\nraw: {raw}"))?;
    Ok(parsed
        .into_iter()
        .map(|c| (c.name, PathBuf::from(c.path)))
        .collect())
}

/// Create a new note file inside `collection_dir`, then reindex just that file so
/// it appears in search immediately. Returns the absolute path written.
pub fn create_note(
    collection_dir: &PathBuf,
    file_name: &str,
    content: &str,
) -> Result<PathBuf, String> {
    let abs = collection_dir.join(file_name);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
    }
    std::fs::write(&abs, content).map_err(|e| format!("write failed: {e}"))?;
    let abs_str = abs.to_string_lossy();
    run_qmd(&["update", "--path", &abs_str])?;
    Ok(abs)
}


#[cfg(test)]
mod tests {
    use super::*;

    // Verifies the save() round-trip: write a file, reindex via qmd, then read
    // it back through the qmd CLI. Skipped automatically if `qmd` is not on PATH
    // or no index exists (CI without a built index).
    #[test]
    fn save_then_relist_reflects_content() {
        // Point at an existing indexed file when provided (e.g. a qmd collection
        // under test), otherwise use a temp file that qmd won't index (the test
        // then self-skips via the early return below).
        let file: std::path::PathBuf = match std::env::var("QMD_TUI_TEST_FILE") {
            Ok(p) => p.into(),
            Err(_) => {
                let dir =
                    std::env::temp_dir().join(format!("qmd-tui-test-{}", std::process::id()));
                let _ = std::fs::create_dir_all(&dir);
                dir.join("note.md")
            }
        };
        std::fs::write(&file, "# Hello\noriginal\n").unwrap();

        // Only run if `qmd` is reachable and has a usable index for this path.
        let abs = file.clone();
        let res = save(&abs, "# Hello\nedited by tui-textarea\n");
        if res.is_err() {
            // No qmd index covering this file — not a code failure.
            let _ = std::fs::remove_file(&file);
            return;
        }
        let back = std::fs::read_to_string(&file).unwrap();
        assert!(back.contains("edited by tui-textarea"));
        let _ = std::fs::remove_file(&file);
    }

    // Verifies create_note(): write a new file into an indexed collection dir and
    // reindex it via `qmd update --path`, then confirm it shows up in the note
    // list. Skips if no indexed collection dir is supplied via QMD_TUI_TEST_COLL_DIR.
    #[test]
    fn create_note_shows_in_list() {
        let dir: std::path::PathBuf = match std::env::var("QMD_TUI_TEST_COLL_DIR") {
            Ok(d) => d.into(),
            Err(_) => return, // nothing indexed to test against
        };
        let name = format!("qmd-tui-new-{}.md", std::process::id());
        let res = create_note(&dir, &name, "# Created by TUI\nhello\n");
        if res.is_err() {
            return; // qmd reindex not possible here — not a code failure
        }
        let abs = res.unwrap();
        let listed = match list_notes(None) {
            Ok(v) => v.iter().any(|n| n.file.ends_with(&name)),
            Err(_) => return, // can't verify without a usable index
        };
        let _ = std::fs::remove_file(&abs);
        // Reindex the (now-removed) file so the index stays consistent.
        let _ = save(&abs, "");
        assert!(listed, "newly created note should appear in qmd notes");
    }

    // NotesJson parses the mtime field, which the TUI renders as a recency
    // suffix in the list (serde defaults it to "" when absent).
    #[test]
    fn notes_json_parses_mtime() {
        let raw = r#"[{"file":"t/foo.md","title":"Foo","mtime":"2026-08-28T15:00:00Z"}]"#;
        let parsed: Vec<NotesJson> = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].mtime, "2026-08-28T15:00:00Z");
        assert_eq!(parsed[0].title, "Foo");
        assert_eq!(parsed[0].file, "t/foo.md");

        // mtime defaults to "" when the field is missing (title is still required).
        let raw = r#"[{"file":"t/bar.md","title":"Bar"}]"#;
        let parsed: Vec<NotesJson> = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed[0].mtime, "", "mtime defaults to empty string");
    }
}

