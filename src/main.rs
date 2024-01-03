// TODO: Overall reorganization:
// - Add a little scheduler before the transcriber (download file into a temp dir to hand to the
//   transcriber. Auto cleans up the file and speeds up transcriptions by pre-downloading). This
//   can be represnted by the job holding a temp dir
// - Refactor the transcribers into an explicit fast and slow transcriber. The fast one will prefer
//   short jobs when possible while the slow can chew through longer jobs

mod command;
mod error;
mod telegram;
mod transcriber;
mod utils;

pub use error::{Error, Result};

use telegram::Message;
use teloxide::{
    dispatching::{Dispatcher, UpdateFilterExt},
    types,
    utils::command::BotCommands,
};

#[tokio::main]
async fn main() {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!(".env error: {e}");
    }
    pretty_env_logger::init();
    log::info!("Logging started");
    let bot = telegram::Bot::from_env();

    bot.set_my_commands(command::Command::bot_commands())
        .await
        .unwrap();
    let handler = types::Update::filter_message().endpoint(
        |bot: teloxide::Bot, transcribers: transcriber::Pool, msg: types::Message| async move {
            handle_message(bot.into(), transcribers, msg).await;
            Ok::<(), ()>(())
        },
    );

    let transcribers = transcriber::Pool::spawn(2).await;
    Dispatcher::builder(bot.0, handler)
        // Default distribution_function runs each chat sequentially. Run everything
        // concurrently instead. Embrace the async
        .distribution_function::<()>(|_| None)
        .dependencies(teloxide::dptree::deps![transcribers])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

async fn handle_message(bot: telegram::Bot, transcribers: transcriber::Pool, msg: types::Message) {
    let on_err_reply_to = Message::new(bot.clone(), &msg);
    if let Err(err) = try_handle_message(bot, transcribers, msg).await {
        log::warn!("Hit error: {err}");
        let _ = on_err_reply_to
            .reply(&format!(
                "The bot hit an error while handling this message.\n{err}"
            ))
            .await;
    }
}

// TODO: parse out commands and use that to determine our action once we get useful commands setup
async fn try_handle_message(
    bot: telegram::Bot,
    transcribers: transcriber::Pool,
    msg: types::Message,
) -> Result<()> {
    let types::MessageKind::Common(common) = &msg.kind else {
        return Ok(());
    };
    let types::MediaKind::Voice(voice) = &common.media_kind else {
        return Ok(());
    };
    let voice_file_id = &voice.voice.file.id;
    let voice_msg_duration_secs = voice.voice.duration;

    // Send our initial reply
    let bot_msg = bot.send_message(msg.chat.id, msg.id, "Queued...").await?;

    let job = transcribers
        .submit_job(
            bot.clone(),
            voice_file_id.to_owned(),
            voice_msg_duration_secs,
        )
        .await;
    let download_started = job.await.map_err(Error::worker_died)?;
    let _ = bot_msg.edit_text("Downloading...").await;
    let downloading = download_started.await.map_err(Error::worker_died)??;
    // TODO: what's the point of this state?
    let mut transcribing = downloading.await.map_err(Error::worker_died)??;
    let _ = bot_msg.edit_text("Transcribing...").await;

    let mut last_transcription = None;
    while let Some(transcription) = transcribing.next().await? {
        last_transcription = Some(transcription.clone());
        let telegram_formatted = utils::srt_like_to_telegram_ts(&transcription);
        let formatted_resp = format!("In progress...\n{telegram_formatted}\n[...]");
        let _ = bot_msg.edit_text(&formatted_resp).await;
    }

    if let Some(final_transcription) = last_transcription {
        let telegram_formatted = utils::srt_like_to_telegram_ts(&final_transcription);
        let _ = bot_msg.edit_text(&telegram_formatted).await;
    }

    Ok(())
}
