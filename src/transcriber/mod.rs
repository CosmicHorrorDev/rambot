mod state_machine;
pub use state_machine::DownloadStarted;
use state_machine::{JobFut, JobMeta};

use crate::telegram::Bot;

use tokio::{sync::oneshot, task::JoinSet};

#[derive(Clone)]
pub struct Pool(async_channel::Sender<JobFut>);

impl Pool {
    pub async fn spawn(num_workers: u8) -> Self {
        // TODO: switch this to NonZeroU8?
        assert!(num_workers != 0);
        let mut transcribers = JoinSet::new();
        let (tx_workers, rx_workers) = async_channel::bounded(32);
        for i in 0..num_workers {
            transcribers.spawn(run_worker(rx_workers.clone(), i));
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
        voice_msg_duration_secs: u32,
    ) -> oneshot::Receiver<DownloadStarted> {
        let (msg_handle, job_handle) = oneshot::channel();
        log::info!("Starting transcribe task for {voice_file_id}");
        let _ = self
            .0
            .send(JobFut {
                next: msg_handle,
                meta: JobMeta {
                    bot,
                    voice_file_id,
                    voice_msg_duration_secs,
                },
            })
            .await;

        job_handle
    }
}

// TODO: keep the model around and use a timeout
async fn run_worker(rx: async_channel::Receiver<JobFut>, id: u8) {
    while let Ok(job) = rx.recv().await {
        log::info!("Worker {} got work {}", id, job.meta.voice_file_id);
        if run_transcription_process(job).await.is_none() {
            log::warn!("Transcription job died. Oh well");
        }
    }
}

async fn run_transcription_process(job: JobFut) -> Option<()> {
    job.start_download()?
        .finish_download()
        .await?
        .start_transcription()?
        .finish_transcription()
        .await
}
