use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum TaskError {
    #[error("task `{name}` panicked")]
    Panicked { name: &'static str },
    #[error("task `{name}` failed")]
    Failed {
        name: &'static str,
        #[source]
        source: anyhow::Error,
    },
}

pub struct Shutdown {
    token: CancellationToken,
    tasks: JoinSet<Result<(), TaskError>>,
    names: HashMap<Id, &'static str>,
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            tasks: JoinSet::new(),
            names: HashMap::new(),
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub fn spawn<F>(&mut self, name: &'static str, fut: F)
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let token = self.token.clone();
        let handle = self.tasks.spawn(async move {
            // Drop guard ensures the global token is cancelled even on panic.
            let _guard = CancelOnDrop(token);
            match fut.await {
                Ok(()) => {
                    tracing::info!(task = name, "task exited");
                    Ok(())
                }
                Err(err) => {
                    tracing::error!(task = name, error = ?err, "task failed");
                    Err(TaskError::Failed { name, source: err })
                }
            }
        });
        self.names.insert(handle.id(), name);
    }

    pub fn spawn_os_thread<F>(&mut self, name: &'static str, f: F) -> std::io::Result<()>
    where
        F: FnOnce(CancellationToken) -> anyhow::Result<()> + Send + 'static,
    {
        let token = self.token.clone();
        let thread_token = token.clone();
        let (tx, rx) = oneshot::channel::<anyhow::Result<()>>();

        std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                // No catch_unwind: if `f` panics, `tx` is dropped without sending and the
                // async wrapper observes RecvError → TaskError::Panicked.
                let result = f(thread_token);
                let _ = tx.send(result);
            })?;

        let handle = self.tasks.spawn(async move {
            let _guard = CancelOnDrop(token);
            match rx.await {
                Ok(Ok(())) => {
                    tracing::info!(task = name, "os thread exited");
                    Ok(())
                }
                Ok(Err(err)) => {
                    tracing::error!(task = name, error = ?err, "os thread failed");
                    Err(TaskError::Failed { name, source: err })
                }
                Err(_) => {
                    tracing::error!(task = name, "os thread panicked");
                    Err(TaskError::Panicked { name })
                }
            }
        });
        self.names.insert(handle.id(), name);
        Ok(())
    }

    pub async fn run_until_signal(mut self) -> anyhow::Result<()> {
        let mut first_error: Option<anyhow::Error> = None;

        // Wait for either an external termination signal or any task to exit.
        // On Unix we listen for SIGTERM in addition to SIGINT/Ctrl-C so that
        // `systemctl stop`, `kill <pid>`, and OS reboot/shutdown all route
        // through the same drain path; without this, the output task never
        // gets a chance to push the final state-OFF message and the strip
        // stays lit until the firmware's stream-stale watchdog fires.
        #[cfg(unix)]
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("registering SIGTERM handler")?;

        tokio::select! {
            sig = tokio::signal::ctrl_c() => {
                sig.context("listening for Ctrl-C")?;
                tracing::info!("ctrl-c received, initiating shutdown");
            }
            _ = async {
                #[cfg(unix)]
                { sigterm.recv().await; }
                #[cfg(not(unix))]
                { std::future::pending::<()>().await; }
            } => {
                tracing::info!("sigterm received, initiating shutdown");
            }
            join = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                if let Some(res) = join {
                    Self::record(res, &mut self.names, &mut first_error);
                }
            }
        }
        self.token.cancel();

        let drain = async {
            while let Some(res) = self.tasks.join_next_with_id().await {
                Self::record(res, &mut self.names, &mut first_error);
            }
        };

        if timeout(DRAIN_TIMEOUT, drain).await.is_err() {
            tracing::warn!(
                timeout_secs = DRAIN_TIMEOUT.as_secs(),
                remaining = self.tasks.len(),
                "tasks did not finish within drain timeout; aborting"
            );
            self.tasks.shutdown().await;
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn record(
        res: Result<(Id, Result<(), TaskError>), JoinError>,
        names: &mut HashMap<Id, &'static str>,
        first_error: &mut Option<anyhow::Error>,
    ) {
        match res {
            Ok((id, Ok(()))) => {
                names.remove(&id);
            }
            Ok((id, Err(task_err))) => {
                names.remove(&id);
                if first_error.is_none() {
                    *first_error = Some(anyhow::Error::new(task_err));
                }
            }
            Err(join_err) => {
                let id = join_err.id();
                let name = names.remove(&id).unwrap_or("unknown");
                if join_err.is_panic() {
                    tracing::error!(task = name, "task panicked");
                    if first_error.is_none() {
                        *first_error = Some(anyhow::Error::new(TaskError::Panicked { name }));
                    }
                }
            }
        }
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn new_token_is_not_cancelled() {
        let s = Shutdown::new();
        assert!(!s.token().is_cancelled());
    }

    #[tokio::test]
    async fn ok_task_cancels_token_and_returns_ok() {
        let mut s = Shutdown::new();
        let observer = s.token();
        s.spawn("noop", async { Ok(()) });

        let res = timeout(Duration::from_secs(2), s.run_until_signal())
            .await
            .expect("run_until_signal must complete");
        assert!(res.is_ok(), "expected Ok, got {res:?}");
        assert!(observer.is_cancelled());
    }

    #[tokio::test]
    async fn failing_task_surfaces_error_and_cancels() {
        let mut s = Shutdown::new();
        let observer = s.token();
        s.spawn("boom", async { Err(anyhow::anyhow!("kaboom")) });

        let res = timeout(Duration::from_secs(2), s.run_until_signal())
            .await
            .expect("run_until_signal must complete");
        let err = res.expect_err("expected Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("boom"), "missing task name in error: {msg}");
        assert!(msg.contains("kaboom"), "missing source in error: {msg}");
        assert!(observer.is_cancelled());
    }

    #[tokio::test]
    async fn panicking_task_surfaces_with_name() {
        let mut s = Shutdown::new();
        let observer = s.token();
        s.spawn("panicker", async {
            panic!("intentional");
            #[allow(unreachable_code)]
            Ok(())
        });

        let res = timeout(Duration::from_secs(2), s.run_until_signal())
            .await
            .expect("run_until_signal must complete");
        let err = res.expect_err("expected Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("panicker"), "missing task name: {msg}");
        assert!(observer.is_cancelled());
    }

    #[tokio::test]
    async fn cancellation_completes_well_behaved_task() {
        let mut s = Shutdown::new();
        let token = s.token();
        let observed = Arc::new(AtomicBool::new(false));
        let inner_token = token.clone();
        let inner_observed = observed.clone();
        s.spawn("waiter", async move {
            inner_token.cancelled().await;
            inner_observed.store(true, Ordering::SeqCst);
            Ok(())
        });

        token.cancel();
        let res = timeout(Duration::from_secs(2), s.run_until_signal())
            .await
            .expect("run_until_signal must complete");
        assert!(res.is_ok(), "expected Ok, got {res:?}");
        assert!(observed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn os_thread_ok_propagates_and_cancels() {
        let mut s = Shutdown::new();
        let observer = s.token();
        s.spawn_os_thread("os", |_cancel| Ok(()))
            .expect("os thread spawn");

        let res = timeout(Duration::from_secs(2), s.run_until_signal())
            .await
            .expect("run_until_signal must complete");
        assert!(res.is_ok(), "expected Ok, got {res:?}");
        assert!(observer.is_cancelled());
    }

    #[tokio::test]
    async fn os_thread_err_propagates() {
        let mut s = Shutdown::new();
        s.spawn_os_thread("os", |_cancel| Err(anyhow::anyhow!("os fail")))
            .expect("os thread spawn");

        let res = timeout(Duration::from_secs(2), s.run_until_signal())
            .await
            .expect("run_until_signal must complete");
        let err = res.expect_err("expected Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("os"), "missing task name: {msg}");
        assert!(msg.contains("os fail"), "missing source: {msg}");
    }

    #[tokio::test]
    async fn os_thread_panic_surfaces_as_panicked() {
        let mut s = Shutdown::new();
        s.spawn_os_thread("oops", |_cancel| panic!("thread panic"))
            .expect("os thread spawn");

        let res = timeout(Duration::from_secs(2), s.run_until_signal())
            .await
            .expect("run_until_signal must complete");
        let err = res.expect_err("expected Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("oops"), "missing task name: {msg}");
        assert!(msg.contains("panicked"), "expected panicked text: {msg}");
    }

    #[tokio::test]
    async fn os_thread_observes_cancellation_token() {
        let mut s = Shutdown::new();
        let token = s.token();
        let observed = Arc::new(AtomicBool::new(false));
        let inner_observed = observed.clone();
        s.spawn_os_thread("os", move |cancel| {
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            inner_observed.store(true, Ordering::SeqCst);
            Ok(())
        })
        .expect("os thread spawn");

        token.cancel();
        let res = timeout(Duration::from_secs(2), s.run_until_signal())
            .await
            .expect("run_until_signal must complete");
        assert!(res.is_ok(), "expected Ok, got {res:?}");
        assert!(observed.load(Ordering::SeqCst));
    }
}
