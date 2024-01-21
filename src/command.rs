use crate::db;

use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    #[command(description = "Vroom vroom mother trucker ;V")]
    Vroom,
    #[command(description = "Attach a sidecar for longer voice messages")]
    AttachSidecar(String),
    #[command(description = "Detach the sidecar for/from this chat")]
    DetachSidecar,
    #[command(description = "Get your current transcription trigger")]
    GetTrigger,
    #[command(description = "Set your user's transcription trigger")]
    SetTrigger(db::TranscribeTrigger),
    #[command(description = "Add a user for the bot to recognize")]
    AddUser(String),
}
