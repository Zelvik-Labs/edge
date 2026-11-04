fn candidate(host: &str) -> std::borrow::Cow<'_, str> {
    let normalized = candidate_strip_addr_port(host).trim_end_matches('.');
    let mut chunks = normalized.as_bytes().chunks_exact(32);
    let has_uppercase = chunks.by_ref().any(|chunk| {
        chunk
            .iter()
            .fold(false, |found, byte| found | byte.is_ascii_uppercase())
    }) || chunks
        .remainder()
        .iter()
        .fold(false, |found, byte| found | byte.is_ascii_uppercase());
    if has_uppercase {
        std::borrow::Cow::Owned(normalized.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(normalized)
    }
}
