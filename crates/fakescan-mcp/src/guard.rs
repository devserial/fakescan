//! Local write guard, same rules as hardly's `guard_write`.
//!
//! A `write_*` tool may create a file only inside the working directory, the OS temp directory,
//! `/workspace` (or `FAKESCAN_WORKSPACE` when that directory exists), or a directory listed in
//! `FAKESCAN_WRITE_DIRS`. `FAKESCAN_WRITE_DIRS=*` turns the allowlist off. Credential locations,
//! shell startup files, and anything inside `.git` stay refused either way. Overwrite is a
//! separate check in the caller.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::error::ToolError;

const WRITE_DIRS_ENV: &str = "FAKESCAN_WRITE_DIRS";

const DENIED_HOME_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config/gcloud",
    ".config/gh",
    ".config/fish",
    ".password-store",
];

const DENIED_HOME_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".cshrc",
    ".tcshrc",
    ".kshrc",
    ".inputrc",
    ".netrc",
    ".git-credentials",
    ".gitconfig",
    ".npmrc",
    ".pypirc",
    ".pgpass",
];

pub fn prepare_output(path: &Path, overwrite: bool) -> Result<(), ToolError> {
    if path.as_os_str().is_empty() {
        return Err(missing_path());
    }
    let real = resolve(path);
    if let Some(why) = denied(&real) {
        return Err(refused(path, &why));
    }
    if let Ok(meta) = fs::metadata(path) {
        let kind = meta.file_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(refused(path, "is a special file (device, socket or pipe)"));
        }
    }
    if !allow_all() && !allowed_roots().iter().any(|root| real.starts_with(root)) {
        return Err(refused(
            path,
            "is outside the directories fakescan-mcp may write to",
        ));
    }
    if path.is_dir() {
        return Err(ToolError::new(
            format!("output_path is a directory: {}", path.display()),
            "invalid_argument",
            Some("Give a file name.".into()),
        ));
    }
    if path.exists() && !overwrite {
        return Err(ToolError::new(
            format!("output_path already exists: {}", path.display()),
            "output_exists",
            Some("Pass overwrite=true to replace it, or choose another output path.".into()),
        ));
    }
    Ok(())
}

fn missing_path() -> ToolError {
    ToolError::new(
        "output_path is required",
        "missing_argument",
        Some(
            "write_* tools never choose a path: pass output_path=<file> (use a scratch directory)."
                .into(),
        ),
    )
}

fn refused(path: &Path, why: &str) -> ToolError {
    let roots = allowed_roots()
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    ToolError::new(
        format!("output_path {:?} is not writable: {why}", path),
        "path_not_allowed",
        Some(format!(
            "Write inside an allowed directory ({roots}) or add one with {WRITE_DIRS_ENV}=<dir>. Credential and shell-startup locations under your home directory are never writable."
        )),
    )
}

fn allow_all() -> bool {
    std::env::var(WRITE_DIRS_ENV)
        .map(|v| v.trim() == "*")
        .unwrap_or(false)
}

fn allowed_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = resolve(&cwd);
        if cwd.parent().is_some() {
            push_root(&mut roots, cwd);
        }
    }
    push_root(&mut roots, resolve(&std::env::temp_dir()));
    let workspace = std::env::var("FAKESCAN_WORKSPACE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/workspace".into());
    if Path::new(workspace.trim()).is_dir() {
        push_root(&mut roots, resolve(Path::new(workspace.trim())));
    }
    if let Ok(raw) = std::env::var(WRITE_DIRS_ENV) {
        if raw.trim() != "*" {
            for part in std::env::split_paths(&raw) {
                if !part.as_os_str().is_empty() {
                    push_root(&mut roots, resolve(&part));
                }
            }
        }
    }
    roots
}

fn push_root(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !roots.iter().any(|have| have == &root) {
        roots.push(root);
    }
}

/// Existing ancestor, canonicalized, with the missing tail (and `..`) applied.
fn resolve(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = abs.as_path();
    loop {
        if cur.exists() {
            let mut out = cur.canonicalize().unwrap_or_else(|_| cur.to_path_buf());
            for part in suffix.iter().rev() {
                match part.as_os_str().to_str() {
                    Some("..") => {
                        out.pop();
                    }
                    Some(".") => {}
                    _ => out.push(part),
                }
            }
            return out;
        }
        let Some(name) = cur.file_name() else {
            return lexical(&abs);
        };
        suffix.push(name.to_os_string());
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent,
            _ => return lexical(&abs),
        }
    }
}

fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn denied(real: &Path) -> Option<String> {
    if let Some(home) = home_dir() {
        if real.starts_with(&home) {
            if let Ok(rel) = real.strip_prefix(&home) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                for dir in DENIED_HOME_DIRS {
                    if rel == *dir || rel.starts_with(&format!("{dir}/")) {
                        return Some(format!("~/{dir} holds credentials or tool configuration"));
                    }
                }
                if DENIED_HOME_FILES.contains(&rel.as_str()) {
                    return Some(format!("~/{rel} is a shell/credential startup file"));
                }
            }
        }
    }
    if real.components().any(|c| c.as_os_str() == ".git") {
        return Some("inside a .git directory (hooks and config can run code)".into());
    }
    None
}

fn home_dir() -> Option<PathBuf> {
    let raw = std::env::var_os("HOME")?;
    let path = PathBuf::from(raw);
    if path.as_os_str().is_empty() {
        return None;
    }
    Some(resolve(&path))
}
