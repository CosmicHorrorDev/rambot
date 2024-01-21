//! Home to a state machine and its interruptible handle representing the transcription process
//!
//! All the *Fut and non-*Fut values here represent the two sides of the transcription process'
//! state machine where the *Fut side automatically emits updates to the non-*Fut side that expand
//! out to follow the state machine's flow

use std::{io::Read, path::Path, process::Stdio, sync::Arc};

use crate::{telegram::Bot, Error, Result};

use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot, Mutex},
};

// TODO: provide some kind of constructor
// TODO: wrap non-fut so that we can expose a meaningful error directly?
#[must_use]
pub struct JobFut {
    pub next: oneshot::Sender<DownloadStarted>,
    pub meta: JobMeta,
}

pub struct JobMeta {
    pub bot: Bot,
    pub voice_file_id: String,
    pub voice_msg_duration_secs: u32,
}

impl JobFut {
    pub fn start_download(self) -> Option<DownloadStartedFut> {
        let Self { next, meta } = self;
        let (tx, rx) = oneshot::channel();
        next.send(rx).ok()?;
        Some(DownloadStartedFut { next: tx, meta })
    }
}

pub type DownloadStarted = oneshot::Receiver<Result<Downloading>>;

#[must_use]
pub struct DownloadStartedFut {
    next: oneshot::Sender<Result<Downloading>>,
    meta: JobMeta,
}

impl DownloadStartedFut {
    pub async fn finish_download(self, voice_msg_path: String) -> Option<DownloadingFut> {
        let Self {
            next,
            meta: JobMeta {
                bot, voice_file_id, ..
            },
        } = self;
        let (tx, rx) = oneshot::channel();

        if let Err(e) = bot
            .download_file(Path::new(&voice_msg_path), voice_file_id)
            .await
        {
            next.send(Err(e.into())).ok()?;
            None
        } else {
            next.send(Ok(rx)).ok()?;
            Some(DownloadingFut(tx))
        }
    }
}

// TODO: rename all `Downloading` -> `Downloaded`
pub type Downloading = oneshot::Receiver<Result<Transcribing>>;

#[must_use]
pub struct DownloadingFut(oneshot::Sender<Result<Transcribing>>);

impl DownloadingFut {
    pub fn start_transcription(self) -> Option<TranscribingFut> {
        let (msg_handle, transcriber_handle) = mpsc::channel(16);
        let shared_transcription = Arc::default();
        self.0
            .send(Ok(Transcribing {
                finished: false,
                transcriber_handle,
                shared_transcription: Arc::clone(&shared_transcription),
            }))
            .ok()?;
        Some(TranscribingFut {
            msg_handle,
            shared_transcription,
        })
    }
}

#[must_use]
pub struct Transcribing {
    // TODO: switch this over to a tokio::sync::watch channel
    finished: bool,
    transcriber_handle: mpsc::Receiver<Result<Update>>,
    shared_transcription: Arc<Mutex<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Update {
    InProgress,
    Finished,
}

impl Transcribing {
    pub async fn next(&mut self) -> Result<Option<String>> {
        if self.finished {
            return Ok(None);
        }

        let mut maybe_update = self
            .transcriber_handle
            .recv()
            .await
            .ok_or(Error::WorkerDied)?;
        // Skip any queued in-progress updates
        while maybe_update
            .as_ref()
            .map_or(false, |&update| update == Update::InProgress)
        {
            match self.transcriber_handle.try_recv() {
                Ok(next) => maybe_update = next,
                Err(_) => break,
            }
        }

        match maybe_update {
            Ok(Update::InProgress) => {
                let transcription = self.shared_transcription.lock().await.to_owned();
                Ok(Some(transcription))
            }
            Ok(Update::Finished) => {
                self.finished = true;
                let transcription = self.shared_transcription.lock().await.to_owned();
                Ok(Some(transcription))
            }
            Err(err) => Err(err),
        }
    }
}

#[derive(Clone)]
pub struct TranscribingFut {
    msg_handle: mpsc::Sender<Result<Update>>,
    shared_transcription: Arc<Mutex<String>>,
}

impl TranscribingFut {
    pub async fn finish_transcription(self, voice_msg_path: String, duration: u32) -> Option<()> {
        // Do slower transcriptions on longer messages just while this is running on weaker
        // hardware
        // TODO: remove after we get setup on a GPU
        let num_threads = if duration > 10 * 60 { "2" } else { "4" };

        let fut = self.clone();
        let res = match tokio::task::spawn_blocking(move || {
            run_sync_process(voice_msg_path, num_threads, fut)
        })
        .await
        {
            Ok(Ok(())) => Ok(Update::Finished),
            Ok(Err(err)) => {
                log::warn!("Sync process wrapper returned an error: {err}");
                Err(err)
            }
            Err(err) => {
                log::warn!("Sync process wrapper died unexpectedly: {err}");
                Err(Error::WorkerDied)
            }
        };

        self.msg_handle.send(res).await.ok()
    }
}

fn run_sync_process(
    voice_msg_path: String,
    num_threads: &'static str,
    fut: TranscribingFut,
) -> Result<()> {
    let handle = Handle::current();
    let TranscribingFut {
        shared_transcription,
        msg_handle,
    } = fut;

    // `whisper` is too smart and pipes its output when it detects a non-interactive stdout, so
    // we have to fake being a tty to get it to stream for us. This also seems to fuck up the
    // logs formatting unfortunately.
    let mut whisper_stdout = fake_tty::bash_command(&format!(
        "whisper {voice_msg_path} --model medium.en --thread {num_threads} --fp16 False"
    ))?
    .stderr(Stdio::null())
    .spawn()?
    .stdout
    .expect("We need stdout");
    let mut read_buf = vec![0; 4_096];
    loop {
        let bytes_read = whisper_stdout.read(&mut read_buf)?;
        let utf8_snippet = std::str::from_utf8(&read_buf[..bytes_read])?;
        handle.block_on(async {
            {
                shared_transcription.lock().await.push_str(utf8_snippet);
            }
            msg_handle
                .send(Ok(Update::InProgress))
                .await
                .map_err(|_| Error::MessageHandleDied)
        })?;

        if bytes_read == 0 {
            break;
        }
    }

    Ok(())
}
