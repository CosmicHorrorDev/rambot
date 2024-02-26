//! Telegram has a big API surface area. These are the parts we care about

use std::path::Path;

use crate::HandlerResult;

use teloxide::{net::Download, payloads, requests::Requester, types};

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

    pub async fn get_me(&self) -> Result<types::Me, teloxide::RequestError> {
        log::debug!("Getting me");
        self.0.get_me().await
    }

    pub async fn set_my_commands(
        &self,
        commands: Vec<types::BotCommand>,
    ) -> Result<(), teloxide::RequestError> {
        log::debug!("Setting telegram commands");
        self.0.set_my_commands(commands).await?;
        Ok(())
    }

    pub async fn send_message<S: Into<String>>(
        &self,
        chat_id: types::ChatId,
        reply_to: types::MessageId,
        text: S,
    ) -> HandlerResult<Message> {
        let text = text.into();
        log::debug!("Sending reply to message {reply_to} text:\n{text}");
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

    pub async fn download_file(&self, output_path: &Path, file_id: String) -> HandlerResult {
        log::debug!("Downloading file {} to {}", file_id, output_path.display());
        let file_meta = self.0.get_file(file_id).await?;
        let mut file = tokio::fs::File::create(output_path).await?;
        self.0.download_file(&file_meta.path, &mut file).await?;
        Ok(())
    }

    pub async fn forward_message(
        &self,
        to_chat_id: types::ChatId,
        from_chat_id: types::ChatId,
        msg_id: types::MessageId,
    ) -> HandlerResult<Message> {
        log::debug!("Forwarding message {msg_id} from {from_chat_id} to {to_chat_id}");
        let msg = self
            .0
            .forward_message(to_chat_id, from_chat_id, msg_id)
            .await?;
        Ok(Message::new(self.clone(), &msg))
    }
}

#[derive(Clone)]
pub struct Message {
    bot: teloxide::Bot,
    msg_id: types::MessageId,
    chat_id: types::ChatId,
}

impl Message {
    pub fn new(bot: Bot, msg: &types::Message) -> Self {
        Self {
            bot: bot.0,
            msg_id: msg.id,
            chat_id: msg.chat.id,
        }
    }

    pub fn id(&self) -> types::MessageId {
        self.msg_id
    }

    pub async fn edit_text<S: Into<String>>(&self, text: S) -> HandlerResult {
        let text = text.into();
        log::debug!(
            "Editing message {} len {} snippet:\n{}",
            self.msg_id,
            text.len(),
            if text.chars().count() > 100 {
                text.chars().take(100 - 3).chain("...".chars()).collect()
            } else {
                text.to_owned()
            }
        );
        self.bot
            .edit_message_text(self.chat_id, self.msg_id, text)
            .await?;
        Ok(())
    }

    pub async fn reply<S: Into<String>>(&self, text: S) -> HandlerResult {
        let bot_ext = Bot::from(self.bot.clone());
        bot_ext
            .send_message(self.chat_id, self.msg_id, text.into())
            .await?;
        Ok(())
    }
}
