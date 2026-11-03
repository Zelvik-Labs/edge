// Candidate only: production still uses the original restored HeaderMap path.
fn candidate(headers: &[(String, String)]) -> bool {
    // Preserve the original large-map capacity behavior, including its limits.
    // This bound selects an optimization; it does not reject larger metadata.
    if headers.len() > 32 {
        return baseline(headers);
    }
    let mut content_length = None;
    for (name, value) in headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            return false;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            return false;
        };
        let Ok(text) = value.to_str() else {
            return false;
        };
        let is_content_length = name == http::header::CONTENT_LENGTH;
        if !should_store_response_header(name.as_str()) && !is_content_length {
            return false;
        }
        if is_content_length {
            let Ok(length) = text.trim().parse::<u64>() else {
                return false;
            };
            if content_length.is_some_and(|previous| previous != length) {
                return false;
            }
            content_length = Some(length);
        }
        if name == http::header::VARY {
            return false;
        }
        if name == http::header::CACHE_CONTROL
            && (cache_control_has_directive(text, "no-store")
                || cache_control_has_directive(text, "private")
                || cache_control_has_directive(text, "no-cache"))
        {
            return false;
        }
        if name == http::header::PRAGMA && cache_control_has_directive(text, "no-cache") {
            return false;
        }
        // Set-Cookie and every Transfer-Encoding are rejected by the storage
        // header gate above. A temporary map adds no further framing checks.
    }
    true
}
