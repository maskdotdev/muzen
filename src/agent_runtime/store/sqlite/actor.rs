use futures::future::BoxFuture;
use libsql::{Connection, Database};
use tokio::sync::{mpsc, oneshot};

use crate::agent_runtime::MuzenError;

const CONNECTION_QUEUE_CAPACITY: usize = 256;

trait Operation: Send {
    fn execute<'a>(self: Box<Self>, connection: &'a Connection) -> BoxFuture<'a, ()>;
}

struct TypedOperation<T, F> {
    operation: F,
    result_tx: oneshot::Sender<Result<T, MuzenError>>,
}

impl<T, F> Operation for TypedOperation<T, F>
where
    T: Send + 'static,
    F: for<'a> FnOnce(&'a Connection) -> BoxFuture<'a, Result<T, MuzenError>> + Send + 'static,
{
    fn execute<'a>(self: Box<Self>, connection: &'a Connection) -> BoxFuture<'a, ()> {
        let Self {
            operation,
            result_tx,
        } = *self;
        Box::pin(async move {
            let _ = result_tx.send(operation(connection).await);
        })
    }
}

pub(super) struct ConnectionActor {
    sender: mpsc::Sender<Box<dyn Operation>>,
}

impl std::fmt::Debug for ConnectionActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionActor")
            .finish_non_exhaustive()
    }
}

impl ConnectionActor {
    pub(super) fn start(database: Database, connection: Connection) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Box<dyn Operation>>(CONNECTION_QUEUE_CAPACITY);
        tokio::spawn(async move {
            let _database = database;
            while let Some(operation) = receiver.recv().await {
                operation.execute(&connection).await;
            }
        });
        Self { sender }
    }

    pub(super) async fn call<T, F>(&self, operation: F) -> Result<T, MuzenError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a Connection) -> BoxFuture<'a, Result<T, MuzenError>> + Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        let operation = Box::new(TypedOperation {
            operation,
            result_tx,
        });
        self.sender
            .try_send(operation)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    MuzenError::resource_exhausted("SQLite connection actor queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    MuzenError::internal("SQLite connection actor is unavailable")
                }
            })?;
        result_rx
            .await
            .map_err(|_| MuzenError::internal("SQLite connection actor stopped unexpectedly"))?
    }
}
