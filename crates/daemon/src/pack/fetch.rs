//! Resolve a pack "source" (local path or github:…) to a local pack directory.
//! `crates/pack` stays network-free; this daemon-side layer turns remote into local.

use std::path::PathBuf;

/// A classified pack source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A local directory or pack.toml path (existing behavior).
    Local(PathBuf),
    /// A public GitHub repo.
    Remote(RemoteRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    pub owner: String,
    pub repo: String,
    /// Optional subdirectory (monorepo) where pack.toml lives. No leading/trailing slash.
    pub subdir: Option<String>,
    /// Optional git ref (tag/branch/commit). None → resolve the repo default branch.
    pub git_ref: Option<String>,
}

fn valid_seg(s: &str, extra_ok: fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra_ok(c))
}

/// Classify a source string. `github:owner/repo[/sub/dir][@ref]`, a
/// `https://github.com/owner/repo[/tree/ref/sub]` URL, or else a local path.
pub fn parse_source(input: &str) -> Result<Source, String> {
    let s = input.trim();
    if let Some(rest) = s.strip_prefix("github:") {
        return parse_github_short(rest);
    }
    if let Some(rest) = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
    {
        return parse_github_url(rest);
    }
    // Anything else is a local path.
    Ok(Source::Local(PathBuf::from(s)))
}

fn parse_github_short(rest: &str) -> Result<Source, String> {
    // Split off @ref first.
    let (path, git_ref) = match rest.split_once('@') {
        Some((p, r)) => (p, Some(r.to_string())),
        None => (rest, None),
    };
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or("").to_string();
    let repo = parts.next().unwrap_or("").to_string();
    let subdir: Vec<&str> = parts.collect();
    let subdir = if subdir.is_empty() {
        None
    } else {
        Some(subdir.join("/"))
    };
    finish(owner, repo, subdir, git_ref)
}

fn parse_github_url(rest: &str) -> Result<Source, String> {
    // owner/repo[/tree/<ref>/<subdir...>]  ;  strip a trailing ".git"
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next().unwrap_or("").to_string();
    let repo = parts.next().unwrap_or("").to_string();
    let mut git_ref = None;
    let mut subdir: Option<String> = None;
    if parts.next() == Some("tree") {
        if let Some(r) = parts.next() {
            git_ref = Some(r.to_string());
        }
        let sub: Vec<&str> = parts.collect();
        if !sub.is_empty() {
            subdir = Some(sub.join("/"));
        }
    }
    finish(owner, repo, subdir, git_ref)
}

fn finish(
    owner: String,
    repo: String,
    subdir: Option<String>,
    git_ref: Option<String>,
) -> Result<Source, String> {
    if !valid_seg(&owner, |c| c == '-' || c == '.' || c == '_')
        || !valid_seg(&repo, |c| c == '-' || c == '.' || c == '_')
    {
        return Err(format!("invalid github owner/repo: {owner}/{repo}"));
    }
    if let Some(sub) = &subdir {
        // No traversal, no absolute, no backslash.
        if sub.starts_with('/')
            || sub.contains('\\')
            || sub.split('/').any(|seg| seg == ".." || seg.is_empty())
        {
            return Err(format!("invalid subdir: {sub}"));
        }
    }
    if let Some(r) = &git_ref {
        if r.is_empty()
            || !r
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
        {
            return Err(format!("invalid ref: {r}"));
        }
    }
    Ok(Source::Remote(RemoteRef {
        owner,
        repo,
        subdir,
        git_ref,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(s: &str) -> RemoteRef {
        match parse_source(s).unwrap() {
            Source::Remote(r) => r,
            _ => panic!("expected remote: {s}"),
        }
    }

    #[test]
    fn parses_github_short_forms() {
        assert_eq!(
            remote("github:u/r"),
            RemoteRef {
                owner: "u".into(),
                repo: "r".into(),
                subdir: None,
                git_ref: None
            }
        );
        assert_eq!(remote("github:u/r@v1.2.0").git_ref.as_deref(), Some("v1.2.0"));
        assert_eq!(remote("github:u/r/sub/dir").subdir.as_deref(), Some("sub/dir"));
        let r = remote("github:u/r/sub@main");
        assert_eq!(r.subdir.as_deref(), Some("sub"));
        assert_eq!(r.git_ref.as_deref(), Some("main"));
    }

    #[test]
    fn parses_github_https_url() {
        assert_eq!(
            remote("https://github.com/u/r"),
            RemoteRef {
                owner: "u".into(),
                repo: "r".into(),
                subdir: None,
                git_ref: None
            }
        );
        let r = remote("https://github.com/u/r/tree/dev/packs/a");
        assert_eq!(r.git_ref.as_deref(), Some("dev"));
        assert_eq!(r.subdir.as_deref(), Some("packs/a"));
        assert_eq!(remote("https://github.com/u/r.git").repo, "r");
    }

    #[test]
    fn local_path_fallback() {
        assert_eq!(
            parse_source("/tmp/x/pack.toml").unwrap(),
            Source::Local("/tmp/x/pack.toml".into())
        );
        assert_eq!(
            parse_source("./rel/dir").unwrap(),
            Source::Local("./rel/dir".into())
        );
    }

    #[test]
    fn rejects_injection_and_traversal() {
        assert!(parse_source("github:u/r@a;rm -rf").is_err());
        assert!(parse_source("github:u/r/../etc").is_err());
        assert!(parse_source("github:u /r").is_err());
        assert!(parse_source("github:u/r@a b").is_err());
    }
}
