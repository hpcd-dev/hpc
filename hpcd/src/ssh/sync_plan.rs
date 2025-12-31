//! Sync planning utilities for SSH file synchronization.
//!
//! This module builds a deterministic, testable plan that enumerates local files,
//! applies include/exclude rules, and maps each file to a remote path. It does
//! not perform any network or SFTP operations.

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use super::{SyncFilterAction, SyncFilterRule};

/// A single file chosen for synchronization from local to remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncItem {
    /// Canonicalized absolute path to the local file on disk.
    pub local_path: PathBuf,
    /// Path of the file relative to the local sync root.
    pub rel_path: PathBuf,
    /// Full remote path computed from the remote root and relative path.
    pub remote_path: String,
}

/// The full sync plan derived from a local root, a remote root, and filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    /// Canonicalized local directory that was used as the scan root.
    pub local_root: PathBuf,
    /// Remote directory prefix used to build `remote_path` values.
    pub remote_root: String,
    /// Files selected for upload.
    pub items: Vec<SyncItem>,
    /// Unique remote directories that must exist before uploads.
    pub remote_dirs: Vec<String>,
}

/// Build a sync plan by scanning `local_dir` and applying `filters`.
///
/// This function canonicalizes the local directory, compiles glob-style filter
/// rules, and walks the local directory tree. It prunes excluded directories,
/// collects file entries, maps each file to a remote path, and records the
/// required remote parent directories. `remote_dirs` is returned sorted.
pub fn build_sync_plan<P: AsRef<Path>>(
    local_dir: P,
    remote_dir: &str,
    filters: &[SyncFilterRule],
) -> Result<SyncPlan> {
    let local_root = local_dir.as_ref().canonicalize()?;
    let path_filter = PathFilter::new(filters)?;
    let use_filter = !path_filter.is_empty();
    let mut items = Vec::new();
    let mut remote_dirs = BTreeSet::new();
    let follow_links = false;

    for entry in WalkDir::new(&local_root)
        .follow_links(follow_links)
        .into_iter()
        .filter_entry(|entry| {
            if !use_filter || entry.depth() == 0 {
                return true;
            }
            if !entry.file_type().is_dir() {
                return true;
            }
            match entry.path().strip_prefix(&local_root) {
                Ok(rel) => path_filter.should_include(rel, true),
                Err(_) => true,
            }
        })
    {
        let direntry = match entry {
            Ok(v) => v,
            Err(e) => {
                log::warn!("encountered error when enumerating local files: {:?}", e);
                continue;
            }
        };
        if !direntry.file_type().is_file() {
            // Directory creation is handled by file parents.
            continue;
        }
        let local_path = direntry.path().to_path_buf();
        let rel_path = match local_path.strip_prefix(&local_root) {
            Ok(v) => v.to_path_buf(),
            Err(_) => {
                log::warn!(
                    "failed computing relative path for {:?} from {:?}",
                    local_path,
                    local_root
                );
                continue;
            }
        };
        if use_filter && !path_filter.should_include(&rel_path, false) {
            continue;
        }
        let remote_path = join_remote(remote_dir, &rel_path);
        if let Some(parent_rel) = rel_path.parent()
            && !parent_rel.as_os_str().is_empty()
        {
            remote_dirs.insert(join_remote(remote_dir, parent_rel));
        }
        items.push(SyncItem {
            local_path,
            rel_path,
            remote_path,
        });
    }

    Ok(SyncPlan {
        local_root,
        remote_root: remote_dir.trim_end_matches('/').to_string(),
        items,
        remote_dirs: remote_dirs.into_iter().collect(),
    })
}

/// One compiled filter rule, including pattern metadata needed for matching.
#[derive(Debug)]
struct CompiledFilterRule {
    action: SyncFilterAction,
    matcher: GlobMatcher,
    only_dir: bool,
    match_basename: bool,
}

/// An ordered set of compiled include/exclude rules.
#[derive(Debug)]
struct PathFilter {
    rules: Vec<CompiledFilterRule>,
}

impl PathFilter {
    /// Compile all rules into matchers while preserving order.
    fn new(rules: &[SyncFilterRule]) -> Result<Self> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            compiled.push(CompiledFilterRule::compile(rule)?);
        }
        Ok(Self { rules: compiled })
    }

    /// Return true when there are no rules and everything should be included.
    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Decide whether a path should be included, honoring first-match wins.
    fn should_include(&self, rel_path: &Path, is_dir: bool) -> bool {
        if self.rules.is_empty() {
            return true;
        }
        let rel_str = rel_path_to_slash(rel_path);
        let basename = rel_path
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        for rule in &self.rules {
            if rule.matches(&rel_str, &basename, is_dir) {
                return matches!(rule.action, SyncFilterAction::Include);
            }
        }
        true
    }
}

impl CompiledFilterRule {
    /// Compile a raw filter rule into a glob matcher and matching flags.
    fn compile(rule: &SyncFilterRule) -> Result<Self> {
        let mut pattern = rule.pattern.trim().to_string();
        if pattern.is_empty() {
            anyhow::bail!("filter pattern cannot be empty");
        }

        let mut only_dir = false;
        if pattern.ends_with('/') {
            only_dir = true;
            while pattern.ends_with('/') {
                pattern.pop();
            }
        }
        if pattern.is_empty() {
            anyhow::bail!("filter pattern cannot be empty");
        }

        let anchored = pattern.starts_with('/');
        if anchored {
            pattern = pattern.trim_start_matches('/').to_string();
        }

        let has_slash = pattern.contains('/');
        let match_basename = !has_slash && !anchored;
        if has_slash && !anchored && !pattern.starts_with("**/") {
            pattern = format!("**/{}", pattern);
        }

        let mut builder = GlobBuilder::new(&pattern);
        builder.literal_separator(true);
        let matcher = builder
            .build()
            .with_context(|| format!("invalid filter pattern '{}'", rule.pattern))?
            .compile_matcher();
        Ok(Self {
            action: rule.action,
            matcher,
            only_dir,
            match_basename,
        })
    }

    /// Check whether a rule matches a path and directory flag.
    fn matches(&self, rel_path: &str, basename: &str, is_dir: bool) -> bool {
        if self.only_dir && !is_dir {
            return false;
        }
        if self.match_basename {
            self.matcher.is_match(basename)
        } else {
            self.matcher.is_match(rel_path)
        }
    }
}

/// Convert a relative path to a forward-slash string for glob matching.
fn rel_path_to_slash(path: &Path) -> String {
    let mut out = String::new();
    for comp in path.components() {
        if let std::path::Component::Normal(os) = comp {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&os.to_string_lossy());
        }
    }
    out
}

/// Join a remote base directory and a relative path using '/' separators.
fn join_remote(base: &str, rel: &Path) -> String {
    let mut s = base.trim_end_matches('/').to_string();
    for comp in rel.components() {
        if let std::path::Component::Normal(os) = comp {
            s.push('/');
            s.push_str(&os.to_string_lossy());
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::build_sync_plan;
    use super::{PathFilter, SyncFilterAction, SyncFilterRule};
    use std::fs;
    use std::path::Path;

    fn rule(action: SyncFilterAction, pattern: &str) -> SyncFilterRule {
        SyncFilterRule {
            action,
            pattern: pattern.to_string(),
        }
    }

    #[test]
    fn filter_order_first_match_wins() {
        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Include, "*.rs"),
            rule(SyncFilterAction::Exclude, "*"),
        ])
        .unwrap();
        assert!(filter.should_include(Path::new("src/lib.rs"), false));
        assert!(!filter.should_include(Path::new("Cargo.toml"), false));

        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Exclude, "*.rs"),
            rule(SyncFilterAction::Include, "*.rs"),
        ])
        .unwrap();
        assert!(!filter.should_include(Path::new("src/lib.rs"), false));
    }

    #[test]
    fn filter_matches_basename_and_paths() {
        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Include, "src/*.rs"),
            rule(SyncFilterAction::Exclude, "*"),
        ])
        .unwrap();
        assert!(filter.should_include(Path::new("src/lib.rs"), false));
        assert!(!filter.should_include(Path::new("lib.rs"), false));

        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Include, "*.rs"),
            rule(SyncFilterAction::Exclude, "*"),
        ])
        .unwrap();
        assert!(filter.should_include(Path::new("src/lib.rs"), false));
    }

    #[test]
    fn filter_respects_anchor_and_directory_only() {
        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Include, "/src/*.rs"),
            rule(SyncFilterAction::Exclude, "*"),
        ])
        .unwrap();
        assert!(filter.should_include(Path::new("src/lib.rs"), false));
        assert!(!filter.should_include(Path::new("foo/src/lib.rs"), false));

        let filter = PathFilter::new(&[rule(SyncFilterAction::Exclude, "target/")]).unwrap();
        assert!(!filter.should_include(Path::new("target"), true));
        assert!(filter.should_include(Path::new("target"), false));
    }

    #[test]
    fn build_sync_plan_applies_filters_and_collects_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("src/bin")).unwrap();
        fs::create_dir_all(root.join("target/tmp")).unwrap();

        fs::write(root.join("README.md"), "readme").unwrap();
        fs::write(root.join("src/lib.rs"), "lib").unwrap();
        fs::write(root.join("src/bin/main.rs"), "bin").unwrap();
        fs::write(root.join("target/tmp/ignored.rs"), "ignored").unwrap();

        let filters = [rule(SyncFilterAction::Exclude, "target/")];

        let plan = build_sync_plan(root, "/remote", &filters).unwrap();

        let mut remote_paths: Vec<String> = plan
            .items
            .iter()
            .map(|item| item.remote_path.clone())
            .collect();
        remote_paths.sort();

        assert_eq!(
            remote_paths,
            vec![
                "/remote/README.md".to_string(),
                "/remote/src/bin/main.rs".to_string(),
                "/remote/src/lib.rs".to_string(),
            ]
        );

        assert_eq!(
            plan.remote_dirs,
            vec!["/remote/src".to_string(), "/remote/src/bin".to_string()]
        );
    }
}
