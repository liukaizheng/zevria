//! Frontend command staging and correlation.

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};
use zevria_session_api::SessionCommand;

pub struct RequestIdAllocator;

impl RequestIdAllocator {
    /// Correlation IDs are not reused if an inspector is replaced while an
    /// earlier request is still in flight.
    pub fn allocate(prefix: &str) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        format!("{prefix}{}", NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

pub type CommandOutbox = VecDeque<SessionCommand>;
