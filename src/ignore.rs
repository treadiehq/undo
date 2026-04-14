use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

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

// ── matcher storage: production ──────────────────────────────────────
//
// In production the matcher is initialised once per process via OnceLock
// and reused for the daemon's lifetime.

#[cfg(not(test))]
static CUSTOM_IGNORE: std::sync::OnceLock<Gitignore> = std::sync::OnceLock::new();

#[cfg(not(test))]
pub fn init(project_root: &Path) {
    CUSTOM_IGNORE.get_or_init(|| build_custom_ignore(project_root));
}

// ── matcher storage: test ───────────────────────────────────────────
//
// In tests the matcher is stored per-thread so each test can call
// init() with its own project root without poisoning other threads.

#[cfg(test)]
thread_local! {
    static THREAD_IGNORE: std::cell::RefCell<Option<Gitignore>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub fn init(project_root: &Path) {
    THREAD_IGNORE.with(|gi| *gi.borrow_mut() = Some(build_custom_ignore(project_root)));
}

// ── ignore logic (single implementation, always under test) ─────────

/// Apply a compiled Gitignore matcher to a path. Returns `Some(true/false)`
/// if the matcher has an opinion, or `None` to fall through to the builtin list.
fn apply_matcher(gi: &Gitignore, path: &Path, project_root: &Path) -> Option<bool> {
    let rel = path.strip_prefix(project_root).unwrap_or(path);
    let m = gi.matched(rel, path.is_dir());
    if m.is_whitelist() {
        Some(false)
    } else if m.is_ignore() {
        Some(true)
    } else {
        None
    }
}

/// Core decision function shared by both production and test paths.
/// Only the *storage* of the matcher differs; the branching logic here
/// is always the same code that ships in the binary.
fn should_ignore_with(gi: Option<&Gitignore>, path: &Path, project_root: &Path) -> bool {
    if let Some(gi) = gi {
        if let Some(result) = apply_matcher(gi, path, project_root) {
            return result;
        }
    }
    matches_builtin(path, project_root)
}

/// Returns true if the path should be excluded from watching.
/// Negation patterns in `.undoignore` (e.g. `!build/`) override the builtin list.
#[cfg(not(test))]
pub fn should_ignore(path: &Path, project_root: &Path) -> bool {
    should_ignore_with(CUSTOM_IGNORE.get(), path, project_root)
}

#[cfg(test)]
pub fn should_ignore(path: &Path, project_root: &Path) -> bool {
    THREAD_IGNORE.with(|gi| {
        should_ignore_with(gi.borrow().as_ref(), path, project_root)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/home/user/project";

    fn root() -> &'static Path {
        Path::new(ROOT)
    }

    /// .git directories must always be excluded from recording.
    #[test]
    fn git_directory_is_ignored() {
        let path = Path::new("/home/user/project/.git/config");
        assert!(should_ignore(path, root()));
    }

    /// node_modules is a default-ignored vendor directory.
    #[test]
    fn node_modules_is_ignored() {
        let path = Path::new("/home/user/project/node_modules/lodash/index.js");
        assert!(should_ignore(path, root()));
    }

    /// Rust build output in target/ must be excluded from recording.
    #[test]
    fn target_directory_is_ignored() {
        let path = Path::new("/home/user/project/target/debug/undo");
        assert!(should_ignore(path, root()));
    }

    /// A normal source file under src/ must pass through the ignore filter.
    #[test]
    fn regular_source_file_is_not_ignored() {
        let path = Path::new("/home/user/project/src/main.rs");
        assert!(!should_ignore(path, root()));
    }

    /// The .undo data directory must not watch itself to avoid infinite loops.
    #[test]
    fn undo_directory_is_ignored() {
        let path = Path::new("/home/user/project/.undo/database.db");
        assert!(should_ignore(path, root()));
    }

    /// Glob patterns in .undoignore must cause matching paths to be ignored.
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

    /// .gitignore patterns are loaded and applied alongside .undoignore rules.
    #[test]
    fn gitignore_patterns_are_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "*.tmp\n").unwrap();

        let gi = build_custom_ignore(root);

        assert!(gi.matched("scratch.tmp", false).is_ignore());
        assert!(!gi.matched("main.rs", false).is_ignore());
    }

    /// When no ignore files are present, no paths are incorrectly excluded.
    #[test]
    fn works_without_any_ignore_files() {
        let dir = tempfile::tempdir().unwrap();
        let gi = build_custom_ignore(dir.path());
        assert!(!gi.matched("anything.rs", false).is_ignore());
    }

    /// A ! pattern in .undoignore must override a built-in exclusion for directories.
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

    /// A ! pattern in .undoignore must override a built-in exclusion for individual files.
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

    /// Prove that init() is isolated per thread — two threads with conflicting
    /// ignore rules must not see each other's matchers.
    ///
    /// This test explicitly spawns two threads rather than relying on separate
    /// #[test] functions being scheduled on different threads. That makes the
    /// isolation guarantee deterministic and independent of test-runner behaviour.
    ///
    /// If isolation were broken (e.g. a process-wide OnceLock), one thread
    /// would inherit the other's matcher and at least one assertion would fail.
    #[test]
    fn init_is_isolated_per_thread() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();

        // Thread A's project ignores *.foo; thread B's project ignores *.bar.
        std::fs::write(dir_a.path().join(".undoignore"), "*.foo\n").unwrap();
        std::fs::write(dir_b.path().join(".undoignore"), "*.bar\n").unwrap();

        let root_a = dir_a.path().to_path_buf();
        let root_b = dir_b.path().to_path_buf();

        let handle_a = std::thread::spawn(move || {
            init(&root_a);
            let foo = root_a.join("test.foo");
            let bar = root_a.join("test.bar");
            assert!(should_ignore(&foo, &root_a), "thread A: *.foo should be ignored");
            assert!(!should_ignore(&bar, &root_a), "thread A: *.bar should not be ignored");
        });

        let handle_b = std::thread::spawn(move || {
            init(&root_b);
            let foo = root_b.join("test.foo");
            let bar = root_b.join("test.bar");
            assert!(!should_ignore(&foo, &root_b), "thread B: *.foo should not be ignored");
            assert!(should_ignore(&bar, &root_b), "thread B: *.bar should be ignored");
        });

        handle_a.join().expect("thread A panicked — matchers bled across threads");
        handle_b.join().expect("thread B panicked — matchers bled across threads");
    }
}
