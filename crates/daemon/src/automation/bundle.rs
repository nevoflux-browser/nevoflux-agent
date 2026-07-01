//! Debug-bundle collector + writer (P6). Accumulates per-step artifacts
//! (screenshots, DOM) during a task and writes them to the workspace at end —
//! best-effort, including on failure, so a failed unattended run leaves a
//! forensic trail on the mounted workspace volume.

use std::path::{Path, PathBuf};

/// One captured step artifact.
#[derive(Debug, Clone)]
pub enum StepArtifact {
    /// A PNG screenshot for a step.
    Screenshot { step: u32, png: Vec<u8> },
    /// A DOM snapshot for a step.
    Dom { step: u32, html: String },
}

/// Accumulates step artifacts for a task.
#[derive(Debug, Default)]
pub struct DebugBundle {
    steps: Vec<StepArtifact>,
}

impl DebugBundle {
    /// Create an empty bundle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a screenshot for `step`.
    pub fn add_screenshot(&mut self, step: u32, png: Vec<u8>) {
        self.steps.push(StepArtifact::Screenshot { step, png });
    }

    /// Record a DOM snapshot for `step`.
    pub fn add_dom(&mut self, step: u32, html: String) {
        self.steps.push(StepArtifact::Dom { step, html });
    }

    /// Number of recorded artifacts.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Write the bundle under `<workspace>/debug-bundle/` with a `manifest.json`.
    /// Best-effort: individual artifact write failures are tolerated; returns the
    /// bundle directory.
    pub fn write_to(&self, workspace: &Path) -> std::io::Result<PathBuf> {
        let dir = workspace.join("debug-bundle");
        std::fs::create_dir_all(&dir)?;
        let mut manifest: Vec<String> = Vec::new();
        for art in &self.steps {
            match art {
                StepArtifact::Screenshot { step, png } => {
                    let name = format!("step-{step:04}.png");
                    let _ = std::fs::write(dir.join(&name), png);
                    manifest.push(name);
                }
                StepArtifact::Dom { step, html } => {
                    let name = format!("step-{step:04}.dom.html");
                    let _ = std::fs::write(dir.join(&name), html);
                    manifest.push(name);
                }
            }
        }
        let manifest_json = serde_json::to_string_pretty(&manifest).unwrap_or_default();
        let _ = std::fs::write(dir.join("manifest.json"), manifest_json);
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_writes_artifacts_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = DebugBundle::new();
        assert!(b.is_empty());
        b.add_screenshot(1, vec![1, 2, 3]);
        b.add_dom(1, "<html></html>".into());
        b.add_screenshot(2, vec![4, 5]);
        assert_eq!(b.len(), 3);

        let dir = b.write_to(tmp.path()).unwrap();
        assert!(dir.join("step-0001.png").exists());
        assert!(dir.join("step-0001.dom.html").exists());
        assert!(dir.join("step-0002.png").exists());
        let manifest = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(manifest.contains("step-0001.png"));
        assert!(manifest.contains("step-0002.png"));
    }
}
