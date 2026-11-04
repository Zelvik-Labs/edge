fn candidate(session: &Session, raw_ip: IpAddr) -> IpAddr {
    if !is_local_ip(&raw_ip) {
        return raw_ip;
    }
    let headers = &session.req_header().headers;
    if headers.len() <= 4 {
        // Preserve cheap lookups for the two highest-priority headers.
        for header in &FALLBACK_IP_HEADERS[..2] {
            if let Some(value) = headers
                .get(header)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                && let Some(ip) = parse_candidate_ip(value)
            {
                return canonicalize_ip(ip);
            }
        }
        let mut seen = 3u16;
        let mut best = FALLBACK_IP_HEADERS.len();
        let mut result = raw_ip;
        for (name, value) in headers {
            // These standard fields are never sources in the fixed IP list.
            if matches!(name.as_str(), "host" | "user-agent" | "accept") {
                continue;
            }
            let Some(priority) = FALLBACK_IP_HEADERS.iter().position(|header| header == name)
            else {
                continue;
            };
            let bit = 1u16 << priority;
            if seen & bit != 0 || priority >= best {
                continue;
            }
            seen |= bit;
            if let Some(value) = value
                .to_str()
                .ok()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                && let Some(ip) = parse_candidate_ip(value)
            {
                best = priority;
                result = canonicalize_ip(ip);
            }
        }
        return result;
    }
    for header in FALLBACK_IP_HEADERS.iter() {
        if let Some(value) = session
            .req_header()
            .headers
            .get(header)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && let Some(ip) = parse_candidate_ip(value)
        {
            return canonicalize_ip(ip);
        }
    }
    raw_ip
}
