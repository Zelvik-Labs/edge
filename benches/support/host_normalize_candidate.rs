fn candidate_strip_addr_port(value: &str) -> &str {
    if let Some(rest) = value.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return &rest[..end];
    }
    if value.matches(':').count() == 1
        && let Some((host, _)) = value.rsplit_once(':')
    {
        return host;
    }
    value
}

fn candidate(host: &str) -> String {
    let host = host.rsplit('@').next().unwrap_or(host).trim();
    candidate_strip_addr_port(host)
        .trim_end_matches('.')
        .to_ascii_lowercase()
}
