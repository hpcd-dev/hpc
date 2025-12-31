use std::path::{Component, Path, PathBuf};

/// takes a base path and a relative path and joins them, additionally normalizing the path,
/// without local access to those paths.
pub fn resolve_relative(remote_base: impl AsRef<Path>, relative_path: impl AsRef<Path>) -> PathBuf {
    let base = remote_base.as_ref();

    // Coerce the provided "relative" to actually be relative:
    // - strip leading "./"
    // - if it's absolute, drop the root so it becomes relative
    let mut rel = relative_path.as_ref();

    if let Ok(stripped) = rel.strip_prefix("./") {
        rel = stripped;
    }

    // If the path is absolute (starts with a root or drive), strip the prefix/root
    // so we still treat it as relative to `base`.
    if rel.is_absolute() {
        // On Unix this strips the leading `/`. On Windows this also removes any drive/UNC prefix.
        if let Ok(stripped) = rel.strip_prefix(rel.components().next().unwrap()) {
            rel = stripped;
        }
    }

    normalize_path(base.join(rel))
}

/// Normalize a path syntactically
/// - remove `.`
/// - resolve .. where possible
/// - keeps an absolute root/prefix if present
pub fn normalize_path(p: impl AsRef<Path>) -> PathBuf {
    let mut out = PathBuf::new();
    let p = p.as_ref();
    // Preserve any Windows prefix (drive/UNC) and root if present.
    let mut comps = p.components().peekable();
    while let Some(c) = comps.peek() {
        match c {
            Component::Prefix(prefix) => {
                out.push(Path::new(prefix.as_os_str()));
                comps.next();
            }
            Component::RootDir => {
                out.push(Path::new(std::path::MAIN_SEPARATOR_STR));
                comps.next();
            }
            _ => break,
        }
    }

    for comp in comps {
        match comp {
            Component::CurDir => {
                // skip
            }
            Component::ParentDir => {
                // pop only if we have something to pop and next wouldn't erase root/prefix
                let popped = out.pop();
                // If `out` was empty or only a root/prefix, keep `..`
                if !popped || out.as_os_str().is_empty() {
                    out.push("..");
                }
            }
            Component::Normal(seg) => {
                out.push(seg);
            }
            // Prefix/RootDir were already consumed at the start
            Component::Prefix(_) | Component::RootDir => { /* already handled */ }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // cross platform
    #[test]
    fn joins_with_dot_slash() {
        let base = pb("srv/data/projects"); // intentionally no leading slash for portability
        let rel = "./foo/bar.txt";
        let got = resolve_relative(&base, rel);
        assert_eq!(got, base.join("foo/bar.txt"));
    }

    #[test]
    fn joins_plain_relative() {
        let base = pb("srv/data/projects");
        let rel = "alpha/beta.txt";
        let got = resolve_relative(&base, rel);
        assert_eq!(got, base.join(rel));
    }

    #[test]
    fn normalizes_dot_and_dotdot() {
        let base = pb("srv/data/projects");
        let rel = "sub/./module/../file.txt";
        let got = resolve_relative(&base, rel);
        assert_eq!(got, base.join("sub/file.txt"));
    }

    #[test]
    fn handles_trailing_slash_on_base() {
        let base = pb("srv/data/projects/"); // trailing slash
        let rel = "x/y/../z";
        let got = resolve_relative(&base, rel);
        assert_eq!(got, pb("srv/data/projects/x/z"));
    }

    #[test]
    fn unix_coerces_absolute_relative_to_under_base() {
        let base = Path::new("/srv/data");
        let rel = "/logs/app.log"; // should be still treated as relative
        let got = resolve_relative(base, rel);
        assert_eq!(got, Path::new("/srv/data/logs/app.log"));
    }

    #[test]
    fn unix_preserves_root_when_normalizing() {
        let p = Path::new("/a/./b/../c");
        let got = super::normalize_path(p);
        assert_eq!(got, Path::new("/a/c"));
    }

    #[test]
    fn unix_parent_past_root_keeps_dotdot() {
        let p = Path::new("/..");
        let got = super::normalize_path(p);
        // We can’t pop past root, so .. remains
        assert_eq!(got, Path::new("/.."));
    }
}
