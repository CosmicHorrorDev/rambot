//! Home to a state machine and its interruptible handle representing the transcription process
//!
//! All the *Fut and non-*Fut values here represent the two sides of the transcription process'
//! state machine where the *Fut side automatically emits updates to the non-*Fut side that expand
//! out to follow the state machine's flow

use std::{process::Stdio, sync::Arc};

use crate::{telegram::Bot, utils::SegmentCallbackData, HandlerError, HandlerResult, Line};

use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot, Mutex},
};
use whisper_rs::{FullParams, WhisperContext, WhisperContextParameters};

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

pub type DownloadStarted = oneshot::Receiver<HandlerResult<Downloading>>;

#[must_use]
pub struct DownloadStartedFut {
    next: oneshot::Sender<HandlerResult<Downloading>>,
    meta: JobMeta,
}

impl DownloadStartedFut {
    pub async fn finish_download(self) -> Option<DownloadingFut> {
        let Self {
            next,
            meta: JobMeta {
                bot, voice_file_id, ..
            },
        } = self;
        let (tx, rx) = oneshot::channel();

        // TODO: tempdir here to download into
        let ogg_file = tempfile::Builder::new()
            .prefix("rambot")
            .suffix(".ogg")
            .tempfile()
            .unwrap();
        let ogg_path = ogg_file.path();

        if let Err(e) = bot.download_file(ogg_path, voice_file_id).await {
            next.send(Err(e.into())).ok()?;
            None
        } else {
            // TODO: switch to symphonia once they have an opus decoder
            let wav_file = tempfile::Builder::new()
                .prefix("rambot")
                .suffix(".wav")
                .tempfile()
                .unwrap();
            let wav_path = wav_file.path();
            #[rustfmt::skip]
            std::process::Command::new("ffmpeg")
                .arg("-i").arg(ogg_path)
                // Convert to i16 LE samples because that's what the example used
                .arg("-acodec").arg("pcm_s16le")
                // 16kHz
                .arg("-ar").arg("16000")
                // Skip confirmation
                .arg("-y")
                .arg(wav_path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();

            let wav_reader = hound::WavReader::open(wav_path).unwrap();
            let audio_data: Vec<_> = wav_reader
                .into_samples::<i16>()
                .map(|x| x.unwrap())
                .collect();
            let audio_data = whisper_rs::convert_integer_to_float_audio(&audio_data);

            next.send(Ok(rx)).ok()?;
            Some(DownloadingFut {
                next: tx,
                audio_data,
            })
        }
    }
}

// TODO: rename all `Downloading` -> `Downloaded`
pub type Downloading = oneshot::Receiver<HandlerResult<Transcribing>>;

#[must_use]
pub struct DownloadingFut {
    next: oneshot::Sender<HandlerResult<Transcribing>>,
    audio_data: Vec<f32>,
}

impl DownloadingFut {
    pub fn start_transcription(self) -> Option<TranscribingFut> {
        let Self { next, audio_data } = self;
        let (msg_handle, transcriber_handle) = mpsc::channel(16);
        let shared_transcription = Arc::default();
        next.send(Ok(Transcribing {
            finished: false,
            transcriber_handle,
            shared_transcription: Arc::clone(&shared_transcription),
        }))
        .ok()?;
        Some(TranscribingFut {
            msg_handle,
            shared_transcription,
            audio_data,
        })
    }
}

#[must_use]
pub struct Transcribing {
    finished: bool,
    transcriber_handle: mpsc::Receiver<HandlerResult<Update>>,
    shared_transcription: Arc<Mutex<String>>,
}

impl Transcribing {
    pub async fn next(&mut self) -> HandlerResult<Option<Line>> {
        let maybe_update = self
            .transcriber_handle
            .recv()
            .await
            .ok_or(HandlerError::WorkerDied)?;

        maybe_update.map(|update| match update {
            Update::Line(line) => Some(line),
            Update::Eof => None,
        })
    }
}

#[derive(Clone)]
pub struct TranscribingFut {
    msg_handle: mpsc::Sender<HandlerResult<Update>>,
    shared_transcription: Arc<Mutex<String>>,
    audio_data: Vec<f32>,
}

impl TranscribingFut {
    pub async fn finish_transcription(self) -> Option<()> {
        let fut = self.clone();
        let res = match tokio::task::spawn_blocking(move || run_sync_process(fut)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                log::warn!("Sync process wrapper returned an error: {err}");
                Err(err)
            }
            Err(err) => {
                log::warn!("Sync process wrapper died unexpectedly: {err}");
                Err(HandlerError::WorkerDied)
            }
        };

        if let Err(e) = res {
            self.msg_handle.send(Err(e)).await.ok()?;
        }

        Some(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Update {
    Line(Line),
    Eof,
}

impl From<Line> for Update {
    fn from(line: Line) -> Self {
        Self::Line(line)
    }
}

impl From<SegmentCallbackData> for Update {
    fn from(segment: SegmentCallbackData) -> Self {
        Self::Line(segment.into())
    }
}

fn run_sync_process(fut: TranscribingFut) -> HandlerResult {
    let TranscribingFut {
        shared_transcription,
        msg_handle,
        audio_data,
    } = fut;

    let model_path = dirs::data_dir().unwrap().join("rambot").join("model.bin");
    let params = WhisperContextParameters::new();
    let ctx = WhisperContext::new_with_params(model_path.to_str().unwrap(), params).unwrap();
    let mut state = ctx.create_state().unwrap();
    let mut params = FullParams::new(Default::default());
    // let (tx, _) = tokio::sync::mpsc::unbounded_channel::<()>();
    params.set_no_context(true);
    // TODO: This callback segfaults... Need to minimize and report the issue upstream
    // let msg_handle2 = msg_handle.clone();
    // params.set_progress_callback_safe(move |_| {
    //     Handle::current().block_on(async {
    //         let _ = tx.send(());
    //     });
    // });

    // Actually run the model on the audio file
    state.full(params, &audio_data).unwrap();

    Handle::current().block_on(async {
        let n_segments = state.full_n_segments().unwrap();
        for i in 0..n_segments {
            let start_timestamp = state.full_get_segment_t0(i).unwrap();
            let end_timestamp = state.full_get_segment_t1(i).unwrap();
            let text = state.full_get_segment_text(i).unwrap();
            let segment = SegmentCallbackData {
                segment: i,
                start_timestamp,
                end_timestamp,
                text,
            };
            let _ = msg_handle.send(Ok(segment.into())).await;
        }
        let _ = msg_handle.send(Ok(Update::Eof)).await;
    });

    Ok(())
}
