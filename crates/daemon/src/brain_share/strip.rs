//! Apply `StripRules` to a `BrainPage`, producing the markdown shipped in
//! a `.nbrain` bundle. Privacy-safe by default (compiled truth only, no
//! frontmatter unless whitelisted, `.raw` always excluded by the caller).

use nevoflux_brain::{BrainPage, StripRules};

/// Render a stripped markdown document for one page.
///
/// Layout: optional `--- yaml frontmatter ---` (whitelisted minus redacted),
/// then `compiled_truth`, then (only if `!compiled_only`) `\n---\n` + timeline.
pub fn strip_page(page: &BrainPage, rules: &StripRules) -> String {
    let mut out = String::new();

    // Frontmatter: keep only whitelisted keys, minus redacted.
    let mut kept: Vec<(&String, &serde_json::Value)> = page
        .frontmatter
        .iter()
        .filter(|(k, _)| rules.frontmatter_whitelist.contains(k))
        .filter(|(k, _)| !rules.frontmatter_redacted.contains(k))
        .collect();
    kept.sort_by(|a, b| a.0.cmp(b.0)); // deterministic order
    if !kept.is_empty() {
        out.push_str("---\n");
        for (k, v) in kept {
            // Render scalars plainly; everything else via JSON.
            let rendered = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.push_str(&format!("{k}: {rendered}\n"));
        }
        out.push_str("---\n");
    }

    out.push_str(&page.compiled_truth);

    if !rules.compiled_only && !page.timeline.is_empty() {
        out.push_str("\n---\n");
        out.push_str(&page.timeline);
    }

    out
}

/// Whether a slug is excluded by directory rules. `.raw` is ALWAYS excluded
/// (invariant A.3), independent of `directories_excluded`.
pub fn is_excluded(slug: &str, rules: &StripRules) -> bool {
    let first = slug.split('/').next().unwrap_or("");
    if first == ".raw" || slug.starts_with(".raw/") {
        return true;
    }
    rules.directories_excluded.iter().any(|d| {
        let d = d.trim_end_matches('/');
        slug == d || slug.starts_with(&format!("{d}/"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn page_with(compiled: &str, timeline: &str, fm: &[(&str, &str)]) -> BrainPage {
        let mut frontmatter = HashMap::new();
        for (k, v) in fm {
            frontmatter.insert(k.to_string(), serde_json::Value::String(v.to_string()));
        }
        let now = chrono::Utc::now();
        BrainPage {
            slug: "concepts/yc".into(),
            title: "YC".into(),
            compiled_truth: compiled.into(),
            timeline: timeline.into(),
            frontmatter,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn compiled_only_drops_timeline() {
        let page = page_with("body", "evidence stream", &[]);
        let rules = StripRules::default(); // compiled_only = true
        let out = strip_page(&page, &rules);
        assert!(out.contains("body"));
        assert!(!out.contains("evidence stream"));
    }

    #[test]
    fn timeline_included_when_not_compiled_only() {
        let page = page_with("body", "evidence stream", &[]);
        let rules = StripRules {
            compiled_only: false,
            ..Default::default()
        };
        let out = strip_page(&page, &rules);
        assert!(out.contains("evidence stream"));
    }

    #[test]
    fn only_whitelisted_frontmatter_kept() {
        let page = page_with("b", "", &[("title", "YC"), ("score", "9")]);
        let rules = StripRules {
            frontmatter_whitelist: vec!["title".into()],
            ..Default::default()
        };
        let out = strip_page(&page, &rules);
        assert!(out.contains("title: YC"));
        assert!(!out.contains("score"));
    }

    #[test]
    fn redacted_overrides_whitelist() {
        let page = page_with("b", "", &[("title", "YC"), ("score", "9")]);
        let rules = StripRules {
            frontmatter_whitelist: vec!["title".into(), "score".into()],
            frontmatter_redacted: vec!["score".into()],
            ..Default::default()
        };
        let out = strip_page(&page, &rules);
        assert!(out.contains("title: YC"));
        assert!(!out.contains("score"));
    }

    #[test]
    fn raw_always_excluded() {
        let rules = StripRules::default();
        assert!(is_excluded(".raw/secret", &rules));
        assert!(is_excluded(".raw", &rules));
        assert!(!is_excluded("concepts/yc", &rules));
    }
}
