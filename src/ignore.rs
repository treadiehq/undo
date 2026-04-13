use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;
use std::sync::OnceLock;

const IGNORED_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".next",
    "dist",
    "build",
    ".DS_Store",
    "__pycache__",
    ".idea",
    ".vscode",
    ".backtrack",
    ".undo",
    ".env",
    ".env.local",
    ".env.production",
    ".ssh",
];

const IGNORED_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "keystore"];

/// Check if a path component matches one of the hardcoded ignore names,
/// or the file has a sensitive extension.
fn matches_builtin(path: &Path, project_root: &Path) -> bool {
    let rel = path.strip_prefix(project_root).unwrap_or(path);
    for component in rel.components() {
        if let std::path::Component::Normal(name) = component {
            let name_str = name.to_string_lossy();
            if IGNORED_NAMES.iter().any(|&ignored| ignored == name_str.as_ref()) {
                return true;
            }
        }
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if IGNORED_EXTENSIONS.iter().any(|&e| e == ext) {
            return true;
        }
    }
    false
}

/// Build a Gitignore matcher from `.gitignore` and `.undoignore` in the project root.
fn build_custom_ignore(project_root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(project_root);

    let gitignore = project_root.join(".gitignore");
    if gitignore.exists() {
        let _ = builder.add(gitignore);
    }

    // .undoignore patterns are added after .gitignore so they take precedence.
    let undoignore = project_root.join(".undoignore");
    if undoignore.exists() {
        let _ = builder.add(undoignore);
    }

    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Thread-local cache for the compiled ignore matcher.
/// Rebuilt once per project root (the root is fixed for the daemon's lifetime).
static CUSTOM_IGNORE: OnceLock<Gitignore> = OnceLock::new();

/// Initialise the custom ignore matcher for this project.
/// Must be called once before `should_ignore` is used.
pub fn init(project_root: &Path) {
    CUSTOM_IGNORE.get_or_init(|| build_custom_ignore(project_root));
}

/// Returns true if the path should be excluded from watching.
/// Negation patterns in `.undoignore` (e.g. `!build/`) override the builtin list.
pub fn should_ignore(path: &Path, project_root: &Path) -> bool {
    if let Some(gi) = CUSTOM_IGNORE.get() {
        let rel = path.strip_prefix(project_root).unwrap_or(path);
        let is_dir = path.is_dir();
        let m = gi.matched(rel, is_dir);
        if m.is_whitelist() {
            return false; // negation pattern explicitly includes this path
        }
        if m.is_ignore() {
            return true;
        }
    }

    matches_builtin(path, project_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/home/user/project";

    fn root() -> &'static Path {
        Path::new(ROOT)
    }

    #[test]
    fn git_directory_is_ignored() {
        let path = Path::new("/home/user/project/.git/config");
        assert!(should_ignore(path, root()));
    }

    #[test]
    fn node_modules_is_ignored() {
        let path = Path::new("/home/user/project/node_modules/lodash/index.js");
        assert!(should_ignore(path, root()));
    }

    #[test]
    fn target_directory_is_ignored() {
        let path = Path::new("/home/user/project/target/debug/undo");
        assert!(should_ignore(path, root()));
    }

    #[test]
    fn regular_source_file_is_not_ignored() {
        let path = Path::new("/home/user/project/src/main.rs");
        assert!(!should_ignore(path, root()));
    }

    #[test]
    fn undo_directory_is_ignored() {
        let path = Path::new("/home/user/project/.undo/database.db");
        assert!(should_ignore(path, root()));
    }

    #[test]
    fn undoignore_patterns_are_respected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".undoignore"), "*.log\ndata/\n").unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("app.log"), "x").unwrap();
        std::fs::write(root.join("src/main.rs"), "x").unwrap();

        let gi = build_custom_ignore(root);

        assert!(gi.matched("app.log", false).is_ignore());
        assert!(gi.matched("data", true).is_ignore());
        assert!(!gi.matched("src/main.rs", false).is_ignore());
    }

    #[test]
    fn gitignore_patterns_are_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "*.tmp\n").unwrap();

        let gi = build_custom_ignore(root);

        assert!(gi.matched("scratch.tmp", false).is_ignore());
        assert!(!gi.matched("main.rs", false).is_ignore());
    }

    #[test]
    fn works_without_any_ignore_files() {
        let dir = tempfile::tempdir().unwrap();
        let gi = build_custom_ignore(dir.path());
        assert!(!gi.matched("anything.rs", false).is_ignore());
    }

    #[test]
    fn negation_pattern_whitelists_builtin_ignored_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".undoignore"), "!build/\n").unwrap();

        let gi = build_custom_ignore(root);
        let m = gi.matched("build", true);
        assert!(
            m.is_whitelist(),
            "!build/ in .undoignore should whitelist the build directory"
        );
    }

    #[test]
    fn negation_pattern_whitelists_builtin_ignored_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".undoignore"), "!.env\n").unwrap();

        let gi = build_custom_ignore(root);
        let m = gi.matched(".env", false);
        assert!(
            m.is_whitelist(),
            "!.env in .undoignore should whitelist .env files"
        );
    }
}
