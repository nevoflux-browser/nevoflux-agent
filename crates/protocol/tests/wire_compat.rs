use nevoflux_protocol::chat::Artifact;

#[test]
fn test_artifact_wire_compat_missing_is_persistent() {
    // Old JSON without is_persistent field (from older senders)
    let old_json = r#"{"id":"art-001","title":"My Dashboard","content_type":"text/html","description":null,"content":"<html><body><h1>Hello</h1></body></html>","files":null,"entry":null}"#;
    let artifact: Artifact = serde_json::from_str(old_json).expect("Should deserialize");
    assert_eq!(
        artifact.is_persistent, false,
        "Missing is_persistent should default to false"
    );
    assert_eq!(artifact.id, "art-001");
}

#[test]
fn test_artifact_wire_compat_with_is_persistent_true() {
    // New JSON with is_persistent explicitly set to true
    let new_json = r#"{"id":"art-002","title":"My Dashboard","content_type":"text/html","description":null,"content":"<html><body><h1>Hello</h1></body></html>","files":null,"entry":null,"is_persistent":true}"#;
    let artifact: Artifact = serde_json::from_str(new_json).expect("Should deserialize");
    assert_eq!(artifact.is_persistent, true);
    assert_eq!(artifact.id, "art-002");
}

#[test]
fn test_artifact_wire_compat_with_is_persistent_false() {
    // New JSON with is_persistent explicitly set to false
    let new_json = r#"{"id":"art-003","title":"My Dashboard","content_type":"text/html","description":null,"content":"<html><body><h1>Hello</h1></body></html>","files":null,"entry":null,"is_persistent":false}"#;
    let artifact: Artifact = serde_json::from_str(new_json).expect("Should deserialize");
    assert_eq!(artifact.is_persistent, false);
    assert_eq!(artifact.id, "art-003");
}
