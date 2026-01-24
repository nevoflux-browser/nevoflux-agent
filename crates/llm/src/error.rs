use thiserror::Error;

#[derive(Error, Debug)]
pub enum LlmError {
    #[error("Not implemented")]
    NotImplemented,
}

pub type Result<T> = std::result::Result<T, LlmError>;
