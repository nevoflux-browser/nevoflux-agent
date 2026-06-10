//! pack.validate handler: well-formed manifest passes; a seed not covered by
//! protected is reported as a violation.
use serde_json::json;

#[test]
fn validate_reports_violation_for_unprotected_seed() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("pack.toml");
    std::fs::write(
        &manifest,
        "[pack]\nname=\"demo\"\nversion=\"0.1.0\"\nprotocol=\"pack-protocol/0.1\"\nmin_nevoflux=\"0.3.0\"\n\
         [[components.seed]]\nslug=\"demo/cv\"\nfrom=\"seed/cv.md\"\n",
    )
    .unwrap();

    // The dispatch flattens params into the top-level object and adds
    // request_id; mirror that shape here.
    let params = json!({
        "request_id": "r1",
        "manifest_path": manifest.to_str().unwrap()
    });
    let resp = nevoflux_daemon::pack::rpc::handle_pack_validate(&params);
    let payload = &resp["payload"];
    assert_eq!(payload["success"], true);
    assert_eq!(payload["data"]["ok"], false);
    let violations = payload["data"]["violations"].as_array().unwrap();
    assert!(violations
        .iter()
        .any(|v| v.as_str().unwrap().contains("SeedNotProtected")));
}
