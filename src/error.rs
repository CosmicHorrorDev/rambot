use std::{io, result::Result as StdResult};

use crate::db;

use teloxide::types;
use thiserror::Error as ThisError;

pub type InitResult<T = ()> = StdResult<T, InitError>;
pub type HandlerResult<T = ()> = StdResult<T, HandlerError>;
pub type DbResult<T = ()> = StdResult<T, DbError>;

#[derive(Debug, ThisError)]
pub enum InitError {
    #[error("{0}")]
    BotCommands(teloxide::RequestError),
    #[error("Failed loading the database: {0}")]
    DbLoad(#[from] DbError),
    #[error("Unable to detect bot name")]
    InvalidBotName,
}

#[derive(Debug, ThisError)]
pub enum HandlerError {
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
    #[error("{0}")]
    UserError(#[from] UserError),
    #[error("A silent way for a handler to bail")]
    Ignore,
}

impl HandlerError {
    pub fn worker_died<E: std::error::Error>(_: E) -> Self {
        Self::WorkerDied
    }
}

impl From<teloxide::utils::command::ParseError> for HandlerError {
    fn from(parse_error: teloxide::utils::command::ParseError) -> Self {
        Self::UserError(parse_error.into())
    }
}

// TODO: rename to `UserFacing`
#[derive(Debug, ThisError)]
pub enum UserError {
    #[error("{0}")]
    CommandParseError(#[from] teloxide::utils::command::ParseError),
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
    #[error("Your message should be a reply to another message")]
    NotReply,
    #[error("Your message should be a reply to a voice message")]
    ReplyNotVoice,
    #[error("I can't see the author of the message you're replying to")]
    ReplyUnknownAuthor,
    #[error("I can't transcribe as that user has their trigger set to {0}")]
    BadSummon(db::TranscribeTrigger),
    #[error("No chat found titled: {0:?}")]
    NoChatTitled(String),
    #[error("Ambiguous request. Multiple chats were found with that title")]
    AmbiguousChatTitle,
}

#[derive(Debug, ThisError)]
pub enum DbError {
    #[error("The database could not find its home")]
    NoDataDir,
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
