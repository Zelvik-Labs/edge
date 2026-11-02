//! Storage encoding is independent of HTTP Content-Encoding. Small known-size
//! bodies avoid a compression context per fill; old compressed objects remain
//! readable through their existing metadata flag. Unknown-size bodies retain
//! the streaming compression policy rather than buffering to discover size.
const MIN_COMPRESSED_BODY_BYTES: u64 = 64 * 1024;

pub(crate) fn should_compress(
    content_type: &str,
    content_encoding: &str,
    length: Option<u64>,
) -> bool {
    !length.is_some_and(|length| length < MIN_COMPRESSED_BODY_BYTES)
        && content_encoding.is_empty()
        && (content_type.contains("text/")
            || content_type.contains("json")
            || content_type.contains("javascript")
            || content_type.contains("xml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_text_avoids_context_but_streaming_and_large_text_keep_compression() {
        for size in [0, 1024, 32 * 1024, MIN_COMPRESSED_BODY_BYTES - 1] {
            assert!(!should_compress("text/plain", "", Some(size)));
        }
        for length in [None, Some(MIN_COMPRESSED_BODY_BYTES), Some(1024 * 1024)] {
            assert!(should_compress("text/plain", "", length));
            assert!(should_compress("application/json", "", length));
            assert!(!should_compress("text/plain", "gzip", length));
            assert!(!should_compress("image/png", "", length));
        }
    }
}
