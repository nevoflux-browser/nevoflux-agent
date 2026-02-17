//! Knowledge category routing — maps knowledge entries to their target soul
//! documents and sections.

use nevoflux_storage::Knowledge;

/// The result of routing a knowledge entry: which file and section it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    /// Target soul document, e.g. "TOOLS.md".
    pub target_file: String,
    /// Section heading within the document, e.g. "Site Adaptation Graph".
    pub section: String,
}

/// Determine which soul document and section a knowledge entry should be
/// promoted to.
///
/// If the entry already has a `promotion_target` set, that value is preferred
/// (mapped to `{TARGET}.md`). Otherwise, the routing table is:
///
/// | category / subcategory          | target_file | section                 |
/// |---------------------------------|-------------|-------------------------|
/// | `site_interaction`              | TOOLS.md    | Site Adaptation Graph   |
/// | `tool_optimization`             | TOOLS.md    | Runtime Parameters      |
/// | `user_preference`               | USER.md     | Workflow Patterns       |
/// | `user_preference` / `language`  | USER.md     | Communication Overrides |
/// | `user_preference` / `domain`    | USER.md     | Professional Domains    |
/// | (everything else)               | TOOLS.md    | Site Adaptation Graph   |
pub fn route_knowledge(entry: &Knowledge) -> RouteTarget {
    // If the entry already has a promotion_target set, use it as the file name
    // and fall through to category-based section routing.
    let target_file = entry
        .promotion_target
        .as_deref()
        .map(|t| {
            // If it already ends with ".md", use as-is; otherwise append ".md"
            if t.ends_with(".md") {
                t.to_string()
            } else {
                format!("{}.md", t.to_uppercase())
            }
        });

    let subcategory = entry.subcategory.as_deref().unwrap_or("");

    match (entry.category.as_str(), subcategory) {
        ("user_preference" | "userpreference", "language") => RouteTarget {
            target_file: target_file.unwrap_or_else(|| "USER.md".to_string()),
            section: "Communication Overrides".to_string(),
        },
        ("user_preference" | "userpreference", "domain") => RouteTarget {
            target_file: target_file.unwrap_or_else(|| "USER.md".to_string()),
            section: "Professional Domains".to_string(),
        },
        ("user_preference" | "userpreference", _) => RouteTarget {
            target_file: target_file.unwrap_or_else(|| "USER.md".to_string()),
            section: "Workflow Patterns".to_string(),
        },
        ("tool_optimization" | "tooloptimization", _) => RouteTarget {
            target_file: target_file.unwrap_or_else(|| "TOOLS.md".to_string()),
            section: "Runtime Parameters".to_string(),
        },
        ("site_interaction" | "siteinteraction", _) => RouteTarget {
            target_file: target_file.unwrap_or_else(|| "TOOLS.md".to_string()),
            section: "Site Adaptation Graph".to_string(),
        },
        // Default fallback
        _ => RouteTarget {
            target_file: target_file.unwrap_or_else(|| "TOOLS.md".to_string()),
            section: "Site Adaptation Graph".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a minimal Knowledge struct for routing tests.
    fn knowledge_with(
        category: &str,
        subcategory: Option<&str>,
        promotion_target: Option<&str>,
    ) -> Knowledge {
        Knowledge {
            id: "K-00000000-000000".to_string(),
            category: category.to_string(),
            subcategory: subcategory.map(|s| s.to_string()),
            domain: None,
            summary: "test".to_string(),
            details: "details".to_string(),
            resolution: None,
            confidence: 0.8,
            hit_count: 5,
            success_count: 0,
            fail_count: 0,
            effectiveness: 0.5,
            priority: "medium".to_string(),
            status: "validated".to_string(),
            source_ids: None,
            related_ids: None,
            tags: None,
            privacy_level: "internal".to_string(),
            promotion_target: promotion_target.map(|s| s.to_string()),
            promoted_section: None,
            source_type: "system".to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            last_hit_at: None,
            promoted_at: None,
        }
    }

    #[test]
    fn routes_site_interaction_to_tools_md() {
        let entry = knowledge_with("site_interaction", Some("selector_result"), None);
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "TOOLS.md");
        assert_eq!(route.section, "Site Adaptation Graph");
    }

    #[test]
    fn routes_siteinteraction_variant_to_tools_md() {
        let entry = knowledge_with("siteinteraction", None, None);
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "TOOLS.md");
        assert_eq!(route.section, "Site Adaptation Graph");
    }

    #[test]
    fn routes_tool_optimization_to_runtime_parameters() {
        let entry = knowledge_with("tool_optimization", None, None);
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "TOOLS.md");
        assert_eq!(route.section, "Runtime Parameters");
    }

    #[test]
    fn routes_tooloptimization_variant() {
        let entry = knowledge_with("tooloptimization", None, None);
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "TOOLS.md");
        assert_eq!(route.section, "Runtime Parameters");
    }

    #[test]
    fn routes_user_preference_to_workflow_patterns() {
        let entry = knowledge_with("user_preference", None, None);
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "USER.md");
        assert_eq!(route.section, "Workflow Patterns");
    }

    #[test]
    fn routes_user_preference_language_to_communication_overrides() {
        let entry = knowledge_with("user_preference", Some("language"), None);
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "USER.md");
        assert_eq!(route.section, "Communication Overrides");
    }

    #[test]
    fn routes_user_preference_domain_to_professional_domains() {
        let entry = knowledge_with("user_preference", Some("domain"), None);
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "USER.md");
        assert_eq!(route.section, "Professional Domains");
    }

    #[test]
    fn routes_userpreference_variant_language() {
        let entry = knowledge_with("userpreference", Some("language"), None);
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "USER.md");
        assert_eq!(route.section, "Communication Overrides");
    }

    #[test]
    fn unknown_category_defaults_to_tools_md() {
        let entry = knowledge_with("unknown_category", None, None);
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "TOOLS.md");
        assert_eq!(route.section, "Site Adaptation Graph");
    }

    #[test]
    fn promotion_target_overrides_default_file() {
        let entry = knowledge_with("site_interaction", None, Some("AGENTS"));
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "AGENTS.md");
        // Section is still based on category
        assert_eq!(route.section, "Site Adaptation Graph");
    }

    #[test]
    fn promotion_target_with_md_extension_used_as_is() {
        let entry = knowledge_with("user_preference", Some("language"), Some("TOOLS.md"));
        let route = route_knowledge(&entry);
        assert_eq!(route.target_file, "TOOLS.md");
        assert_eq!(route.section, "Communication Overrides");
    }
}
