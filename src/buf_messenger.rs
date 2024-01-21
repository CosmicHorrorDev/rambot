//! Our _Play nice with the external API_ adapter
//!
//! Basically we could spend a lot of care throughout the whole app to avoid sending unneccessary
//! work to the telegram API, or we can slap this little bad boi on top of things to get buffering
//! (coalescing multiple edits together) and avoiding resending identical edits without having to
//! worry about it (well we worry about it here, but nowhere else)

use std::time::Duration;

use crate::{telegram, Error, Result};

use teloxide::types;
use tokio::{sync::mpsc, time};

#[derive(Clone)]
pub struct SendMsgHandle {
    req_tx: mpsc::UnboundedSender<SendReq>,
}

impl SendMsgHandle {
    pub fn dispatch_send_msg<S: Into<String>>(
        &self,
        chat_id: types::ChatId,
        reply_to: types::MessageId,
        text: S,
    ) -> Result<UpdateMsgHandle> {
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        self.req_tx
            .send(SendReq {
                chat_id,
                reply_to,
                text: text.into(),
                req_rx,
                resp_tx,
            })
            .map_err(|_| Error::SendMsgWorkerDied)?;
        Ok(UpdateMsgHandle { req_tx, resp_rx })
    }
}

// NOTE: Intentionally not `Clone` to ensure that this is a unique handle to the message (otherwise
// `.flush()`ing can break)
pub struct UpdateMsgHandle {
    req_tx: mpsc::UnboundedSender<UpdateReq>,
    resp_rx: mpsc::UnboundedReceiver<MsgResp>,
}

impl UpdateMsgHandle {
    pub fn dispatch_edit_text<S: Into<String>>(&mut self, text: S) -> Result<()> {
        let text = text.into();
        self.req_tx
            .send(UpdateReq::Edit(text))
            .map_err(|_| Error::UpdateMsgWorkerDied)?;
        while let Ok(resp) = self.resp_rx.try_recv() {
            match resp {
                MsgResp::Flush => unreachable!("Should never be seen outside a `.flush()` call"),
                MsgResp::Error(e) => return Err(e),
            }
        }

        Ok(())
    }

    // NOTE: calling `.flush()` is the only source of `Flush`es getting sent through. You MUST
    // ensure that we always consume the `Flush` that we sent through even when we're getting
    // errors in the process
    pub async fn flush(&mut self) -> Result<()> {
        // Pass a flush through and make sure we get it back on the other side
        self.req_tx
            .send(UpdateReq::Flush)
            .map_err(|_| Error::UpdateMsgWorkerDied)?;

        let mut delayed_error = None;
        loop {
            match self.resp_rx.recv().await {
                // An error returned from the worker is more interesting than (and could be the
                // cause of) the worker dying. Return that when available
                None => break Err(delayed_error.unwrap_or(Error::UpdateMsgWorkerDied)),
                Some(MsgResp::Error(e)) => {
                    log::info!("Captured delayed error: {e}");
                    delayed_error = Some(e)
                }
                Some(MsgResp::Flush) => break Ok(()),
            }
        }
    }

    pub async fn close(mut self) -> Result<()> {
        self.flush().await?;
        Ok(())
    }
}

struct SendReq {
    chat_id: types::ChatId,
    reply_to: types::MessageId,
    text: String,
    req_rx: mpsc::UnboundedReceiver<UpdateReq>,
    resp_tx: mpsc::UnboundedSender<MsgResp>,
}

enum UpdateReq {
    Edit(String),
    Flush,
}

enum MsgResp {
    Flush,
    Error(Error),
}

async fn run_send_worker(mut rx: mpsc::UnboundedReceiver<SendReq>, bot: telegram::Bot) {
    while let Some(req) = rx.recv().await {
        let SendReq {
            chat_id,
            reply_to,
            text,
            req_rx,
            resp_tx,
        } = req;
        let msg = match bot.send_message(chat_id, reply_to, text.clone()).await {
            Ok(msg) => msg,
            Err(e) => {
                let _ = resp_tx.send(MsgResp::Error(e.into()));
                continue;
            }
        };

        // Detach a worker for handling message updates
        let _ = tokio::task::spawn(run_update_worker(req_rx, resp_tx, msg, text));
    }
}

async fn run_update_worker(
    mut rx: mpsc::UnboundedReceiver<UpdateReq>,
    tx: mpsc::UnboundedSender<MsgResp>,
    msg: telegram::Message,
    mut current_text: String,
) {
    while let Some(req) = rx.recv().await {
        match req {
            UpdateReq::Flush => _ = tx.send(MsgResp::Flush),
            UpdateReq::Edit(mut text) => {
                let slight_delay = time::Instant::now() + Duration::from_millis(200);
                let mut flush_after = false;

                // Instead of editing immediately we wait for a bit of time to coalesce any more
                // edits together (breaking early if we get a flush)
                loop {
                    // NOTE: Receiving on a channel is cancellation safe, so no work gets lost
                    tokio::select! {
                        _ = time::sleep_until(slight_delay) => break,
                        maybe_req = rx.recv() => {
                            let Some(req) = maybe_req else {
                                break;
                            };
                            match req {
                                UpdateReq::Edit(fresher_text) => {
                                    log::trace!("Coalescing edits together");
                                    text = fresher_text;
                                },
                                UpdateReq::Flush => {
                                    flush_after = true;
                                    break;
                                }
                            }
                        }
                    };
                }

                if text == current_text {
                    log::trace!("Skipping duplicate message text");
                } else {
                    current_text = text.clone();
                    if let Err(e) = msg.edit_text(text).await {
                        let _ = tx.send(MsgResp::Error(e.into()));
                    }
                }

                if flush_after {
                    let _ = tx.send(MsgResp::Flush);
                }
            }
        }
    }
}

// TODO: use a once_<something> to get this to only ever run once
pub fn init(bot: telegram::Bot) -> SendMsgHandle {
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let _ = tokio::task::spawn(run_send_worker(req_rx, bot));

    SendMsgHandle { req_tx }
}
