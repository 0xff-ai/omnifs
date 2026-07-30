//! The single durable writer.
//!
//! One task owns the sole write connection. Callers hand it a job rather than
//! a description of a job, so adding a durable operation touches one place.

use anyhow::Context as _;
use sqlx::Connection as _;
use sqlx::sqlite::SqliteConnection;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::{mpsc, oneshot};

const WRITER_QUEUE_CAPACITY: usize = 32;

/// A job owns the connection for its duration and hands it back, which keeps
/// every queued future `'static` and the queue itself trivially typed.
type WriterJob = Box<
    dyn FnOnce(SqliteConnection) -> Pin<Box<dyn Future<Output = SqliteConnection> + Send>> + Send,
>;

enum WriterCommand {
    Run(WriterJob),
    Shutdown { reply: oneshot::Sender<()> },
}

pub(crate) struct StateWriter {
    sender: mpsc::Sender<WriterCommand>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<anyhow::Result<()>>>>,
}

impl StateWriter {
    pub(crate) fn spawn(connection: SqliteConnection) -> Self {
        let (sender, receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        Self {
            sender,
            task: tokio::sync::Mutex::new(Some(tokio::spawn(run_writer(connection, receiver)))),
        }
    }

    /// Queue one durable job. It runs to completion even when its caller stops
    /// waiting for the reply.
    pub(crate) async fn call<T, F, Fut>(&self, body: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(SqliteConnection) -> Fut + Send + 'static,
        Fut: Future<Output = (SqliteConnection, T)> + Send + 'static,
    {
        let (reply, receiver) = oneshot::channel();
        let job: WriterJob = Box::new(move |connection| {
            Box::pin(async move {
                let (connection, value) = body(connection).await;
                let _ = reply.send(value);
                connection
            })
        });
        self.sender
            .send(WriterCommand::Run(job))
            .await
            .map_err(|_| anyhow::anyhow!("StateStore writer stopped"))?;
        receiver.await.context("StateStore writer dropped reply")
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        let mut task = self.task.lock().await;
        let Some(task) = task.take() else {
            return Ok(());
        };
        let (reply, receiver) = oneshot::channel();
        let command_result = async {
            self.sender
                .send(WriterCommand::Shutdown { reply })
                .await
                .map_err(|_| anyhow::anyhow!("StateStore writer stopped before shutdown"))?;
            receiver
                .await
                .context("StateStore writer dropped shutdown reply")
        }
        .await;
        let task_result = task
            .await
            .context("join StateStore writer task")?
            .context("StateStore writer failed");
        command_result?;
        task_result
    }
}

async fn run_writer(
    mut connection: SqliteConnection,
    mut receiver: mpsc::Receiver<WriterCommand>,
) -> anyhow::Result<()> {
    while let Some(command) = receiver.recv().await {
        match command {
            WriterCommand::Run(job) => connection = job(connection).await,
            WriterCommand::Shutdown { reply } => {
                let _ = reply.send(());
                return connection.close().await.context("close StateStore writer");
            },
        }
    }
    connection.close().await.context("close StateStore writer")
}
