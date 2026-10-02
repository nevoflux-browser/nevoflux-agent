//! The daemon reads the same fixtures the Muse client pins (distribution
//! design §8.2). If one of these fails, the two sides disagree about the wire.

use nevoflux_daemon::remote::channel_codec;
use nevoflux_daemon::remote::envelope::{self, InboundVerifier};
use nevoflux_daemon::remote::mcp_server::PROTOCOL;
use nevoflux_daemon::remote::relay_protocol::WireMessage;
use nevoflux_daemon::remote::session::Wire;
use nevoflux_mcp::rmcp::service::RxJsonRpcMessage;
use nevoflux_mcp::rmcp::RoleServer;
use serde_json::Value;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/muse");

fn json(name: &str) -> Value {
    let text = std::fs::read_to_string(format!("{DIR}/{name}")).expect("fixture exists");
    serde_json::from_str(&text).expect("fixture is JSON")
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn protocol_version_matches_the_server() {
    let text = std::fs::read_to_string(format!("{DIR}/PROTOCOL_VERSION")).unwrap();
    assert_eq!(text.trim(), PROTOCOL.to_string());
}

#[test]
fn kdf_vectors_match_the_daemon() {
    for v in json("kdf.json")["vectors"].as_array().unwrap() {
        let key = nevoflux_daemon::remote::crypto::derive_channel_key(
            v["code"].as_str().unwrap(),
            v["channel_id"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(key.to_vec(), unhex(v["key_hex"].as_str().unwrap()));
    }
}

#[test]
fn the_daemon_only_prints_canonical_codes() {
    // The daemon never normalizes because it only ever prints the canonical
    // form; this pins that, against the fixture's own alphabet.
    let f = json("pairing_code.json");
    let alphabet = f["alphabet"].as_str().unwrap();
    for _ in 0..50 {
        let code = nevoflux_daemon::share::generate_password();
        let groups: Vec<&str> = code.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![1, 4, 4, 4],
            "{code}"
        );
        assert!(
            code.chars()
                .filter(|c| *c != '-')
                .all(|c| alphabet.contains(c)),
            "{code}"
        );
    }
    for v in f["valid"].as_array().unwrap() {
        let canonical = v["canonical"].as_str().unwrap();
        assert_eq!(canonical.len(), 16, "{canonical}");
    }
}

#[test]
fn the_sealed_vector_opens_to_its_plaintext() {
    let f = json("seal.json");
    let key: [u8; 32] = unhex(f["key_hex"].as_str().unwrap()).try_into().unwrap();
    let wire = Wire::Binary(unhex(f["sealed_hex"].as_str().unwrap()));
    match channel_codec::decode(Some(&key), &wire) {
        Some(WireMessage::Frame { seq: None, frame }) => {
            assert_eq!(frame, f["plaintext"]["frame"]);
        }
        other => panic!("did not open as a seq-less frame: {other:?}"),
    }
}

#[test]
fn envelope_cases_match_the_verifier() {
    let f = json("envelope.json");
    let challenge = f["challenge"].as_str().unwrap();
    for case in f["cases"].as_array().unwrap() {
        let mut v = InboundVerifier::new(challenge.to_string());
        for p in case["prior"].as_array().unwrap() {
            v.accept(p).expect("prior frames are accepted");
        }
        let got = match v.accept(&case["frame"]) {
            Ok(_) => "accept".to_string(),
            Err(r) => format!("reject:{}", r.as_str()),
        };
        assert_eq!(
            got,
            case["expect"].as_str().unwrap(),
            "case {}",
            case["name"]
        );
    }
}

#[test]
fn the_refusal_budget_matches_the_verifier() {
    assert_eq!(
        json("envelope.json")["bad_frame_limit"].as_u64(),
        Some(u64::from(envelope::BAD_FRAME_LIMIT))
    );
}

#[test]
fn example_requests_are_valid_json_rpc_for_rmcp() {
    for r in json("mcp_messages.json")["requests"].as_array().unwrap() {
        serde_json::from_value::<RxJsonRpcMessage<RoleServer>>(r.clone())
            .unwrap_or_else(|e| panic!("{r} is not a server-bound message: {e}"));
    }
}
