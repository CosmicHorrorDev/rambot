use std::{io::Read, path::Path, process::Stdio};

use crate::telegram::Bot;

use tokio::{sync::oneshot, task::JoinSet};

struct Job {
    bot: Bot,
    voice_file_id: String,
    msg_handle: oneshot::Sender<Downloading>,
}

// All of these values represent the two sides of the state machine. The *Fut values are the
// transcriber side that send their updated state through the non-*Fut sides back to the message
struct StartFut(oneshot::Sender<Downloading>);

impl StartFut {
    fn downloading(self) -> DownloadingFut {
        let (tx, rx) = oneshot::channel();
        self.0.send(Ok(rx)).unwrap();
        DownloadingFut(tx)
    }
}

type Downloading = Result<oneshot::Receiver<Transcribing>, ()>;

struct DownloadingFut(oneshot::Sender<Transcribing>);

impl DownloadingFut {
    fn transcribing(self) -> TranscribingFut {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(Transcribing::InProgress {
                next: rx,
                transcription: String::new(),
            })
            .unwrap();
        TranscribingFut(tx)
    }
}

#[derive(Debug)]
pub enum Transcribing {
    InProgress {
        next: oneshot::Receiver<Transcribing>,
        transcription: String,
    },
    Finished(String),
    Error,
}

struct TranscribingFut(oneshot::Sender<Transcribing>);

impl TranscribingFut {
    fn in_progress(self, transcription: String) -> Self {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(Transcribing::InProgress {
                next: rx,
                transcription,
            })
            .unwrap();
        Self(tx)
    }

    fn job_finished(self, transcription: String) {
        self.0.send(Transcribing::Finished(transcription)).unwrap();
    }
}

#[derive(Clone)]
pub struct Pool(async_channel::Sender<Job>);

impl Pool {
    pub async fn spawn(num_workers: u8) -> Self {
        // TODO: switch this to NonZeroU8?
        assert!(num_workers != 0);
        let mut transcribers = JoinSet::new();
        let (tx_workers, rx_workers) = async_channel::bounded(32);
        for i in 0..num_workers {
            transcribers.spawn(run_transcriber(rx_workers.clone(), i));
        }

        // NOTE: Keep all the transcribers running in the background
        transcribers.detach_all();

        Self(tx_workers)
    }

    #[must_use]
    pub async fn submit_job(
        &self,
        bot: Bot,
        voice_file_id: String,
    ) -> oneshot::Receiver<Downloading> {
        let (msg_handle, job_handle) = oneshot::channel();
        log::info!("Starting transcribe task for {voice_file_id}");
        self.0
            .send(Job {
                bot,
                voice_file_id,
                msg_handle,
            })
            .await
            .unwrap();

        job_handle
    }
}

async fn run_transcriber(rx: async_channel::Receiver<Job>, id: u8) {
    while let Ok(Job {
        bot,
        voice_file_id,
        // TODO: rename this to like state_machine
        msg_handle,
    }) = rx.recv().await
    {
        let voice_msg_path = format!("/tmp/voice_msg_{id}.ogg");
        log::info!("Worker {id} got work {voice_file_id}");
        let start_fut = StartFut(msg_handle);
        let downloading_fut = start_fut.downloading();

        bot.download_file(Path::new(&voice_msg_path), voice_file_id)
            .await
            .unwrap();
        let duration_bytes = std::process::Command::new("ffprobe")
            .args([
                "-i",
                &voice_msg_path,
                "-show_entries",
                "format=duration",
                "-v",
                "quiet",
                "-of",
                "csv=p=0",
            ])
            .output()
            .unwrap()
            .stdout;
        let duration = String::from_utf8(duration_bytes)
            .unwrap()
            .trim()
            .parse::<f32>()
            .unwrap() as u32;
        let transcribing_fut = downloading_fut.transcribing();

        // Do slower transcriptions on longer messages just while this is running on weaker
        // hardware
        // TODO: remove after we get setup on a GPU
        let num_threads = if duration > 10 * 60 { "2" } else { "4" };

        if let Err(_) = tokio::task::spawn_blocking(move || {
            run_sync_process_wrapper(&voice_msg_path, &num_threads, transcribing_fut)
        })
        .await
        {
            // let _ = msg_handle.send(Update::ErroredOut).await;
            todo!("Send error back to message");
        }
    }
}

fn run_sync_process_wrapper(
    voice_msg_path: &str,
    num_threads: &str,
    transcribing_fut: TranscribingFut,
) {
    fn inner(
        voice_msg_path: &str,
        num_threads: &str,
        mut transcribing_fut: TranscribingFut,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // let handle = Handle::current();

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
        let mut read_buf = vec![0; 512];
        let mut transcription = String::new();
        loop {
            let bytes_read = whisper_stdout.read(&mut read_buf)?;
            transcription.push_str(std::str::from_utf8(&read_buf[..bytes_read])?);

            transcribing_fut = transcribing_fut.in_progress(transcription.to_owned());
            // handle.block_on(
            //     msg_handle.send(Update::CurrentTranscription(transcription.to_owned())),
            // )?;

            if bytes_read == 0 {
                break;
            }
        }

        // handle.block_on(msg_handle.send(Update::Finished(transcription)))?;
        transcribing_fut.job_finished(transcription);

        Ok(())
    }

    if let Err(_) = inner(voice_msg_path, num_threads, transcribing_fut) {
        // let handle = Handle::current();
        // let _ = handle.block_on(msg_handle.send(Update::ErroredOut));
        todo!("Report error back");
    }
}
