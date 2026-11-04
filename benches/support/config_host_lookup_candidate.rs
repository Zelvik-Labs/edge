fn candidate(host: &str) -> std::borrow::Cow<'_, str> {
    let normalized = candidate_strip_addr_port(host).trim_end_matches('.');
    let (chunks, remainder) = normalized.as_bytes().as_chunks::<32>();
    let has_uppercase = chunks.iter().any(|chunk| {
        chunk
            .iter()
            .fold(false, |found, byte| found | byte.is_ascii_uppercase())
    }) || remainder
        .iter()
        .fold(false, |found, byte| found | byte.is_ascii_uppercase());
    if has_uppercase {
        std::borrow::Cow::Owned(normalized.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(normalized)
    }
}
