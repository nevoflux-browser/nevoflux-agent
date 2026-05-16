use thiserror::Error;

pub type EvalResult<T> = Result<T, EvalError>;

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("daemon connection failed: {0}")]
    DaemonConnection(String),

    #[error("daemon returned error: {0}")]
    DaemonError(String),

    #[error("benchmark `{name}` not found or not enabled")]
    BenchmarkNotFound { name: String },

    #[error("task file `{path}` malformed: {reason}")]
    TaskParse { path: String, reason: String },

    #[error("judge `{judge}` failed on task `{task_id}`: {reason}")]
    JudgeFailure {
        judge: String,
        task_id: String,
        reason: String,
    },

    #[error("timeout after {seconds}s on task `{task_id}`")]
    Timeout { task_id: String, seconds: u64 },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),

    #[error("other: {0}")]
    Other(String),
}
