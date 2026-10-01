//! `usage.export_calls`: the per-call usage rows as CSV, for the cost
//! breakdown that fixes G2's X (spec §9 M4).

pub use nevoflux_storage::repositories::LlmCallRow;

const HEADER: &str = "ts,session_id,role,model,input,output,cache_read,cache_write,estimated";

/// RFC 4180 quoting for one field.
fn field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn opt(v: Option<u64>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

/// The rows as CSV with a header line.
pub fn to_csv(rows: &[LlmCallRow]) -> String {
    let mut out = String::from(HEADER);
    out.push('\n');
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            r.ts,
            field(&r.session_id),
            field(&r.role),
            field(&r.model),
            r.input,
            r.output,
            opt(r.cache_read),
            opt(r.cache_write),
            u8::from(r.estimated),
        ));
    }
    out
}

const CMD: &str = "usage.export_calls";
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// `usage.export_calls` with optional `since_ms` (default: seven days ago):
/// `{data: {csv}}`.
pub fn handle_export(
    params: &serde_json::Value,
    database: &nevoflux_storage::Database,
) -> serde_json::Value {
    let request_id = crate::local::rpc::request_id(params);
    let since_ms = params
        .get("since_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
                - WEEK_MS
        });
    match nevoflux_storage::repositories::LlmCallRepository::new(database).list_since(since_ms) {
        Ok(rows) => crate::kb_wizard::ok_response(
            &request_id,
            CMD,
            serde_json::json!({ "csv": to_csv(&rows), "rows": rows.len() }),
        ),
        Err(e) => crate::kb_wizard::err_response(&request_id, CMD, "FAILED", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_export_rpc_returns_rows_since_the_given_time() {
        let db = nevoflux_storage::Database::open_in_memory().unwrap();
        let repo = nevoflux_storage::repositories::LlmCallRepository::new(&db);
        for (ts, role) in [(100, "main"), (200, "jev")] {
            repo.append(&LlmCallRow {
                ts,
                session_id: "s".into(),
                role: role.into(),
                model: String::new(),
                input: 1,
                output: 1,
                cache_read: None,
                cache_write: None,
                estimated: false,
            })
            .unwrap();
        }
        let resp = handle_export(
            &serde_json::json!({"request_id": "r", "since_ms": 150}),
            &db,
        );
        assert_eq!(resp["payload"]["success"], true, "{resp}");
        assert_eq!(resp["payload"]["command"], "usage.export_calls");
        let csv = resp["payload"]["data"]["csv"].as_str().unwrap();
        assert_eq!(csv.lines().count(), 2, "header + the jev row: {csv}");
        assert!(csv.contains(",jev,"));
    }

    #[test]
    fn export_csv_has_a_header_and_one_line_per_row() {
        let rows = vec![
            LlmCallRow {
                ts: 1,
                session_id: "s,1".into(),
                role: "main".into(),
                model: "k3".into(),
                input: 100,
                output: 5,
                cache_read: Some(90),
                cache_write: None,
                estimated: false,
            },
            LlmCallRow {
                ts: 2,
                session_id: "s2".into(),
                role: "jev".into(),
                model: String::new(),
                input: 30,
                output: 2,
                cache_read: None,
                cache_write: None,
                estimated: true,
            },
        ];
        let csv = to_csv(&rows);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(
            lines[0],
            "ts,session_id,role,model,input,output,cache_read,cache_write,estimated"
        );
        assert_eq!(lines[1], "1,\"s,1\",main,k3,100,5,90,,0");
        assert_eq!(lines[2], "2,s2,jev,,30,2,,,1");
        assert_eq!(lines.len(), 3);
    }
}
