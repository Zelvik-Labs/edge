//! Request extraction dependencies compiled with an immutable metric generation.
use http::{HeaderMap, header::HeaderName};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

#[derive(Debug)]
struct HeaderField {
    name: HeaderName,
    key: Arc<str>,
}

#[derive(Debug)]
struct HostField {
    key: Arc<str>,
    index: usize,
    reverse: bool,
}

#[derive(Debug, Default)]
pub struct RequestFields {
    variables: BTreeSet<Arc<str>>,
    fixed: [u64; 2],
    response_headers: bool,
    response_names: Vec<Arc<str>>,
    origin: bool,
    headers: Vec<HeaderField>,
    host: Vec<HostField>,
    cookies: BTreeMap<Arc<str>, Arc<str>>,
    arguments: BTreeMap<Arc<str>, Arc<str>>,
}

impl RequestFields {
    pub fn compile(variables: BTreeSet<Arc<str>>) -> Self {
        let mut fields = Self {
            variables,
            ..Self::default()
        };
        let mut headers = BTreeMap::new();
        let mut response_names = BTreeSet::new();
        for name in &fields.variables {
            if let Some(index) = fixed_index(name) {
                fields.fixed[index / 64] |= 1 << (index % 64);
            }
            fields.response_headers |=
                name.starts_with("response.header.") || name.as_ref() == "response.contentType";
            fields.origin |= name.starts_with("origin.");
            if let Some(suffix) = name.to_ascii_lowercase().strip_prefix("response.header.") {
                response_names.insert(Arc::from(suffix));
            }
            if let Some(suffix) = name
                .strip_prefix("header.")
                .or_else(|| name.strip_prefix("http."))
                && let Ok(header) = HeaderName::from_bytes(suffix.as_bytes())
            {
                // Both template aliases resolve through header.*. Store one
                // canonical key even when casing or aliases are repeated.
                headers.insert(Arc::from(format!("header.{}", header.as_str())), header);
            }
            if let Some(suffix) = name.strip_prefix("cookie.") {
                fields.cookies.insert(Arc::from(suffix), Arc::clone(name));
            }
            if let Some(suffix) = name.strip_prefix("arg.") {
                fields.arguments.insert(Arc::from(suffix), Arc::clone(name));
            }
            let position = match name.as_ref() {
                "host.first" | "host.0" => Some((0, false)),
                "host.last" | "host.-1" => Some((0, true)),
                "host.1" => Some((1, false)),
                "host.2" => Some((2, false)),
                "host.3" => Some((3, false)),
                "host.4" => Some((4, false)),
                "host.-2" => Some((1, true)),
                "host.-3" => Some((2, true)),
                "host.-4" => Some((3, true)),
                "host.-5" => Some((4, true)),
                _ => None,
            };
            if let Some((index, reverse)) = position {
                fields.host.push(HostField {
                    key: Arc::clone(name),
                    index,
                    reverse,
                });
            }
        }
        fields.headers = headers
            .into_iter()
            .map(|(key, name)| HeaderField { name, key })
            .collect();
        fields.response_names = response_names.into_iter().collect();
        fields
    }

    pub(crate) fn register(&mut self, variable: Arc<str>) {
        self.variables.insert(variable);
    }

    pub(crate) fn finish(&mut self) {
        *self = Self::compile(std::mem::take(&mut self.variables));
    }

    pub fn variables(&self) -> &BTreeSet<Arc<str>> {
        &self.variables
    }

    #[inline(always)]
    pub fn wants(&self, name: &str) -> bool {
        if let Some(index) = fixed_index(name) {
            self.fixed[index / 64] & (1 << (index % 64)) != 0
        } else {
            self.variables.contains(name)
        }
    }

    pub fn wants_response_headers(&self) -> bool {
        self.response_headers
    }
    pub fn wants_origin(&self) -> bool {
        self.origin
    }
    pub fn wants_cookies(&self) -> bool {
        !self.cookies.is_empty()
    }
    pub fn wants_arguments(&self) -> bool {
        !self.arguments.is_empty()
    }

    pub fn cookie_key(&self, name: &str) -> Option<&str> {
        self.cookies.get(name).map(AsRef::as_ref)
    }

    pub fn argument_key(&self, name: &str) -> Option<&str> {
        self.arguments.get(name).map(AsRef::as_ref)
    }

    pub fn project_headers(&self, headers: &HeaderMap, values: &mut BTreeMap<String, String>) {
        for field in &self.headers {
            // Legacy extraction keeps the first valid UTF-8 value, including
            // when earlier duplicate values are invalid.
            if let Some(value) = headers
                .get_all(&field.name)
                .iter()
                .find_map(|value| value.to_str().ok())
            {
                values.insert(field.key.to_string(), value.to_string());
            }
        }
        if self.wants("headers") {
            let mut all = String::new();
            for (name, value) in headers {
                if let Ok(value) = value.to_str() {
                    if !all.is_empty() {
                        all.push('\n');
                    }
                    all.push_str(name.as_str());
                    all.push_str(": ");
                    all.push_str(value);
                }
            }
            values.insert("headers".to_string(), all);
        }
    }

    pub fn project_host_parts(&self, host: &str, values: &mut BTreeMap<String, String>) {
        for field in &self.host {
            let value = if field.reverse {
                host.rsplit('.').nth(field.index)
            } else {
                host.split('.').nth(field.index)
            };
            if let Some(value) = value {
                values.insert(field.key.to_string(), value.to_string());
            }
        }
    }

    pub fn project_response_headers(
        &self,
        headers: &HashMap<String, String>,
        values: &mut BTreeMap<String, String>,
    ) {
        if !self.wants_response_headers() {
            return;
        }
        for (name, value) in headers {
            if self.wants("response.contentType") && name.eq_ignore_ascii_case("content-type") {
                values.insert("response.contentType".to_string(), value.clone());
            }
            if self
                .response_names
                .binary_search_by(|candidate| {
                    candidate
                        .bytes()
                        .cmp(name.bytes().map(|byte| byte.to_ascii_lowercase()))
                })
                .is_ok()
            {
                values.insert(format!("response.header.{name}"), value.clone());
            }
        }
    }
}

#[inline(always)]
fn fixed_index(name: &str) -> Option<usize> {
    match name {
        "args" => Some(0),
        "bodyBytesSent" => Some(1),
        "bodyBytesSentBytes" => Some(2),
        "browser.isMobile" => Some(3),
        "bytesReceived" => Some(4),
        "bytesSent" => Some(5),
        "cname" => Some(6),
        "contentType" => Some(7),
        "cookies" => Some(8),
        "documentRoot" => Some(9),
        "edgeVersion" => Some(10),
        "headers" => Some(11),
        "host" => Some(12),
        "host.-1" => Some(13),
        "host.-2" => Some(14),
        "host.-3" => Some(15),
        "host.-4" => Some(16),
        "host.-5" => Some(17),
        "host.0" => Some(18),
        "host.1" => Some(19),
        "host.2" => Some(20),
        "host.3" => Some(21),
        "host.4" => Some(22),
        "host.first" => Some(23),
        "host.last" => Some(24),
        "hostname" => Some(25),
        "isArgs" => Some(26),
        "method" => Some(27),
        "msec" => Some(28),
        "node.id" => Some(29),
        "node.name" => Some(30),
        "node.role" => Some(31),
        "origin.addr" => Some(32),
        "origin.address" => Some(33),
        "origin.code" => Some(34),
        "origin.host" => Some(35),
        "origin.id" => Some(36),
        "origin.protocol" => Some(37),
        "origin.scheme" => Some(38),
        "path" => Some(39),
        "product.name" => Some(40),
        "product.version" => Some(41),
        "proto" => Some(42),
        "queryString" => Some(43),
        "rawRemoteAddr" => Some(44),
        "referer" => Some(45),
        "referer.host" => Some(46),
        "remoteAddr" => Some(47),
        "remoteAddrValue" => Some(48),
        "remotePort" => Some(49),
        "remoteUser" => Some(50),
        "request" => Some(51),
        "request.URI" => Some(52),
        "request.method" => Some(53),
        "request.path" => Some(54),
        "request.uri" => Some(55),
        "requestFilename" => Some(56),
        "requestId" => Some(57),
        "requestLength" => Some(58),
        "requestMethod" => Some(59),
        "requestPath" => Some(60),
        "requestPathExtension" => Some(61),
        "requestPathLowerExtension" => Some(62),
        "requestTime" => Some(63),
        "requestURI" => Some(64),
        "requestURL" => Some(65),
        "requestUri" => Some(66),
        "response.contentType" => Some(67),
        "scheme" => Some(68),
        "serverAddr" => Some(69),
        "serverName" => Some(70),
        "serverPort" => Some(71),
        "serverProtocol" => Some(72),
        "status" => Some(73),
        "statusMessage" => Some(74),
        "timeISO8601" => Some(75),
        "timeLocal" => Some(76),
        "timestamp" => Some(77),
        "uri" => Some(78),
        "userAgent" => Some(79),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(names: &[&str]) -> RequestFields {
        RequestFields::compile(names.iter().copied().map(Arc::from).collect())
    }

    #[test]
    fn fixed_and_unknown_fields_match_exact_set_membership() {
        let names = [
            "args",
            "bodyBytesSent",
            "bodyBytesSentBytes",
            "browser.isMobile",
            "bytesReceived",
            "bytesSent",
            "cname",
            "contentType",
            "cookies",
            "documentRoot",
            "edgeVersion",
            "headers",
            "host",
            "host.-1",
            "host.-2",
            "host.-3",
            "host.-4",
            "host.-5",
            "host.0",
            "host.1",
            "host.2",
            "host.3",
            "host.4",
            "host.first",
            "host.last",
            "hostname",
            "isArgs",
            "method",
            "msec",
            "node.id",
            "node.name",
            "node.role",
            "origin.addr",
            "origin.address",
            "origin.code",
            "origin.host",
            "origin.id",
            "origin.protocol",
            "origin.scheme",
            "path",
            "product.name",
            "product.version",
            "proto",
            "queryString",
            "rawRemoteAddr",
            "referer",
            "referer.host",
            "remoteAddr",
            "remoteAddrValue",
            "remotePort",
            "remoteUser",
            "request",
            "request.URI",
            "request.method",
            "request.path",
            "request.uri",
            "requestFilename",
            "requestId",
            "requestLength",
            "requestMethod",
            "requestPath",
            "requestPathExtension",
            "requestPathLowerExtension",
            "requestTime",
            "requestURI",
            "requestURL",
            "requestUri",
            "response.contentType",
            "scheme",
            "serverAddr",
            "serverName",
            "serverPort",
            "serverProtocol",
            "status",
            "statusMessage",
            "timeISO8601",
            "timeLocal",
            "timestamp",
            "uri",
            "userAgent",
        ];
        for wanted in names {
            let selected = fields(&[wanted]);
            for name in names {
                assert_eq!(selected.wants(name), name == wanted, "{wanted}: {name}");
            }
            assert!(!selected.wants("request.futureField"));
        }
        let selected = fields(&["request.futureField", "header.X-Test", "cookie.", "arg.%71"]);
        for name in ["request.futureField", "header.X-Test", "cookie.", "arg.%71"] {
            assert!(selected.wants(name));
        }
        assert!(!selected.wants("request.FutureField"));
        assert_eq!(selected.cookie_key(""), Some("cookie."));
        assert_eq!(selected.cookie_key("sid"), None);
        assert_eq!(selected.argument_key("%71"), Some("arg.%71"));
        assert_eq!(selected.argument_key("q"), None);
    }

    #[test]
    fn header_aliases_share_one_key_and_keep_first_valid_duplicate() {
        let selected = fields(&[
            "header.X-Test",
            "header.x-test",
            "http.X-TEST",
            "headers",
            "header.bad name",
        ]);
        let mut headers = HeaderMap::new();
        headers.append("x-test", http::HeaderValue::from_bytes(&[255]).unwrap());
        headers.append("x-test", http::HeaderValue::from_static("first"));
        headers.append("x-test", http::HeaderValue::from_static("second"));
        headers.insert("x-other", http::HeaderValue::from_static("unselected"));
        let mut values = BTreeMap::new();
        selected.project_headers(&headers, &mut values);
        assert_eq!(values["header.x-test"], "first");
        assert_eq!(values.len(), 2);
        let legacy_all = headers
            .iter()
            .filter_map(|(name, value)| value.to_str().ok().map(|value| format!("{name}: {value}")))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(values["headers"], legacy_all);
        assert_eq!(selected.variables().len(), 5);
        headers.remove("x-test");
        values.clear();
        selected.project_headers(&headers, &mut values);
        assert_eq!(values.len(), 1);
        assert_eq!(values["headers"], "x-other: unselected");
    }

    #[test]
    fn host_projection_matches_legacy_for_empty_short_and_long_hosts() {
        let names = [
            "host.first",
            "host.last",
            "host.0",
            "host.1",
            "host.2",
            "host.3",
            "host.4",
            "host.-1",
            "host.-2",
            "host.-3",
            "host.-4",
            "host.-5",
        ];
        for requested in [
            names.to_vec(),
            vec!["host"],
            vec!["host.1", "host.-2"],
            vec!["host.99"],
        ] {
            let selected = fields(&requested);
            for host in ["", ".", "localhost", "a.b", ".a.b.", "a.b.c.d.e.f.g"] {
                let parts = host.split('.').collect::<Vec<_>>();
                let mut expected = BTreeMap::new();
                for (name, index, reverse) in [
                    ("host.first", 0, false),
                    ("host.last", 0, true),
                    ("host.0", 0, false),
                    ("host.-1", 0, true),
                    ("host.1", 1, false),
                    ("host.-2", 1, true),
                    ("host.2", 2, false),
                    ("host.-3", 2, true),
                    ("host.3", 3, false),
                    ("host.-4", 3, true),
                    ("host.4", 4, false),
                    ("host.-5", 4, true),
                ] {
                    if requested.contains(&name) && index < parts.len() {
                        let part = if reverse {
                            parts[parts.len() - index - 1]
                        } else {
                            parts[index]
                        };
                        expected.insert(name.to_owned(), part.to_owned());
                    }
                }
                let mut actual = BTreeMap::new();
                selected.project_host_parts(host, &mut actual);
                assert_eq!(actual, expected, "{requested:?} / {host}");
            }
        }
    }

    #[test]
    fn empty_fields_do_not_materialize_request_attributes() {
        let selected = RequestFields::default();
        let mut headers = HeaderMap::new();
        headers.insert("x-test", http::HeaderValue::from_static("unselected"));
        let mut values = BTreeMap::new();
        selected.project_headers(&headers, &mut values);
        selected.project_host_parts("a.b.c", &mut values);
        assert!(values.is_empty());
        assert!(!selected.wants_cookies());
        assert!(!selected.wants_arguments());
        assert!(!selected.wants_origin());
        assert!(!selected.wants_response_headers());
    }

    #[test]
    fn response_selection_preserves_case_and_duplicate_name_behavior() {
        let headers = HashMap::from([
            ("Content-Type".to_string(), "first".to_string()),
            ("content-type".to_string(), "second".to_string()),
            ("X-Metric".to_string(), "selected".to_string()),
            ("x-other".to_string(), "unselected".to_string()),
        ]);
        for names in [
            vec![],
            vec!["response.contentType"],
            vec!["response.header.CONTENT-TYPE", "RESPONSE.HEADER.X-METRIC"],
            vec!["RESPONSE.HEADER.X-METRIC"],
        ] {
            let selected = fields(&names);
            let mut expected = BTreeMap::new();
            if names
                .iter()
                .any(|name| name.starts_with("response.header.") || *name == "response.contentType")
            {
                for (name, value) in &headers {
                    if names.contains(&"response.contentType")
                        && name.eq_ignore_ascii_case("content-type")
                    {
                        expected.insert("response.contentType".to_string(), value.clone());
                    }
                    let key = format!("response.header.{name}");
                    if names
                        .iter()
                        .any(|variable| variable.eq_ignore_ascii_case(&key))
                    {
                        expected.insert(key, value.clone());
                    }
                }
            }
            let mut actual = BTreeMap::new();
            selected.project_response_headers(&headers, &mut actual);
            assert_eq!(actual, expected, "{names:?}");
        }
    }
}
