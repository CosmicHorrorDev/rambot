use thiserror::Error as ThisError;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Telegram API download error: {0}")]
    Download(#[from] teloxide::DownloadError),
    #[error("Telegram API request error: {0}")]
    Request(#[from] teloxide::RequestError),
    #[error("Encountered invalid UTF-8 text: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("The message task has stopped responding")]
    MessageHandleDied,
    #[error("The transcription worker died :c")]
    WorkerDied,
}

impl Error {
    pub(crate) fn worker_died<E: std::error::Error>(_: E) -> Self {
        Self::WorkerDied
    }
}
