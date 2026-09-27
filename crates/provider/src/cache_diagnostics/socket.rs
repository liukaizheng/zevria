//! Optional sidecar only. The operational terminal watch and precedence stay authoritative.
use crate::websocket_session::OpenAiWebSocketTerminalCategory as Category;
use std::{
    io::ErrorKind,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio_tungstenite::tungstenite::Error;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Observation {
    pub at: Option<Instant>,
    pub category: Category,
    pub source: &'static str,
    pub error_class: Option<&'static str>,
    pub io_kind: Option<ErrorKind>,
}

#[derive(Clone, Default)]
pub(crate) struct SocketDiagnostics(Arc<Mutex<Option<Observation>>>);

impl SocketDiagnostics {
    pub(crate) fn observation(&self) -> Option<Observation> {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    pub(crate) fn disconnected(category: Category) -> Self {
        Self(Arc::new(Mutex::new(Some(Observation {
            at: None,
            category,
            source: "inferred",
            error_class: None,
            io_kind: None,
        }))))
    }

    pub(crate) fn error(&self, category: Category, error: &Error) {
        let (error_class, io_kind) = match error {
            Error::Io(error) => ("io", Some(error.kind())),
            Error::ConnectionClosed => ("connection_closed", None),
            Error::AlreadyClosed => ("already_closed", None),
            Error::Protocol(_) => ("protocol", None),
            Error::Tls(_) => ("tls", None),
            Error::Capacity(_) => ("capacity", None),
            _ => ("other", None),
        };
        let mut observation = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        observation.get_or_insert_with(|| Observation {
            at: Some(Instant::now()),
            category,
            source: "pump",
            error_class: Some(error_class),
            io_kind,
        });
    }

    pub(crate) fn observed(&self, category: Category, source: &'static str) {
        let mut observation = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        observation.get_or_insert_with(|| Observation {
            at: Some(Instant::now()),
            category,
            source,
            error_class: None,
            io_kind: None,
        });
    }

    pub(crate) fn finished(&self, category: Category) {
        let mut observation = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        // Queue overflow/closure can override an underlying read/pong error.
        // Keep that bounded cause and its first observation time, but reflect
        // the pump's actual final category rather than changing its choice.
        match observation.as_mut() {
            Some(observation) => observation.category = category,
            None => {
                *observation = Some(Observation {
                    at: Some(Instant::now()),
                    category,
                    source: "pump",
                    error_class: None,
                    io_kind: None,
                })
            }
        }
    }
}
