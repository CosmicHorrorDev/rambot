use std::{io::Read, path::Path, process::Stdio};

use crate::telegram::Bot;

use tokio::{runtime::Handle, sync::mpsc, task::JoinSet};

struct Job {
    bot: Bot,
    voice_file_id: String,
    msg_handle: mpsc::Sender<Update>,
}

#[derive(Debug)]
pub enum Update {
    Downloading,
    StartedTranscription,
    CurrentTranscription(String),
    Finished(String),
    ErroredOut,
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
    pub async fn submit_job(&self, bot: Bot, voice_file_id: String) -> mpsc::Receiver<Update> {
        let (msg_handle, job_handle) = mpsc::channel(32);
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
        msg_handle,
    }) = rx.recv().await
    {
        let voice_msg_path = format!("/tmp/voice_msg_{id}.ogg");
        log::info!("Worker {id} got work {voice_file_id}");
        let _ = msg_handle.send(Update::Downloading).await;
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
        let _ = msg_handle.send(Update::StartedTranscription).await;

        // Do slower transcriptions on longer messages just while this is running on weaker
        // hardware
        // TODO: remove after we get setup on a GPU
        let num_threads = if duration > 10 * 60 { "2" } else { "4" };

        let msg_handle2 = msg_handle.clone();
        if let Err(_) = tokio::task::spawn_blocking(move || {
            run_sync_process_wrapper(&voice_msg_path, &num_threads, msg_handle2)
        })
        .await
        {
            let _ = msg_handle.send(Update::ErroredOut).await;
        }
    }
}

fn run_sync_process_wrapper(
    voice_msg_path: &str,
    num_threads: &str,
    msg_handle: mpsc::Sender<Update>,
) {
    fn inner(
        voice_msg_path: &str,
        num_threads: &str,
        msg_handle: mpsc::Sender<Update>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let handle = Handle::current();

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

            handle.block_on(
                msg_handle.send(Update::CurrentTranscription(transcription.to_owned())),
            )?;

            if bytes_read == 0 {
                break;
            }
        }

        handle.block_on(msg_handle.send(Update::Finished(transcription)))?;

        Ok(())
    }

    if let Err(_) = inner(voice_msg_path, num_threads, msg_handle.clone()) {
        let handle = Handle::current();
        let _ = handle.block_on(msg_handle.send(Update::ErroredOut));
    }
}
