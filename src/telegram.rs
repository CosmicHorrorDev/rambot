//! Telegram has a big API surface area. These are the parts we care about

use std::path::Path;

use teloxide::{net::Download, payloads, requests::Requester, types};
use thiserror::Error as ThisError;

#[derive(Clone)]
pub struct Bot(pub teloxide::Bot);

impl From<teloxide::Bot> for Bot {
    fn from(bot: teloxide::Bot) -> Self {
        Self(bot)
    }
}

impl Bot {
    pub fn from_env() -> Self {
        Self(teloxide::Bot::from_env())
    }

    pub async fn set_my_commands(&self, commands: Vec<types::BotCommand>) -> Result<()> {
        self.0.set_my_commands(commands).await?;
        Ok(())
    }

    pub async fn send_message(
        &self,
        chat_id: types::ChatId,
        reply_to: types::MessageId,
        text: &str,
    ) -> Result<Message> {
        let msg = <teloxide::Bot as Requester>::SendMessage::new(
            self.0.clone(),
            payloads::SendMessage {
                reply_to_message_id: Some(reply_to),
                ..payloads::SendMessage::new(chat_id.clone(), text)
            },
        )
        .await?;

        Ok(Message {
            bot: self.0.clone(),
            msg_id: msg.id,
            chat_id,
        })
    }

    pub async fn download_file(&self, output_path: &Path, file_id: String) -> Result<()> {
        let file_meta = self.0.get_file(file_id).await?;
        let mut file = tokio::fs::File::create(output_path).await?;
        self.0.download_file(&file_meta.path, &mut file).await?;
        Ok(())
    }
}

pub struct Message {
    bot: teloxide::Bot,
    msg_id: types::MessageId,
    chat_id: types::ChatId,
}

impl Message {
    pub async fn edit_text(&self, text: &str) -> Result<()> {
        self.bot
            .edit_message_text(self.chat_id, self.msg_id, text)
            .await?;
        Ok(())
    }
}

// TODO: just make a crate-wide error
type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Telegram API download error: {0}")]
    Download(#[from] teloxide::DownloadError),
    #[error("Telegram API request error: {0}")]
    Request(#[from] teloxide::RequestError),
}
