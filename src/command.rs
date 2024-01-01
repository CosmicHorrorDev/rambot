use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
pub enum Command {
    #[command(description = "Vroom vroom mother trucker ;V")]
    Help,
    #[command(description = "Transcribe a message")]
    Tx,
}
