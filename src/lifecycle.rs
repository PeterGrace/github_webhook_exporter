use std::{future::Future, io};

/// The operating-system event that initiated graceful shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    /// The process received SIGINT, normally from Ctrl-C.
    Interrupt,
    /// The process received SIGTERM from a service manager or orchestrator.
    Terminate,
}

/// Waits for SIGINT or SIGTERM through Tokio's asynchronous signal support.
///
/// # Errors
///
/// Returns an I/O error when the operating system's signal handler cannot be installed or fails
/// while waiting. SIGTERM is available on Unix; other platforms wait for Tokio's Ctrl-C event.
pub async fn shutdown_signal() -> io::Result<ShutdownSignal> {
    let interrupt = tokio::signal::ctrl_c();

    #[cfg(unix)]
    let terminate = {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        async move {
            signal.recv().await;
            Ok(())
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<io::Result<()>>();

    select_shutdown_signal(interrupt, terminate).await
}

async fn select_shutdown_signal<I, T>(interrupt: I, terminate: T) -> io::Result<ShutdownSignal>
where
    I: Future<Output = io::Result<()>>,
    T: Future<Output = io::Result<()>>,
{
    tokio::pin!(interrupt);
    tokio::pin!(terminate);

    tokio::select! {
        result = &mut interrupt => result.map(|()| ShutdownSignal::Interrupt),
        result = &mut terminate => result.map(|()| ShutdownSignal::Terminate),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{pending, ready},
        io,
    };

    use super::{select_shutdown_signal, ShutdownSignal};

    #[tokio::test]
    async fn interrupt_and_terminate_map_to_distinct_normalized_signals() {
        assert_eq!(
            select_shutdown_signal(ready(Ok(())), pending())
                .await
                .expect("interrupt signal succeeds"),
            ShutdownSignal::Interrupt
        );
        assert_eq!(
            select_shutdown_signal(pending(), ready(Ok(())))
                .await
                .expect("terminate signal succeeds"),
            ShutdownSignal::Terminate
        );
    }

    #[tokio::test]
    async fn signal_registration_failures_are_returned() {
        let error = select_shutdown_signal(
            ready(Err(io::Error::other("signal registration failed"))),
            pending(),
        )
        .await
        .expect_err("signal failure must be returned");

        assert_eq!(error.kind(), io::ErrorKind::Other);
    }
}
