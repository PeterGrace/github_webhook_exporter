use std::io;

use thiserror::Error;
use tracing::Subscriber;
use tracing_subscriber::{fmt::MakeWriter, layer::SubscriberExt, EnvFilter, Registry};

/// A failure while constructing or installing local structured logging.
#[derive(Debug, Error)]
pub enum TelemetryError {
    /// The tracing filter directive is invalid.
    #[error("RUST_LOG contains an invalid tracing filter")]
    InvalidFilter(#[source] tracing_subscriber::filter::ParseError),
    /// Another global tracing subscriber has already been installed.
    #[error("the global tracing subscriber is already initialized")]
    AlreadyInitialized(#[source] tracing::subscriber::SetGlobalDefaultError),
}

/// Installs the production tracing subscriber with structured output to stderr.
///
/// # Errors
///
/// Returns [`TelemetryError`] when `rust_log` is invalid or a global subscriber is already set.
pub fn init(rust_log: &str) -> Result<(), TelemetryError> {
    let subscriber = build_subscriber(rust_log, io::stderr)?;
    tracing::subscriber::set_global_default(subscriber).map_err(TelemetryError::AlreadyInitialized)
}

/// Installs a minimal error-level subscriber when normal initialization fails.
///
/// If a subscriber is already installed, this function leaves it unchanged.
pub fn init_fallback() {
    drop(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(EnvFilter::new("error"))
            .with_writer(io::stderr)
            .try_init(),
    );
}

fn build_subscriber<W>(
    rust_log: &str,
    writer: W,
) -> Result<impl Subscriber + Send + Sync, TelemetryError>
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(rust_log).map_err(TelemetryError::InvalidFilter)?;
    Ok(Registry::default().with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer),
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use super::build_subscriber;

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn contents(&self) -> String {
            let bytes = self.0.lock().expect("capture lock is available").clone();
            String::from_utf8(bytes).expect("tracing output is UTF-8")
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock is available")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for SharedWriter {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn subscriber_honors_the_validated_filter() {
        let output = SharedWriter::default();
        let subscriber = build_subscriber("github_webhook_exporter=debug", output.clone())
            .expect("filter is valid");

        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "github_webhook_exporter", "visible-debug-event");
            tracing::debug!(target: "unrelated_dependency", "hidden-debug-event");
        });

        let rendered = output.contents();
        assert!(rendered.contains("visible-debug-event"));
        assert!(!rendered.contains("hidden-debug-event"));
    }
}
