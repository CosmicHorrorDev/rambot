// TODO: Overall reorganization:
// - Add a little scheduler before the transcriber (download file into a temp dir to hand to the
//   transcriber. Auto cleans up the file and speeds up transcriptions by pre-downloading). This
//   can be represnted by the job holding a temp dir
// - Refactor the transcribers into an explicit fast and slow transcriber. The fast one will prefer
//   short jobs when possible while the slow can chew through longer jobs

mod command;
mod telegram;
mod transcriber;
mod utils;

use teloxide::{dispatching::UpdateFilterExt, types, utils::command::BotCommands};

#[tokio::main]
async fn main() {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!(".env error: {e}");
    }
    pretty_env_logger::init();
    let bot = telegram::Bot::from_env();

    bot.set_my_commands(command::Command::bot_commands())
        .await
        .unwrap();
    let handler = types::Update::filter_message().endpoint(
        |bot: teloxide::Bot, transcribers: transcriber::Pool, msg: types::Message| async move {
            handle_message(bot.into(), transcribers, msg).await
        },
    );

    let transcribers = transcriber::Pool::spawn(2).await;
    teloxide::dispatching::Dispatcher::builder(bot.0, handler)
        // Default distribution_function runs each chat sequentially. Run everything
        // concurrently instead. Embrace the async
        .distribution_function::<()>(|_| None)
        .dependencies(teloxide::dptree::deps![transcribers])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

// TODO: parse out commands and use that to determine our action once we get useful commands setup
async fn handle_message(
    bot: telegram::Bot,
    transcribers: transcriber::Pool,
    msg: types::Message,
) -> Result<(), teloxide::RequestError> {
    let types::MessageKind::Common(common) = &msg.kind else {
        return Ok(());
    };
    let types::MediaKind::Voice(voice) = &common.media_kind else {
        return Ok(());
    };
    let voice_file_id = &voice.voice.file.id;

    // Send our initial reply
    let bot_msg = bot
        .send_message(msg.chat.id, msg.id, "Queued...")
        .await
        .unwrap();

    let mut job_handle = transcribers
        .submit_job(bot.clone(), voice_file_id.to_owned())
        .await;
    let mut current_transcription = String::new();
    // TODO: skip useless updates by eagerly receiving more events to start
    while let Some(update) = job_handle.recv().await {
        match update {
            transcriber::Update::Downloading => {
                let _ = bot_msg.edit_text("Downloading...").await;
            }
            transcriber::Update::StartedTranscription => {
                let _ = bot_msg.edit_text("Transcribing...").await;
            }
            transcriber::Update::CurrentTranscription(transcription) => {
                let telegram_formatted = transcription
                    .lines()
                    .filter_map(utils::srt_like_to_telegram_ts_line)
                    .collect::<Vec<_>>()
                    .join("\n");
                current_transcription = telegram_formatted.clone();
                let formatted_resp = format!("In progress...\n{telegram_formatted}\n[...]");
                // TODO: handle this more gracefully. It errors when the message wasn't
                // actually modified
                let _ = bot_msg.edit_text(&formatted_resp).await;
            }
            transcriber::Update::Finished(full) => {
                let telegram_formatted = full
                    .lines()
                    .filter_map(utils::srt_like_to_telegram_ts_line)
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = bot_msg.edit_text(&telegram_formatted).await;
            }
            transcriber::Update::ErroredOut => {
                let _ = bot_msg.edit_text("Errored out >:V").await;
            }
        }
    }
    let _ = bot_msg.edit_text(&current_transcription).await;

    Ok(())
}
