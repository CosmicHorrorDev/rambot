// TODO: Overall reorganization:
// - Switch the whole `buf_messenger` naming and organization into a `LiveMessage` or something
//   like that
// - Split the transcription logic into a `PreviewMessage` and `LongMessage` that can be used
//   separately (DMs default to just long, groupchats default to just preview)
// - Add a little styling to the messages to make different parts more obvious
// - Add a little scheduler before the transcriber (download file into a temp dir to hand to the
//   transcriber. Auto cleans up the file and speeds up transcriptions by pre-downloading). This
//   can be represnted by the job holding a temp dir
// - Acquire a lockfile to start to ensure we're the only bot running?

mod buf_messenger;
mod command;
mod db;
mod error;
mod telegram;
mod transcriber;
mod utils;

use std::{convert::Infallible, sync::OnceLock, time::Instant};

use buf_messenger::UpdateMsgHandle;
use db::TranscribeTrigger;
pub use error::{HandlerError, HandlerResult, InitError, InitResult, UserError};

use telegram::Message;
use teloxide::{
    dispatching::{Dispatcher, UpdateFilterExt},
    types,
    utils::command::{BotCommands, ParseError as CommandParseError},
};
use utils::Line;

static BOT_NAME: OnceLock<String> = OnceLock::new();

#[derive(Clone)]
struct State {
    transcriber_pool: transcriber::Pool,
    // TODO: move this into `telegram::Bot`
    send_msg_handle: buf_messenger::SendMsgHandle,
    db: db::Db,
}

#[tokio::main]
async fn main() -> InitResult {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!(".env error: {e}");
    }
    pretty_env_logger::init();
    log::info!("Logging started");

    let db = db::Db::load().await?;

    let bot = telegram::Bot::from_env();
    bot.set_my_commands(command::Command::bot_commands())
        .await
        .map_err(InitError::BotCommands)?;
    let handler = types::Update::filter_message().endpoint(
        |bot: teloxide::Bot, state: State, msg: types::Message| async move {
            handle_message(bot.into(), state, msg).await;
            Ok::<_, Infallible>(())
        },
    );

    let transcribers = transcriber::Pool::spawn(2).await;
    let send_msg_handle = buf_messenger::init(bot.clone());
    let name = bot
        .get_me()
        .await
        .ok()
        .and_then(|me| me.user.username)
        .ok_or(InitError::InvalidBotName)?;
    BOT_NAME.get_or_init(|| name);
    let state = State {
        transcriber_pool: transcribers,
        send_msg_handle,
        db,
    };
    Dispatcher::builder(bot.0, handler)
        // The default distribution_function runs each chat sequentially. Run everything
        // concurrently instead. Embrace the async
        .distribution_function::<()>(|_| None)
        .dependencies(teloxide::dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

const SHORT_MSG_CUTOFF_SECS: u32 = 45;
const LONG_MSG_CHUNK_CUTOFF_SECS: u32 = 240;

struct Transcription {
    transcription: Vec<Line>,
    status: Option<String>,
    message: TranscriptionLong,
}

impl Transcription {
    async fn start<S: Into<String>>(
        duration_secs: u32,
        status_text: S,
        bot: telegram::Bot,
        send_msg_handle: buf_messenger::SendMsgHandle,
        chat_id: types::ChatId,
        msg_id: types::MessageId,
        sidecar_id: Option<types::ChatId>,
    ) -> HandlerResult<Self> {
        let status_text = status_text.into();
        let (long_msg_chat, long_msg_reply_to, maybe_sidecar) = match sidecar_id {
            Some(sidecar_id) => {
                let forwarded = bot.forward_message(sidecar_id, chat_id, msg_id).await?;
                let forwarded_id = forwarded.id();
                let preview = send_msg_handle.dispatch_send_msg(chat_id, msg_id, &status_text)?;
                let with_sidecar = WithSidecar { preview, forwarded };
                (sidecar_id, forwarded_id, Some(with_sidecar))
            }
            None => (chat_id, msg_id, None),
        };
        let mut multipart = Vec::new();
        let num_parts = 1 + duration_secs / LONG_MSG_CHUNK_CUTOFF_SECS;
        for index in 0..num_parts {
            let chunk = send_msg_handle.dispatch_send_msg(
                long_msg_chat,
                long_msg_reply_to,
                &format!("[{}/{}] {}", index + 1, num_parts, status_text),
            )?;
            multipart.push(chunk);
        }

        Ok(Self {
            transcription: Vec::new(),
            status: Some(status_text),
            message: TranscriptionLong {
                multipart,
                maybe_sidecar,
            },
        })
    }

    async fn update_status(&mut self, new_status: Option<&str>) -> HandlerResult {
        self.status = new_status.map(ToOwned::to_owned);
        self.reflow_message().await
    }

    async fn push_line(&mut self, line: Line) -> HandlerResult {
        self.transcription.push(line);
        self.reflow_message().await
    }

    async fn reflow_message(&mut self) -> HandlerResult {
        let long_msg = &mut self.message;
        if self.transcription.is_empty() {
            let status = self.status.as_deref().unwrap_or("");
            if let Some(WithSidecar { preview, .. }) = &mut long_msg.maybe_sidecar {
                let _ = preview.dispatch_edit_text(status);
            }
            let num_parts = long_msg.multipart.len();
            for (i, chunk) in long_msg.multipart.iter_mut().enumerate() {
                let msg_text = format!("[{}/{}] {}", i + 1, num_parts, status);
                let _ = chunk.dispatch_edit_text(msg_text);
            }

            // TODO: this is bad code style, use a match
            return Ok(());
        }

        let status = self.status.as_deref().unwrap_or("");
        let preview: Vec<_> = self
            .transcription
            .iter()
            .take_while(|line| line.end_secs < SHORT_MSG_CUTOFF_SECS)
            .map(utils::Line::to_telegram_line)
            .collect();
        let preview_is_truncated = self.transcription.len() > preview.len();
        let mut preview_text = format!("Preview:\n{}", preview.join("\n"));
        if preview_is_truncated {
            preview_text.push_str("\n...");
        }

        if let Some(WithSidecar { preview, .. }) = &mut long_msg.maybe_sidecar {
            let _ = preview.dispatch_edit_text(format!("{status}\n{preview_text}").trim());
        }

        let mut lines_iter = self.transcription.iter().peekable();
        let mut chunk_duration_limit = LONG_MSG_CHUNK_CUTOFF_SECS;
        let num_chunks = long_msg.multipart.len();
        for (i, chunk) in long_msg.multipart.iter_mut().enumerate() {
            let mut chunk_lines = Vec::new();
            while lines_iter
                .peek()
                .map_or(false, |line| line.end_secs < chunk_duration_limit)
            {
                let line = lines_iter.next().expect("Peeked");
                chunk_lines.push(line.to_telegram_line());
            }
            let _ = chunk.dispatch_edit_text(
                format!(
                    "[{}/{}] {}\n{}",
                    i + 1,
                    num_chunks,
                    status,
                    chunk_lines.join("\n")
                )
                .trim(),
            );
            chunk_duration_limit += LONG_MSG_CHUNK_CUTOFF_SECS;
        }

        Ok(())
    }

    pub async fn close(self) -> HandlerResult {
        let TranscriptionLong {
            multipart,
            maybe_sidecar,
        } = self.message;
        // TODO: closing all of these can be done concurrently
        for part in multipart {
            part.close().await?;
        }

        if let Some(WithSidecar { preview, .. }) = maybe_sidecar {
            preview.close().await?;
        }

        Ok(())
    }
}

struct TranscriptionLong {
    maybe_sidecar: Option<WithSidecar>,
    multipart: Vec<UpdateMsgHandle>,
}

struct WithSidecar {
    forwarded: Message,
    preview: UpdateMsgHandle,
}

async fn handle_message(bot: telegram::Bot, state: State, msg: types::Message) {
    let start = Instant::now();

    let on_err_reply_to = Message::new(bot.clone(), &msg);
    let res = try_handle_message(bot, state, msg.clone()).await;
    log::info!("Handling message {} took {:?}", msg.id, start.elapsed());
    if let Err(err) = res {
        match err {
            HandlerError::Ignore => { /* do as it says */ }
            HandlerError::UserError(_) => {
                let _ = on_err_reply_to.reply(err.to_string()).await;
            }
            _ => {
                log::warn!("Hit error: {err}");
                let msg = format!("The bot hit an error while handling this message.\n{err}");
                let _ = on_err_reply_to.reply(msg).await;
            }
        }
    }
}

async fn try_handle_message(
    bot: telegram::Bot,
    state: State,
    msg: types::Message,
) -> HandlerResult {
    // New message means a potentially more up-to-date view of the world
    state.db.update_metadata(&msg).await?;

    // Now that we have that saved let's see if we care about this message
    let RelevantMsg { meta, kind } = (&msg).try_into()?;

    // We only interact with users that we know
    let from = meta.from.clone();
    let sender = state.db.user(from.id).await.expect("Already added");
    if !sender.is_trusted().await {
        log::debug!("Ignoring non-trusted user: {from:?}");
        return Err(HandlerError::Ignore);
    }

    match kind {
        RelevantMsgKind::Command(com) => try_handle_command(bot, state, &meta, com, sender).await,
        RelevantMsgKind::Voice(voice) => {
            let trigger = sender.get_transcribe_trigger().await;
            if trigger == TranscribeTrigger::Always {
                try_handle_voice_message(bot, state, &meta, voice, sender).await?;
            }
            Ok(())
        }
    }
}

struct RelevantMsg {
    meta: RelevantMeta,
    kind: RelevantMsgKind,
}

impl TryFrom<&types::Message> for RelevantMsg {
    type Error = HandlerError;

    fn try_from(msg: &types::Message) -> Result<Self, Self::Error> {
        let id = msg.id;
        let chat_id = msg.chat.id;
        let from = msg.from().ok_or(HandlerError::Ignore)?.to_owned();
        let meta = RelevantMeta { id, chat_id, from };
        let kind = msg.try_into()?;

        Ok(Self { meta, kind })
    }
}

struct RelevantMeta {
    id: types::MessageId,
    chat_id: types::ChatId,
    from: types::User,
}

enum RelevantMsgKind {
    Command(RelevantCommand),
    Voice(types::Voice),
}

impl TryFrom<&types::Message> for RelevantMsgKind {
    type Error = HandlerError;

    fn try_from(msg: &types::Message) -> Result<Self, Self::Error> {
        if let Some(text) = msg.text() {
            let bot_name = BOT_NAME.get().unwrap();
            let com = match command::Command::parse(text, &bot_name) {
                // Probably just a regular text, so ignore
                Err(CommandParseError::UnknownCommand(_) | CommandParseError::WrongBotName(_)) => {
                    return Err(HandlerError::Ignore);
                }
                other => other,
            }?;
            let reply_to = msg.reply_to_message().map(Into::into);
            let relevant_com = RelevantCommand { com, reply_to };
            Ok(Self::Command(relevant_com))
        } else if let Some(voice) = msg.voice() {
            Ok(Self::Voice(voice.to_owned()))
        } else {
            Err(HandlerError::Ignore)
        }
    }
}

struct RelevantCommand {
    com: command::Command,
    reply_to: Option<RelevantParentMsg>,
}

struct RelevantParentMsg {
    // TODO: chat_id and id shouldn't be optional
    meta: Option<RelevantMeta>,
    voice: Option<types::Voice>,
}

impl From<&types::Message> for RelevantParentMsg {
    fn from(msg: &types::Message) -> Self {
        let id = msg.id;
        let chat_id = msg.chat.id;
        let meta = msg.from().map(|from| {
            let from = from.to_owned();
            RelevantMeta { id, chat_id, from }
        });
        let voice = msg.voice().map(ToOwned::to_owned);
        RelevantParentMsg { meta, voice }
    }
}

struct Reply {
    bot: telegram::Bot,
    chat_id: types::ChatId,
    msg_id: types::MessageId,
}

impl Reply {
    fn new(bot: telegram::Bot, chat_id: types::ChatId, msg_id: types::MessageId) -> Self {
        Self {
            bot,
            chat_id,
            msg_id,
        }
    }

    async fn send<S: Into<String>>(&self, text: S) -> HandlerResult<telegram::Message> {
        let msg = self
            .bot
            .send_message(self.chat_id, self.msg_id, text.into())
            .await?;
        Ok(msg)
    }
}

async fn try_handle_command(
    bot: telegram::Bot,
    state: State,
    meta: &RelevantMeta,
    RelevantCommand { com, reply_to }: RelevantCommand,
    sender: db::DbUser,
) -> HandlerResult {
    let reply = Reply::new(bot.clone(), meta.chat_id, meta.id);

    log::debug!("Running command: {com:?}");
    let db = &state.db;
    match com {
        command::Command::Vroom => {
            let start = tokio::time::Instant::now();
            let msg = reply.send("Checking...").await?;
            let elapsed = start.elapsed();
            msg.edit_text(format!(
                "Vroom vroom (sending took {elapsed:.01?})\n🐏🛻💨💨"
            ))
            .await?;
            Ok(())
        }
        command::Command::Transcribe => {
            // TODO: if it's a forward then check the trigger of the original author instead of the
            // author of the forwarder
            // Check the trigger of the sender
            let parent_msg = reply_to.ok_or(UserError::ReplyNotVoice)?;
            let parent_voice = parent_msg.voice.ok_or(UserError::ReplyNotVoice)?;
            let parent_meta = parent_msg.meta.ok_or(UserError::ReplyUnknownAuthor)?;
            let parent = &parent_meta.from;
            let parent = db
                .user(parent.id)
                .await
                .ok_or(UserError::ReplyUnknownAuthor)?;
            let trigger = parent.get_transcribe_trigger().await;
            match trigger {
                TranscribeTrigger::Never => Err(UserError::BadSummon(trigger).into()),
                TranscribeTrigger::SummonBySelf => {
                    if parent == sender {
                        try_handle_voice_message(bot, state, &parent_meta, parent_voice, sender)
                            .await
                    } else {
                        Err(UserError::BadSummon(trigger).into())
                    }
                }
                TranscribeTrigger::SummonByAny | TranscribeTrigger::Always => {
                    try_handle_voice_message(bot, state, &parent_meta, parent_voice, sender).await
                }
            }
        }
        command::Command::AttachSidecar(title) => {
            match *db.get_chat_ids_by_public_title(&title).await {
                [] => Err(UserError::NoChatTitled(title).into()),
                [sidecar] => {
                    db.attach_sidecar(meta.chat_id, sidecar).await?;
                    reply.send("Sidecar attached successfully 💪🐏").await?;
                    Ok(())
                }
                [_, _, ..] => Err(UserError::AmbiguousChatTitle.into()),
            }
        }
        command::Command::DetachSidecar => {
            db.detach_sidecar(meta.chat_id).await?;
            reply.send("Sidecar detached 🫨").await?;
            Ok(())
        }
        command::Command::GetTrigger => {
            let trigger = sender.get_transcribe_trigger().await;
            reply
                .send(&format!(
                    "Your trigger is currently set to: {}\n{}",
                    trigger,
                    trigger.desc()
                ))
                .await?;
            Ok(())
        }
        command::Command::SetTrigger(trigger) => {
            sender.set_transcribe_trigger(trigger).await?;
            reply.send("Trigger updated 🔫🐏").await?;
            Ok(())
        }
        command::Command::AddUser(name) => {
            let parent_msg = reply_to.ok_or(UserError::NotReply)?;
            let meta = parent_msg.meta.ok_or(UserError::ReplyUnknownAuthor)?;
            db.add_trusted_user(meta.from.id, name.clone()).await?;
            reply.send(&format!("Added user {name} 🫡")).await?;
            Ok(())
        }
    }
}

async fn try_handle_voice_message(
    bot: telegram::Bot,
    state: State,
    meta: &RelevantMeta,
    voice: types::Voice,
    sender: db::DbUser,
) -> HandlerResult {
    // TODO: Refactor to avoid `.unwrap()`
    let maybe_sidecar_id = match state.db.get_sidecar_attach(meta.chat_id).await.unwrap() {
        // Sidecar chats are ignored
        Some(a) if a.self_kind == db::SidecarKind::IsSidecar => {
            log::debug!("Ignoring sidecar chat voice message");
            return Err(HandlerError::Ignore);
        }
        Some(a) => Some(a.to),
        None => None,
    };

    let voice_file_id = &voice.file.id;
    let voice_msg_duration_secs = voice.duration;

    // Send our initial reply
    let mut bot_msg = Transcription::start(
        voice_msg_duration_secs,
        "Queued...",
        bot.clone(),
        state.send_msg_handle,
        meta.chat_id,
        meta.id,
        maybe_sidecar_id,
    )
    .await?;

    let job = state
        .transcriber_pool
        .submit_job(
            bot.clone(),
            voice_file_id.to_owned(),
            voice_msg_duration_secs,
        )
        .await;

    let download_started = job.await.map_err(HandlerError::worker_died)?;
    let _ = bot_msg.update_status(Some("Downloading...")).await;
    let downloading = download_started
        .await
        .map_err(HandlerError::worker_died)??;
    let mut transcribing = downloading.await.map_err(HandlerError::worker_died)??;
    let _ = bot_msg.update_status(Some("Transcribing...")).await;

    while let Some(line) = transcribing.next().await? {
        let _ = bot_msg.push_line(line).await;
    }

    bot_msg.update_status(None).await?;
    bot_msg.close().await?;

    Ok(())
}
