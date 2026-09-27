//! Temporary test-only access to real transmission selection, without a socket send.
use super::*;

pub(crate) fn derive(
    prepared: &PreparedRequest,
    provider: &mut OpenAiProvider,
    full: bool,
) -> usize {
    let transmission = if full {
        full_transmission(prepared)
    } else {
        select_transmission(prepared, provider)
    };
    std::hint::black_box(transmission.input.len())
}
