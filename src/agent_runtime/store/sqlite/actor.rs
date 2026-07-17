use futures::future::BoxFuture;
use libsql::{Connection, Database};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Duration;

use crate::agent_runtime::MuzenError;

const CONNECTION_QUEUE_CAPACITY: usize = 256;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

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
    send_timeout: Duration,
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
        Self::start_with_send_timeout(database, connection, SEND_TIMEOUT)
    }

    fn start_with_send_timeout(
        database: Database,
        connection: Connection,
        send_timeout: Duration,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Box<dyn Operation>>(CONNECTION_QUEUE_CAPACITY);
        tokio::spawn(async move {
            let _database = database;
            while let Some(operation) = receiver.recv().await {
                operation.execute(&connection).await;
            }
        });
        Self {
            sender,
            send_timeout,
        }
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
        // Dropping a timed-out `send` releases its reserved queue position. No lock is held
        // while waiting, and the actor continues polling the receiver on its dedicated task.
        match tokio::time::timeout(self.send_timeout, self.sender.send(operation)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(MuzenError::internal(
                    "SQLite connection actor is unavailable",
                ));
            }
            Err(_) => {
                return Err(MuzenError::resource_exhausted(
                    "SQLite connection actor queue is full",
                ));
            }
        }
        result_rx
            .await
            .map_err(|_| MuzenError::internal("SQLite connection actor stopped unexpectedly"))?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use libsql::Builder;
    use tokio::sync::{oneshot, Barrier};

    use super::*;
    use crate::agent_runtime::ErrorCode;

    async fn actor(send_timeout: Duration) -> (tempfile::TempDir, Arc<ConnectionActor>) {
        let directory = tempfile::tempdir().expect("temporary SQLite actor directory");
        let database = Builder::new_local(directory.path().join("actor.db"))
            .build()
            .await
            .expect("SQLite actor database");
        let connection = database.connect().expect("SQLite actor connection");
        let actor = ConnectionActor::start_with_send_timeout(database, connection, send_timeout);
        (directory, Arc::new(actor))
    }

    async fn wait_until_queue_is_full(actor: &ConnectionActor) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while actor.sender.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actor queue should fill");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn burst_larger_than_connection_queue_waits_for_capacity() {
        let (_directory, actor) = actor(SEND_TIMEOUT).await;
        let (blocked_tx, blocked_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let blocked_actor = Arc::clone(&actor);
        let blocker = tokio::spawn(async move {
            blocked_actor
                .call(move |_| {
                    Box::pin(async move {
                        let _ = blocked_tx.send(());
                        let _ = release_rx.await;
                        Ok(())
                    })
                })
                .await
        });
        blocked_rx.await.expect("blocking operation started");

        let call_count = CONNECTION_QUEUE_CAPACITY + 32;
        let barrier = Arc::new(Barrier::new(call_count + 1));
        let calls = (0..call_count)
            .map(|value| {
                let actor = Arc::clone(&actor);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    actor
                        .call(move |_| Box::pin(async move { Ok(value) }))
                        .await
                })
            })
            .collect::<Vec<_>>();
        barrier.wait().await;
        wait_until_queue_is_full(&actor).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        release_tx.send(()).expect("release blocking operation");

        blocker
            .await
            .expect("blocking task")
            .expect("blocking call");
        for (expected, call) in calls.into_iter().enumerate() {
            assert_eq!(
                call.await.expect("call task").expect("queued call"),
                expected
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn full_connection_queue_times_out_with_resource_exhausted() {
        let (_directory, actor) = actor(Duration::from_millis(10)).await;
        let (blocked_tx, blocked_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let blocked_actor = Arc::clone(&actor);
        let blocker = tokio::spawn(async move {
            blocked_actor
                .call(move |_| {
                    Box::pin(async move {
                        let _ = blocked_tx.send(());
                        let _ = release_rx.await;
                        Ok(())
                    })
                })
                .await
        });
        blocked_rx.await.expect("blocking operation started");

        let queued = (0..CONNECTION_QUEUE_CAPACITY)
            .map(|_| {
                let actor = Arc::clone(&actor);
                tokio::spawn(async move { actor.call(|_| Box::pin(async { Ok(()) })).await })
            })
            .collect::<Vec<_>>();
        wait_until_queue_is_full(&actor).await;

        let error = actor
            .call(|_| Box::pin(async { Ok(()) }))
            .await
            .expect_err("overflowing call should time out");
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(error.message(), "SQLite connection actor queue is full");

        release_tx.send(()).expect("release blocking operation");
        blocker
            .await
            .expect("blocking task")
            .expect("blocking call");
        for call in queued {
            call.await.expect("queued task").expect("queued call");
        }
    }
}
