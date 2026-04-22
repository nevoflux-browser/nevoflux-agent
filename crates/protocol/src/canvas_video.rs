//! Wire types for `canvas.video.*` namespace.

use serde::{Deserialize, Serialize};

/// `canvas.video.create_composition` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCompositionRequest {
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub duration_sec: f32,
    pub fps: u32,
    #[serde(default)]
    pub bg: Option<String>,
    /// Raw HTML override. Phase B uses this for end-to-end tests;
    /// P2 adds template-driven authoring.
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCompositionResponse {
    pub artifact_id: String,
}

/// `canvas.video.render.start` — initiates a render job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderStartRequest {
    pub composition_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderStartResponse {
    pub job_id: String,
}

/// `canvas.video.render.cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderCancelRequest {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderCancelResponse {
    pub cancelled: bool,
}

/// Extension -> daemon chunk push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderFrameChunk {
    pub job_id: String,
    pub frame_idx: u32,
    pub chunk_idx: u32,
    pub total_chunks: u32,
    pub is_last: bool,
    pub bytes: Vec<u8>,
}

/// Ready ack from render page once composition is loaded + patched.
///
/// Retained for backward compat; the page-driven render loop (post-actor
/// rework) ignores this message — the first frame chunk implicitly signals
/// that rendering is in progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderReady {
    pub job_id: String,
}

/// Page -> daemon: "all frames sent, close the pipe and finalize."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderDone {
    pub job_id: String,
    /// Total frames the page actually emitted (for sanity checks).
    #[serde(default)]
    pub frames_emitted: u32,
}

/// Page -> daemon: "give me the composition HTML + spec for this job."
/// Served synchronously via `bridge:request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetCompositionRequest {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetCompositionResponse {
    pub html: String,
    pub width: u32,
    pub height: u32,
    pub duration_sec: f32,
    pub fps: u32,
}

/// Progress event (pushed on EventBus).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderProgress {
    pub job_id: String,
    pub step: String,
    pub current: u32,
    pub total: u32,
}

/// Terminal event: success.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderSucceeded {
    pub job_id: String,
    pub composition_id: String,
    pub output_path: String,
    pub size_bytes: u64,
    pub duration_ms: u64,
}

/// Terminal event: failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderFailed {
    pub job_id: String,
    pub error: String,
}

// ─────────────────────────────────────────────────────────────────────────
// Composition metadata (P3)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompositionKind {
    Composition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionMeta {
    pub kind: CompositionKind,
    pub version: u32,
    pub spec: CompositionSpec,
    pub origin: CompositionOrigin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionSpec {
    pub width: u32,
    pub height: u32,
    pub duration_sec: f32,
    pub fps: u32,
    #[serde(default)]
    pub bg: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositionOrigin {
    #[serde(default)]
    pub template: Option<String>,
    pub created_with: String,
    pub created_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum CompositionMetaError {
    #[error("invalid fps {0}: must be 24, 25, or 30")]
    InvalidFps(u32),
    #[error("invalid duration {0}: must be in [0.5, 60]")]
    InvalidDuration(f32),
    #[error("invalid dimensions {0}x{1}: both must be in [1, 1920]")]
    InvalidDimensions(u32, u32),
    #[error("version {0} not supported; expected 1")]
    UnsupportedVersion(u32),
}

impl CompositionMeta {
    pub fn validate_hard_limits(&self) -> Result<(), CompositionMetaError> {
        if self.version != 1 {
            return Err(CompositionMetaError::UnsupportedVersion(self.version));
        }
        if !matches!(self.spec.fps, 24 | 25 | 30) {
            return Err(CompositionMetaError::InvalidFps(self.spec.fps));
        }
        if !(0.5..=60.0).contains(&self.spec.duration_sec) {
            return Err(CompositionMetaError::InvalidDuration(
                self.spec.duration_sec,
            ));
        }
        if self.spec.width == 0
            || self.spec.width > 1920
            || self.spec.height == 0
            || self.spec.height > 1920
        {
            return Err(CompositionMetaError::InvalidDimensions(
                self.spec.width,
                self.spec.height,
            ));
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Lint protocol (P3)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LintSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintIssue {
    pub severity: LintSeverity,
    pub rule_id: String,
    pub message: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub col: Option<u32>,
    #[serde(default)]
    pub fix_hint: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LintReport {
    #[serde(default)]
    pub errors: Vec<LintIssue>,
    #[serde(default)]
    pub warnings: Vec<LintIssue>,
    #[serde(default)]
    pub infos: Vec<LintIssue>,
    #[serde(default)]
    pub elapsed_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintCompositionRequest {
    pub composition_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintCompositionResponse {
    #[serde(flatten)]
    pub report: LintReport,
}

#[cfg(test)]
mod meta_tests {
    use super::*;

    #[test]
    fn test_composition_meta_roundtrip() {
        let m = CompositionMeta {
            kind: CompositionKind::Composition,
            version: 1,
            spec: CompositionSpec {
                width: 1920,
                height: 1080,
                duration_sec: 30.0,
                fps: 30,
                bg: Some("#000000".into()),
            },
            origin: CompositionOrigin {
                template: Some("product-intro-16x9".into()),
                created_with: "canvas_create_composition".into(),
                created_at: 1745260800,
            },
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: CompositionMeta = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind, CompositionKind::Composition);
        assert_eq!(back.spec.width, 1920);
        assert_eq!(back.spec.fps, 30);
        assert_eq!(back.origin.template.as_deref(), Some("product-intro-16x9"));
    }

    #[test]
    fn test_composition_meta_rejects_wrong_kind() {
        let bad = serde_json::json!({
            "kind": "not-a-composition",
            "version": 1,
            "spec": {"width":100,"height":100,"duration_sec":1.0,"fps":30},
            "origin": {"created_with":"x","created_at":0}
        });
        let r: Result<CompositionMeta, _> = serde_json::from_value(bad);
        assert!(r.is_err(), "should reject kind != composition");
    }

    #[test]
    fn test_composition_meta_validate_hard_limits() {
        let mut m = CompositionMeta {
            kind: CompositionKind::Composition,
            version: 1,
            spec: CompositionSpec {
                width: 1920,
                height: 1080,
                duration_sec: 30.0,
                fps: 30,
                bg: None,
            },
            origin: CompositionOrigin {
                template: None,
                created_with: "t".into(),
                created_at: 0,
            },
        };
        assert!(m.validate_hard_limits().is_ok());
        m.spec.fps = 60;
        assert!(m.validate_hard_limits().is_err());
        m.spec.fps = 30;
        m.spec.duration_sec = 90.0;
        assert!(m.validate_hard_limits().is_err());
        m.spec.duration_sec = 30.0;
        m.spec.width = 3000;
        assert!(m.validate_hard_limits().is_err());
    }

    #[test]
    fn test_lint_issue_roundtrip() {
        let i = LintIssue {
            severity: LintSeverity::Warning,
            rule_id: "nf/cdn-whitelist".into(),
            message: "bad cdn".into(),
            line: Some(14),
            col: Some(3),
            fix_hint: Some("use esm.sh".into()),
        };
        let s = serde_json::to_string(&i).unwrap();
        let back: LintIssue = serde_json::from_str(&s).unwrap();
        assert_eq!(back.severity, LintSeverity::Warning);
        assert_eq!(back.rule_id, "nf/cdn-whitelist");
    }

    #[test]
    fn test_create_request_accepts_template_and_session_id() {
        let v = serde_json::json!({
            "title": "t",
            "width": 640, "height": 360,
            "duration_sec": 5.0, "fps": 30,
            "template": "product-intro-16x9",
            "session_id": "sess-abc"
        });
        let r: CreateCompositionRequest = serde_json::from_value(v).unwrap();
        assert_eq!(r.template.as_deref(), Some("product-intro-16x9"));
        assert_eq!(r.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn test_create_request_backward_compat_without_new_fields() {
        let v = serde_json::json!({
            "title": "t",
            "width": 640, "height": 360,
            "duration_sec": 5.0, "fps": 30
        });
        let r: CreateCompositionRequest = serde_json::from_value(v).unwrap();
        assert!(r.template.is_none());
        assert!(r.session_id.is_none());
    }
}
