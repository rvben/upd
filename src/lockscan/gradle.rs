//! Resolved Maven coordinates from Gradle dependency locking, without executing Gradle.
use super::{LockScan, LockedPackage};
use crate::{audit::Ecosystem, updater::read_file_safe, version::gradle::is_literal};
use std::path::Path;

pub fn scan_gradle_lock(path: &Path) -> anyhow::Result<LockScan> {
    let mut result = LockScan::default();
    let content = read_file_safe(path)?;
    for (line, text) in content.lines().enumerate() {
        let text = text.trim();
        if text.is_empty() || text.starts_with('#') || text.starts_with("empty=") {
            continue;
        }
        let Some((coordinate, configurations)) = text.split_once('=') else {
            result.warnings.push(format!(
                "{}:{}: invalid Gradle lock entry",
                path.display(),
                line + 1
            ));
            continue;
        };
        let parts: Vec<_> = coordinate.split(':').collect();
        if parts.len() != 3
            || parts[0].is_empty()
            || parts[1].is_empty()
            || !is_literal(parts[2])
            || configurations.is_empty()
        {
            result.warnings.push(format!(
                "{}:{}: unsupported Gradle lock entry",
                path.display(),
                line + 1
            ));
            continue;
        }
        result.packages.push(LockedPackage {
            name: format!("{}:{}", parts[0], parts[1]),
            version: parts[2].into(),
            ecosystem: Ecosystem::Maven,
            lockfile_path: path.into(),
            line_number: Some(line + 1),
            locator: None,
        });
    }
    if !result.warnings.is_empty() {
        anyhow::bail!("{}", result.warnings.join("; "));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_resolved_coordinates_and_reports_invalid_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("gradle.lockfile");
        std::fs::write(&p, "# locked\ng:a:1.2.3=runtimeClasspath,testRuntimeClasspath\nempty=compileClasspath\ng:b:1.+ =test\nmalformed\n").unwrap();
        assert!(scan_gradle_lock(&p).is_err());
        std::fs::write(
            &p,
            "# locked\ng:a:1.2.3=runtimeClasspath,testRuntimeClasspath\nempty=compileClasspath\n",
        )
        .unwrap();
        let r = scan_gradle_lock(&p).unwrap();
        assert_eq!(r.packages.len(), 1);
        assert_eq!(r.packages[0].ecosystem, Ecosystem::Maven);
        assert_eq!(r.packages[0].name, "g:a");
    }
}
