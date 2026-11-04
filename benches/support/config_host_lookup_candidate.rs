fn candidate(host: &str) -> std::borrow::Cow<'_, str> {
    let normalized = candidate_strip_addr_port(host).trim_end_matches('.');
    if normalized.as_bytes().iter().any(u8::is_ascii_uppercase) {
        std::borrow::Cow::Owned(normalized.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(normalized)
    }
}
