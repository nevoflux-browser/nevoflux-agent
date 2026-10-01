//! Fake System One servers for Jev tests.

use std::future::IntoFuture;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{routing::post, Router};

pub(crate) async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .into_future(),
    );
    format!("http://{addr}/v1/systemone")
}

/// Answers `answer` after `delay`, keeping every request body it saw.
pub(crate) async fn answering(
    answer: serde_json::Value,
    delay: Duration,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let b = bodies.clone();
    let app = Router::new().route(
        "/v1/systemone",
        post(move |body: String| {
            let b = b.clone();
            let answer = answer.clone();
            async move {
                b.lock().unwrap().push(body);
                tokio::time::sleep(delay).await;
                axum::Json(answer)
            }
        }),
    );
    (spawn(app).await, bodies)
}

pub(crate) fn one_noul() -> std::collections::BTreeMap<String, super::wire::Question> {
    let mut q = std::collections::BTreeMap::new();
    q.insert(
        "x".into(),
        super::wire::Question::Noul {
            instructions: "i".into(),
            when_true: "t".into(),
            when_false: "f".into(),
        },
    );
    q
}
