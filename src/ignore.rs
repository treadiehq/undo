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
];

/// Returns true if the path contains an ignored directory component.
pub fn should_ignore(path: &Path, project_root: &Path) -> bool {
    let rel = path.strip_prefix(project_root).unwrap_or(path);
    for component in rel.components() {
        if let std::path::Component::Normal(name) = component {
            let name_str = name.to_string_lossy();
            if IGNORED_NAMES.iter().any(|&ignored| ignored == name_str.as_ref()) {
                return true;
            }
        }
    }
    false
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
}
