//! Live brain-share Worker smoke test. Network-dependent; ignored by
//! default. Run with:
//!   cargo test -p nevoflux-daemon --test brain_share_live -- --ignored --nocapture
//! Honors NEVOFLUX_BRAIN_SHARE_URL (defaults to the prod Worker).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use nevoflux_daemon::brain_share::BrainShareHttpClient;
use nevoflux_daemon::share::owner_token::{generate_owner_token, hash_owner_token};
use nevoflux_daemon::share::share_id::generate_share_id;

fn test_url() -> String {
    std::env::var("NEVOFLUX_BRAIN_SHARE_URL")
        .unwrap_or_else(|_| "https://share.nevoflux.app".into())
}

fn nbrain_blob() -> Vec<u8> {
    let mut v = vec![0u8; 64];
    v[..4].copy_from_slice(b"NBRN");
    v
}

#[tokio::test]
#[ignore]
async fn brain_upload_fetch_renew_revoke_roundtrip() {
    let client = BrainShareHttpClient::new(test_url()).unwrap();
    let share_id = generate_share_id();
    let token = generate_owner_token();
    let hash = hash_owner_token(&share_id, &token);
    let token_b64 = URL_SAFE_NO_PAD.encode(&token);

    let up = client
        .upload(&share_id, &nbrain_blob(), &hash, 3600)
        .await
        .unwrap();
    assert_eq!(up.share_id, share_id);

    let bytes = client.fetch_bundle(&share_id).await.unwrap();
    assert_eq!(&bytes[..4], b"NBRN");

    let renewed = client.renew(&share_id, &token_b64, 3600).await.unwrap();
    assert_eq!(renewed.share_id, share_id);

    client.revoke(&share_id, &token_b64).await.unwrap();
    assert!(client.fetch_bundle(&share_id).await.is_err());
}
