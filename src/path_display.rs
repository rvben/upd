//! User-facing path presentation.
//!
//! Filesystem operations keep their original paths. Output uses paths relative
//! to the directory where `upd` was invoked whenever the target is inside it.

use std::path::{Component, Path, PathBuf};

/// Render a path for CLI and structured output.
///
/// Paths inside the invocation directory are relative and never start with
/// `./`. Paths outside it remain absolute. Canonicalization is a fallback for
/// platforms whose temporary-directory or working-directory paths involve
/// symlinks.
pub fn display_path(path: &Path) -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return path.display().to_string();
    };

    if let Some(relative) = relative_to(path, &cwd) {
        return relative.display().to_string();
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let normalized = normalize_lexically(&absolute);

    if let Some(relative) = relative_to(&normalized, &cwd) {
        return relative.display().to_string();
    }

    if let (Ok(canonical_path), Ok(canonical_cwd)) = (path.canonicalize(), cwd.canonicalize())
        && let Some(relative) = relative_to(&canonical_path, &canonical_cwd)
    {
        return relative.display().to_string();
    }

    normalized.display().to_string()
}

fn relative_to<'a>(path: &'a Path, base: &Path) -> Option<&'a Path> {
    path.strip_prefix(base)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_dot_prefix_from_relative_paths() {
        assert_eq!(display_path(Path::new("./package.json")), "package.json");
    }

    #[test]
    fn makes_absolute_paths_under_cwd_relative() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            display_path(&cwd.join("apps/web/package.json")),
            "apps/web/package.json"
        );
    }

    #[test]
    fn leaves_paths_outside_cwd_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let parent_file = cwd.parent().unwrap().join("outside-package.json");
        assert!(Path::new(&display_path(&parent_file)).is_absolute());
    }
}
