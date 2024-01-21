use std::{env, io};

use crate::db;

use teloxide::types;
use thiserror::Error as ThisError;

pub type Result<T = ()> = std::result::Result<T, Error>;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
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
    #[error("The worker for sending new messages died :c")]
    SendMsgWorkerDied,
    #[error("A worker for updating an existing message died :c")]
    UpdateMsgWorkerDied,
    #[error("Can't locate the user's data directory")]
    UnknownDataDir,
    #[error("Database error: {0}")]
    DbError(#[from] DbError),
    #[error("Missing entry for user {0}")]
    MissingUser(types::UserId),
    #[error("No sidecar attachment found")]
    MissingSidecarAttach,
    #[error("Chat {0} doesn't exist")]
    MissingChat(types::ChatId),
    #[error("Chat already has a sidecar attachment: {0:?}")]
    ChatAlreadyHasAttach(db::SidecarKind),
    #[error("Sidecar chat already has a sidecar attachment: {0:?}")]
    SidecarAlreadyHasAttach(db::SidecarKind),
    #[error("`$BOT_NAME` should be set to the bot's name. Error: {0}")]
    InvalidBotName(env::VarError),
    #[error("{0}")]
    CommandParseError(#[from] teloxide::utils::command::ParseError),
}

impl Error {
    pub(crate) fn worker_died<E: std::error::Error>(_: E) -> Self {
        Self::WorkerDied
    }
}

#[derive(Debug, ThisError)]
pub enum DbError {
    #[error("The database is corrupt!! >>:V")]
    Corrupt,
    #[error("Failed reading the database. Error: {0}")]
    FailedRead(io::Error),
    #[error("Failed writing the database. Error: {0}")]
    FailedWrite(io::Error),
    #[error("Failed deserializing the database. Error: {0}")]
    FailedDeserialize(ron::error::SpannedError),
    #[error("Failed serializing the database. Error: {0}")]
    FailedSerialize(ron::Error),
}
