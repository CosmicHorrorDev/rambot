//! The global config for the bot
//!
//! Considering only one bot is running at any given time the config is simply a global
//! `RwLock<_>` which dumps its internal representation on mutations

use std::{
    collections::BTreeMap, error::Error as StdError, fmt, io, path::PathBuf,
    result::Result as StdResult, str::FromStr, sync::Arc,
};

use crate::{error::DbError, Error, Result};

use serde::{Deserialize, Serialize};
use teloxide::types;
use tokio::{fs, sync::RwLock};

#[derive(Clone)]
pub struct Db {
    inner: Arc<RwLock<Inner>>,
    path: PathBuf,
}

impl Db {
    pub async fn load() -> Result<Self> {
        let path = Self::db_path()?;
        let inner: Inner = match fs::read_to_string(&path).await {
            Ok(contents) => ron::from_str(&contents).map_err(DbError::FailedDeserialize),
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    log::warn!("No existing db found. Loading default configuration");
                    Ok(Inner::default())
                } else {
                    Err(DbError::FailedRead(e))
                }
            }
        }?;
        let inner = Arc::new(RwLock::new(inner));
        Ok(Self { inner, path })
    }

    // TODO: `.write()` really shouldn't be called outside of this. Restrict the API more?
    async fn dump_after<F>(&self, f: F) -> Result
    where
        F: FnOnce(&mut Inner) -> Result,
    {
        let mut write_handle = self.inner.write().await;
        let prev = write_handle.clone();
        let delayed_res = f(&mut *write_handle);
        // Roll the db back if there was an error
        if let Err(e) = &delayed_res {
            *write_handle = prev.clone();
            log::warn!("Aborted db transaction due to error: {e}");
        } else {
            if prev == *write_handle {
                log::trace!("Skipping dumping identical db state");
            } else {
                let contents =
                    ron::ser::to_string_pretty(&*write_handle, ron::ser::PrettyConfig::new())
                        .map_err(DbError::FailedSerialize)?;
                fs::create_dir_all(&self.path.parent().unwrap())
                    .await
                    .map_err(DbError::FailedWrite)?;
                fs::write(&self.path, &contents)
                    .await
                    .map_err(DbError::FailedWrite)?;
                log::debug!("Dumped new database info");
            }
        }

        delayed_res
    }

    fn db_path() -> Result<PathBuf> {
        match dirs::data_dir() {
            Some(dir) => Ok(dir.join("rambot").join("db.ron")),
            None => Err(Error::UnknownDataDir),
        }
    }

    pub async fn update_metadata(&self, msg: &types::Message) -> Result {
        self.dump_after(|inner| {
            let chat = inner
                .chats
                .entry(msg.chat.id)
                .or_insert_with(|| Chat::new(ChatKind::Private));
            chat.kind = ChatKind::from(&msg.chat.kind);

            if let types::MessageKind::Common(types::MessageCommon {
                from: Some(user), ..
            }) = &msg.kind
            {
                inner.users.entry(user.id).or_default();
            }

            Ok(())
        })
        .await
    }

    pub async fn get_chat_ids_by_public_title(&self, title: &str) -> Vec<types::ChatId> {
        let inner = self.inner.read().await;

        inner
            .chats
            .iter()
            .filter_map(|(id, chat)| match &chat.kind {
                ChatKind::Private => None,
                ChatKind::Public(public) => public
                    .title
                    .as_deref()
                    .map_or(false, |t| t == title)
                    .then_some(*id),
            })
            .collect()
    }

    pub async fn get_sidecar_attach(
        &self,
        chat_id: types::ChatId,
    ) -> Result<Option<SidecarAttach>> {
        match self.inner.read().await.chats.get(&chat_id) {
            Some(chat) => Ok(chat.sidecar_attach.clone()),
            None => Err(Error::MissingChat(chat_id)),
        }
    }

    pub async fn attach_sidecar(
        &self,
        chat_id: types::ChatId,
        sidecar_id: types::ChatId,
    ) -> Result {
        self.dump_after(|inner| {
            if let Some(Chat {
                sidecar_attach: Some(attach),
                ..
            }) = inner.chats.get(&chat_id)
            {
                return Err(Error::ChatAlreadyHasAttach(attach.self_kind));
            };
            if let Some(Chat {
                sidecar_attach: Some(attach),
                ..
            }) = inner.chats.get(&sidecar_id)
            {
                return Err(Error::SidecarAlreadyHasAttach(attach.self_kind));
            };

            let chat = inner.chats.get_mut(&chat_id).unwrap();
            chat.sidecar_attach = Some(SidecarAttach::has_sidecar(sidecar_id));
            let sidecar = inner.chats.get_mut(&sidecar_id).unwrap();
            sidecar.sidecar_attach = Some(SidecarAttach::is_sidecar(chat_id));

            Ok(())
        })
        .await
    }

    pub async fn detach_sidecar(&self, chat_id: types::ChatId) -> Result {
        self.dump_after(|inner| {
            let chat = inner
                .chats
                .get_mut(&chat_id)
                .ok_or_else(|| Error::MissingChat(chat_id))?;
            let sidecar_attach = chat
                .sidecar_attach
                .take()
                .ok_or(Error::MissingSidecarAttach)?;

            // Sidecar should always have a valid attachment
            let sidecar = inner
                .chats
                .get_mut(&sidecar_attach.to)
                .ok_or(DbError::Corrupt)?;
            // Sidecar itself wasn't attached to anything
            sidecar.sidecar_attach.take().ok_or(DbError::Corrupt)?;

            Ok(())
        })
        .await
    }

    async fn get_transcribe_trigger(&self, user_id: types::UserId) -> Result<TranscribeTrigger> {
        match self.inner.read().await.users.get(&user_id) {
            Some(user) => Ok(user.trigger),
            None => Err(Error::MissingUser(user_id)),
        }
    }

    async fn set_transcribe_trigger(
        &self,
        user_id: types::UserId,
        trigger: TranscribeTrigger,
    ) -> Result {
        self.dump_after(|inner| match inner.users.get_mut(&user_id) {
            Some(user) => {
                user.trigger = trigger;
                Ok(())
            }
            None => Err(Error::MissingUser(user_id)),
        })
        .await
    }

    pub async fn is_trusted_user(&self, user_id: types::UserId) -> Result<bool> {
        match self.inner.read().await.users.get(&user_id) {
            Some(user) => Ok(user.trusted_user.is_some()),
            None => Err(Error::MissingUser(user_id)),
        }
    }

    pub async fn add_trusted_user(&self, user_id: types::UserId, name: String) -> Result {
        self.dump_after(|inner| {
            let user = inner.users.entry(user_id).or_default();
            user.trusted_user = Some(name);
            Ok(())
        })
        .await
    }

    // NOTE: we MUST NEVER remove a user from the database to keep the invariant that a user
    // returned from here is valid forever
    pub async fn user(&self, user_id: types::UserId) -> Option<DbUser> {
        if self.inner.read().await.users.get(&user_id).is_some() {
            Some(DbUser {
                db: self.to_owned(),
                user_id,
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
struct Inner {
    chats: BTreeMap<types::ChatId, Chat>,
    users: BTreeMap<types::UserId, User>,
}

pub struct DbUser {
    db: Db,
    user_id: types::UserId,
}

impl DbUser {
    pub async fn is_trusted(&self) -> bool {
        self.db.is_trusted_user(self.user_id).await.unwrap()
    }

    pub async fn get_transcribe_trigger(&self) -> TranscribeTrigger {
        self.db.get_transcribe_trigger(self.user_id).await.unwrap()
    }

    pub async fn set_transcribe_trigger(&self, trigger: TranscribeTrigger) -> Result {
        self.db.set_transcribe_trigger(self.user_id, trigger).await
    }
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
struct User {
    trusted_user: Option<String>,
    trigger: TranscribeTrigger,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub enum TranscribeTrigger {
    #[default]
    Never,
    SummonBySelf,
    SummonByAny,
    Always,
}

impl TranscribeTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::SummonBySelf => "self",
            Self::SummonByAny => "anyone",
            Self::Always => "always",
        }
    }

    pub fn desc(self) -> &'static str {
        match self {
            Self::Never => "Never generate transcriptions for your voice messages",
            Self::SummonBySelf => {
                "You can summon the bot on your voice messages to start a transcription"
            }
            Self::SummonByAny => {
                "Anyone can summon the bot on your voice messages to start a transcription"
            }
            Self::Always => {
                "The bot will always automatically transcribe your voice messages when possible"
            }
        }
    }
}

impl FromStr for TranscribeTrigger {
    type Err = ParseTriggerError;

    fn from_str(s: &str) -> StdResult<Self, Self::Err> {
        let trigger = match s {
            "never" => Self::Never,
            "self" => Self::SummonBySelf,
            "anyone" => Self::SummonByAny,
            "always" => Self::Always,
            unknown => return Err(ParseTriggerError(unknown.to_owned())),
        };
        // Sanity check that the values all match
        assert_eq!(s, trigger.as_str());

        Ok(trigger)
    }
}

pub struct ParseTriggerError(String);

impl fmt::Debug for ParseTriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Unknown trigger: {}. Accepted values: never, self, anyone, or always",
            self.0
        )
    }
}

impl fmt::Display for ParseTriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl StdError for ParseTriggerError {}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct Chat {
    kind: ChatKind,
    sidecar_attach: Option<SidecarAttach>,
}

impl Chat {
    fn new(kind: ChatKind) -> Self {
        Self {
            kind,
            sidecar_attach: None,
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
enum ChatKind {
    Public(ChatPublic),
    Private,
}

impl From<&types::ChatKind> for ChatKind {
    fn from(kind: &types::ChatKind) -> Self {
        match kind {
            types::ChatKind::Public(public) => Self::Public(public.into()),
            types::ChatKind::Private(_) => Self::Private,
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct ChatPublic {
    title: Option<String>,
}

impl From<&types::ChatPublic> for ChatPublic {
    fn from(public: &types::ChatPublic) -> Self {
        Self {
            title: public.title.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SidecarAttach {
    pub to: types::ChatId,
    pub self_kind: SidecarKind,
}

impl SidecarAttach {
    fn is_sidecar(to: types::ChatId) -> Self {
        Self {
            to,
            self_kind: SidecarKind::IsSidecar,
        }
    }

    fn has_sidecar(to: types::ChatId) -> Self {
        Self {
            to,
            self_kind: SidecarKind::HasSidecar,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum SidecarKind {
    IsSidecar,
    HasSidecar,
}
