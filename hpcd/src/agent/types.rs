use proto::StreamEvent;
use std::pin::Pin;
use thiserror::Error as ThisError;
use tokio_stream::Stream;
use tonic::Status;

pub type OutStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, Status>> + Send + Sync + 'static>>;

#[derive(Debug, PartialEq, Eq, ThisError)]
pub enum AgentSvcError {
    #[error("unknown hostid")]
    UnknownHostId,

    #[error("network error: {0}")]
    NetworkError(String),

    #[error("database error: {error}")]
    DatabaseError { error: String },
}
