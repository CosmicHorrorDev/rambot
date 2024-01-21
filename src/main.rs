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

use std::{env, sync::Arc, time::Instant};

use buf_messenger::UpdateMsgHandle;
pub use error::{Error, Result};

use telegram::Message;
use teloxide::{
    dispatching::{Dispatcher, UpdateFilterExt},
    types,
    utils::command::{BotCommands, ParseError as CommandParseError},
};

#[derive(Clone)]
struct State {
    transcriber_pool: transcriber::Pool,
    // TODO: move this into `telegram::Bot`
    send_msg_handle: buf_messenger::SendMsgHandle,
    db: db::Db,
    // TODO: move this into `telegram::Bot`
    bot_name: Arc<str>,
}

#[tokio::main]
async fn main() -> Result {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!(".env error: {e}");
    }
    pretty_env_logger::init();
    log::info!("Logging started");

    let db = db::Db::load().await?;

    let bot = telegram::Bot::from_env();
    bot.set_my_commands(command::Command::bot_commands())
        .await
        .unwrap();
    let handler = types::Update::filter_message().endpoint(
        |bot: teloxide::Bot, state: State, msg: types::Message| async move {
            handle_message(bot.into(), state, msg).await;
            Ok::<(), ()>(())
        },
    );

    let transcribers = transcriber::Pool::spawn(2).await;
    let send_msg_handle = buf_messenger::init(bot.clone());
    let bot_name = env::var("BOT_NAME").map_err(Error::InvalidBotName)?.into();
    let state = State {
        transcriber_pool: transcribers,
        send_msg_handle,
        db,
        bot_name,
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
const LONG_MSG_CHUNK_CUTOFF_SECS: u32 = 300;

struct Transcription {
    transcription: Option<String>,
    status: Option<String>,
    message: TranscriptionMessage,
}

enum TranscriptionMessage {
    Short(UpdateMsgHandle),
    Long(TranscriptionLong),
}

impl Transcription {
    async fn start<S: Into<String>>(
        duration_secs: u32,
        status_text: S,
        bot: telegram::Bot,
        send_msg_handle: buf_messenger::SendMsgHandle,
        chat_id: types::ChatId,
        msg_id: types::MessageId,
        sidecar_id: types::ChatId,
    ) -> Result<Self> {
        let status_text = status_text.into();
        if duration_secs <= SHORT_MSG_CUTOFF_SECS {
            let short_msg = send_msg_handle.dispatch_send_msg(chat_id, msg_id, &status_text)?;
            Ok(Self {
                transcription: None,
                status: Some(status_text),
                message: TranscriptionMessage::Short(short_msg),
            })
        } else {
            // Oh lawd he ramblin
            let forwarded = bot.forward_message(sidecar_id, chat_id, msg_id).await?;
            let mut chunks = Vec::new();
            let num_chunks = 1 + duration_secs / LONG_MSG_CHUNK_CUTOFF_SECS;
            for index in 0..num_chunks {
                let chunk = send_msg_handle.dispatch_send_msg(
                    sidecar_id,
                    forwarded.id(),
                    &format!("[{}/{}] {}", index + 1, num_chunks, status_text),
                )?;
                chunks.push(chunk);
            }
            let preview = send_msg_handle.dispatch_send_msg(chat_id, msg_id, &status_text)?;

            Ok(Self {
                transcription: None,
                status: Some(status_text),
                message: TranscriptionMessage::Long(TranscriptionLong {
                    preview,
                    sidecar: Sidecar { forwarded, chunks },
                }),
            })
        }
    }

    async fn update_status(&mut self, new_status: Option<&str>) -> Result {
        self.status = new_status.map(ToOwned::to_owned);
        self.reflow_message().await
    }

    async fn update_transcription(&mut self, new_transcription: Option<&str>) -> Result {
        self.transcription = new_transcription.map(ToOwned::to_owned);
        self.reflow_message().await
    }

    async fn reflow_message(&mut self) -> Result {
        match &mut self.message {
            TranscriptionMessage::Short(msg) => {
                let transcription = self.transcription.as_deref().map(|transcription| {
                    transcription
                        .lines()
                        .filter_map(|line| {
                            let line = utils::Line::new(line)?;
                            Some(line.into_telegram_line())
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                let msg_text = match (self.status.as_deref(), transcription.as_deref()) {
                    (Some(status), Some(transcription)) => format!("{status}\n{transcription}"),
                    (Some(lone), None) | (None, Some(lone)) => lone.to_owned(),
                    (None, None) => String::new(),
                };
                msg.dispatch_edit_text(msg_text)?;
            }
            TranscriptionMessage::Long(long_msg) => {
                if self.transcription.is_none() {
                    let status = self.status.as_deref().unwrap_or("");
                    let _ = long_msg.preview.dispatch_edit_text(status);
                    let num_chunks = long_msg.sidecar.chunks.len();
                    for (i, chunk) in long_msg.sidecar.chunks.iter_mut().enumerate() {
                        let msg_text = format!("[{}/{}] {}", i + 1, num_chunks, status);
                        let _ = chunk.dispatch_edit_text(msg_text);
                    }

                    // TODO: this is bad code style, use a match
                    return Ok(());
                }

                let status = self.status.as_deref().unwrap_or("");
                let lines = self
                    .transcription
                    .as_deref()
                    .unwrap()
                    .lines()
                    .filter_map(utils::Line::new)
                    .collect::<Vec<_>>();
                let preview: Vec<_> = lines
                    .iter()
                    .cloned()
                    .take_while(|line| line.end_secs < SHORT_MSG_CUTOFF_SECS)
                    .map(utils::Line::into_telegram_line)
                    .collect();
                let preview_is_truncated = lines.len() > preview.len();
                let mut preview_text = format!("Preview:\n{}", preview.join("\n"));
                if preview_is_truncated {
                    preview_text.push_str("\n...");
                }

                let _ = long_msg
                    .preview
                    .dispatch_edit_text(format!("{status}\n{preview_text}").trim());

                let mut lines_iter = lines.iter().peekable();
                let mut chunk_duration_limit = LONG_MSG_CHUNK_CUTOFF_SECS;
                let num_chunks = long_msg.sidecar.chunks.len();
                for (i, chunk) in long_msg.sidecar.chunks.iter_mut().enumerate() {
                    let mut chunk_lines = Vec::new();
                    while lines_iter
                        .peek()
                        .map_or(false, |line| line.end_secs < chunk_duration_limit)
                    {
                        let line = lines_iter.next().expect("Peeked");
                        chunk_lines.push(line.to_owned().into_telegram_line());
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
            }
        }

        Ok(())
    }

    pub async fn close(self) -> Result {
        match self.message {
            TranscriptionMessage::Short(msg_handle) => msg_handle.close().await,
            TranscriptionMessage::Long(TranscriptionLong {
                preview,
                sidecar: Sidecar { chunks, .. },
            }) => {
                // TODO: closing all of these can be done concurrently
                for chunk in chunks {
                    chunk.close().await?;
                }
                preview.close().await
            }
        }
    }
}

struct TranscriptionLong {
    preview: UpdateMsgHandle,
    sidecar: Sidecar,
}

struct Sidecar {
    forwarded: Message,
    chunks: Vec<UpdateMsgHandle>,
}

async fn handle_message(bot: telegram::Bot, state: State, msg: types::Message) {
    let start = Instant::now();

    let on_err_reply_to = Message::new(bot.clone(), &msg);
    if let Err(err) = try_handle_message(bot, state, msg.clone()).await {
        log::warn!("Hit error: {err}");
        let _ = on_err_reply_to
            .reply(&format!(
                "The bot hit an error while handling this message.\n{err}"
            ))
            .await;
    }

    log::info!("Handling message {} took {:?}", msg.id, start.elapsed());
}

// TODO: parse out commands and use that to determine our action once we get useful commands setup
async fn try_handle_message(bot: telegram::Bot, state: State, msg: types::Message) -> Result {
    // New message means a potentially more up-to-date view of the world
    state.db.update_metadata(&msg).await?;

    // We only interact with common messages
    let types::MessageKind::Common(
        common @ types::MessageCommon {
            from: Some(from), ..
        },
    ) = &msg.kind
    else {
        log::debug!("Ignoring non-common or authorless message");
        return Ok(());
    };

    // We only interact with users that we know
    let sender = state.db.user(from.id).await.expect("Already added");
    if !sender.is_trusted().await {
        log::debug!("Ignoring non-trusted user: {from:?}");
        return Ok(());
    }

    match &common.media_kind {
        types::MediaKind::Text(text) => {
            try_handle_text_message(bot, state, &msg, text, sender).await
        }
        types::MediaKind::Voice(voice) => {
            try_handle_voice_message(bot, state, &msg, voice, sender).await
        }
        _ => Ok(()),
    }
}

async fn try_handle_text_message(
    bot: telegram::Bot,
    state: State,
    msg: &types::Message,
    text: &types::MediaText,
    sender: db::DbUser,
) -> Result {
    // Try to parse out a valid command
    let com = match command::Command::parse(&text.text, &state.bot_name) {
        // Probably just a regular text, so ignore
        Err(CommandParseError::UnknownCommand(_)) => return Ok(()),
        other => other,
    }?;

    log::debug!("Running command: {com:?}");
    let db = &state.db;
    match com {
        command::Command::Vroom => {
            bot.send_message(msg.chat.id, msg.id, "Vroom vroom\n🐏🛻💨💨")
                .await?;
        }
        command::Command::AttachSidecar(title) => {
            match db.get_chat_ids_by_public_title(&title).await.as_slice() {
                [] => {
                    bot.send_message(
                        msg.chat.id,
                        msg.id,
                        format!("No chat found called: {title:?}"),
                    )
                    .await?;
                }
                [sidecar] => {
                    db.attach_sidecar(msg.chat.id, *sidecar).await?;
                    bot.send_message(msg.chat.id, msg.id, "Sidecar attached successfully 💪🐏")
                        .await?;
                }
                [_, _, ..] => {
                    bot.send_message(
                        msg.chat.id,
                        msg.id,
                        "Ambiguous request. Multiple chats were found with that title",
                    )
                    .await?;
                }
            }
        }
        command::Command::DetachSidecar => {
            db.detach_sidecar(msg.chat.id).await?;
            bot.send_message(msg.chat.id, msg.id, "Sidecar detached 🫨")
                .await?;
        }
        command::Command::GetTrigger => {
            let trigger = sender.get_transcribe_trigger().await;
            bot.send_message(
                msg.chat.id,
                msg.id,
                format!(
                    "Your trigger is currently set to: {}\n{}",
                    trigger.as_str(),
                    trigger.desc()
                ),
            )
            .await?;
        }
        command::Command::SetTrigger(trigger) => {
            sender.set_transcribe_trigger(trigger).await?;
            bot.send_message(msg.chat.id, msg.id, "Trigger updated 🔫🐏")
                .await?;
        }
        command::Command::AddUser(name) => {
            let types::MessageKind::Common(common) = &msg.kind else {
                // TODO: refactor to avoid having vv
                unreachable!("This was already checked earlier");
            };

            let Some(parent_msg) = &common.reply_to_message else {
                bot.send_message(
                    msg.chat.id,
                    msg.id,
                    "Your message should be a reply to a message of the user getting added",
                )
                .await?;
                return Ok(());
            };

            let maybe_parent_id = if let types::MessageKind::Common(common) = &parent_msg.kind {
                common.from.as_ref().map(|user| user.id)
            } else {
                None
            };

            match maybe_parent_id {
                None => {
                    bot.send_message(
                        msg.chat.id,
                        msg.id,
                        "I couldn't determine the author of the message you're replying to",
                    )
                    .await?;
                }
                Some(user_id) => {
                    db.add_trusted_user(user_id, name.clone()).await?;
                    bot.send_message(msg.chat.id, msg.id, format!("Added user {name} 🫡"))
                        .await?;
                }
            }
        }
    }

    Ok(())
}

async fn try_handle_voice_message(
    bot: telegram::Bot,
    state: State,
    msg: &types::Message,
    voice: &types::MediaVoice,
    sender: db::DbUser,
) -> Result {
    // TODO: Refactor to avoid `.unwrap()`
    let maybe_sidecar_id = match state.db.get_sidecar_attach(msg.chat.id).await.unwrap() {
        // Sidecar chats are ignored
        Some(a) if a.self_kind == db::SidecarKind::IsSidecar => {
            log::debug!("Ignoring sidecar chat voice message");
            return Ok(());
        }
        Some(a) => Some(a.to),
        None => None,
    };

    let Some(sidecar_id) = maybe_sidecar_id else {
        log::warn!("Actually implement this");
        return Ok(());
    };

    let voice_file_id = &voice.voice.file.id;
    let voice_msg_duration_secs = voice.voice.duration;

    // Send our initial reply
    let mut bot_msg = Transcription::start(
        voice_msg_duration_secs,
        "Queued...",
        bot.clone(),
        state.send_msg_handle,
        msg.chat.id,
        msg.id,
        sidecar_id,
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

    let download_started = job.await.map_err(Error::worker_died)?;
    let _ = bot_msg.update_status(Some("Downloading...")).await;
    let downloading = download_started.await.map_err(Error::worker_died)??;
    let mut transcribing = downloading.await.map_err(Error::worker_died)??;
    let _ = bot_msg.update_status(Some("Transcribing...")).await;

    while let Some(transcription) = transcribing.next().await? {
        let _ = bot_msg.update_transcription(Some(&transcription)).await;
    }

    bot_msg.update_status(None).await?;
    bot_msg.close().await?;

    Ok(())
}
