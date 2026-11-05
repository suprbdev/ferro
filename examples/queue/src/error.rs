/// Every way a queue operation can fail, from a lost lease to a database error.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error(transparent)]
    Ferro(#[from] ferro::Error),
    #[error("no such job: {0}")]
    NotFound(u64),
    #[error("lease on job {0} is lost")]
    LeaseLost(u64),
    #[error("unknown dependency: {0}")]
    UnknownDependency(u64),
    #[error("job {0} is not in the dead letter queue")]
    NotDead(u64),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, QueueError>;
