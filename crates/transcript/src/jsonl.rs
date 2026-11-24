/// Reuse the allocation for ordinary records without retaining a large image
/// or batch allocation for the lifetime of the writer, even after an error.
pub(crate) fn with_record_buffer<T>(
    buffer: &mut Vec<u8>,
    use_buffer: impl FnOnce(&mut Vec<u8>) -> T,
) -> T {
    const RETAINED_CAPACITY: usize = 64 * 1024;
    buffer.clear();
    let result = use_buffer(buffer);
    buffer.clear();
    if buffer.capacity() > RETAINED_CAPACITY {
        buffer.shrink_to(RETAINED_CAPACITY);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_records_do_not_set_the_retained_buffer_size() {
        let mut buffer = Vec::new();
        with_record_buffer(&mut buffer, |buffer| buffer.resize(256 * 1024, 0));
        assert!(buffer.is_empty());
        assert!(buffer.capacity() <= 64 * 1024);
    }
}
