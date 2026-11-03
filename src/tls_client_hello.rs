// Pure TLS record / ClientHello inspection shared by kernel and AF_XDP SNI paths.

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ClientHelloParse {
    Found(String),
    NeedMore,
    NotClientHello,
}

pub(crate) fn parse_tls_client_hello_sni(buf: &[u8]) -> ClientHelloParse {
    if buf.len() < 9 {
        return ClientHelloParse::NeedMore;
    }
    if buf[0] != 22 {
        return ClientHelloParse::NotClientHello;
    }

    let first_record_len = usize::from(u16::from_be_bytes([buf[3], buf[4]]));
    if buf.len() < 5 + first_record_len {
        return ClientHelloParse::NeedMore;
    }
    let first_record = &buf[5..5 + first_record_len];
    if first_record.len() < 4 {
        return ClientHelloParse::NeedMore;
    }
    if first_record[0] != 1 {
        return ClientHelloParse::NotClientHello;
    }
    let hello_len = ((usize::from(first_record[1])) << 16)
        | ((usize::from(first_record[2])) << 8)
        | usize::from(first_record[3]);
    let needed_handshake_len = 4 + hello_len;

    if first_record.len() >= needed_handshake_len {
        return parse_contiguous_hello_sni(&first_record[4..needed_handshake_len]);
    }

    // Validate all record boundaries before interpreting fields. Incomplete
    // input must not allocate using the untrusted 24-bit handshake length.
    let mut available = first_record_len;
    let mut pos = 5 + first_record_len;
    while pos + 5 <= buf.len() && available < needed_handshake_len {
        if buf[pos] != 22 {
            return ClientHelloParse::NotClientHello;
        }
        let record_len = usize::from(u16::from_be_bytes([buf[pos + 3], buf[pos + 4]]));
        if buf.len() < pos + 5 + record_len {
            return ClientHelloParse::NeedMore;
        }
        available += record_len;
        pos += 5 + record_len;
    }
    if available < needed_handshake_len {
        return ClientHelloParse::NeedMore;
    }
    let hello = RecordPayload {
        wire: buf,
        pos: 9,
        record_end: 5 + first_record_len,
        remaining: hello_len,
    };
    match parse_fragmented_hello_sni(hello) {
        Some(host) => ClientHelloParse::Found(host),
        None => ClientHelloParse::NotClientHello,
    }
}

fn parse_contiguous_hello_sni(hello: &[u8]) -> ClientHelloParse {
    let mut pos = 0usize;

    pos += 2; // legacy_version
    pos += 32; // random
    if hello.len() < pos + 1 {
        return ClientHelloParse::NotClientHello;
    }
    let session_len = usize::from(hello[pos]);
    pos += 1 + session_len;
    if hello.len() < pos + 2 {
        return ClientHelloParse::NotClientHello;
    }
    let cipher_len = usize::from(u16::from_be_bytes([hello[pos], hello[pos + 1]]));
    pos += 2 + cipher_len;
    if hello.len() < pos + 1 {
        return ClientHelloParse::NotClientHello;
    }
    let compression_len = usize::from(hello[pos]);
    pos += 1 + compression_len;
    if hello.len() < pos + 2 {
        return ClientHelloParse::NotClientHello;
    }
    let extensions_len = usize::from(u16::from_be_bytes([hello[pos], hello[pos + 1]]));
    pos += 2;
    if hello.len() < pos + extensions_len {
        return ClientHelloParse::NotClientHello;
    }

    let extensions = &hello[pos..pos + extensions_len];
    let mut ext_pos = 0usize;
    while ext_pos + 4 <= extensions.len() {
        let ext_type = u16::from_be_bytes([extensions[ext_pos], extensions[ext_pos + 1]]);
        let ext_len = usize::from(u16::from_be_bytes([
            extensions[ext_pos + 2],
            extensions[ext_pos + 3],
        ]));
        ext_pos += 4;
        if extensions.len() < ext_pos + ext_len {
            return ClientHelloParse::NotClientHello;
        }
        if ext_type == 0 {
            let ext = &extensions[ext_pos..ext_pos + ext_len];
            if ext.len() < 2 {
                return ClientHelloParse::NotClientHello;
            }
            let list_len = usize::from(u16::from_be_bytes([ext[0], ext[1]]));
            if ext.len() < 2 + list_len {
                return ClientHelloParse::NotClientHello;
            }
            let mut name_pos = 2usize;
            while name_pos + 3 <= 2 + list_len {
                let name_type = ext[name_pos];
                let name_len =
                    usize::from(u16::from_be_bytes([ext[name_pos + 1], ext[name_pos + 2]]));
                name_pos += 3;
                if ext.len() < name_pos + name_len {
                    return ClientHelloParse::NotClientHello;
                }
                if name_type == 0 {
                    let Some(host) = std::str::from_utf8(&ext[name_pos..name_pos + name_len])
                        .ok()
                        .map(|host| host.to_ascii_lowercase())
                    else {
                        return ClientHelloParse::NotClientHello;
                    };
                    return ClientHelloParse::Found(host);
                }
                name_pos += name_len;
            }
        }
        ext_pos += ext_len;
    }
    ClientHelloParse::NotClientHello
}

// Cursor over already validated record payloads. It excludes the handshake
// header and stops at its declared end, even when the final record has a tail.
struct RecordPayload<'a> {
    wire: &'a [u8],
    pos: usize,
    record_end: usize,
    remaining: usize,
}

impl<'a> RecordPayload<'a> {
    fn chunk(&mut self) -> &'a [u8] {
        while self.pos == self.record_end && self.remaining > 0 {
            let len = usize::from(u16::from_be_bytes([
                self.wire[self.pos + 3],
                self.wire[self.pos + 4],
            ]));
            self.pos += 5;
            self.record_end = self.pos + len;
        }
        &self.wire[self.pos..self.record_end.min(self.pos + self.remaining)]
    }

    fn advance(&mut self, bytes: usize) {
        self.pos += bytes;
        self.remaining -= bytes;
    }

    fn skip(&mut self, bytes: usize) -> Option<()> {
        if bytes > self.remaining {
            return None;
        }
        let mut left = bytes;
        while left > 0 {
            let count = left.min(self.chunk().len());
            self.advance(count);
            left -= count;
        }
        Some(())
    }

    fn byte(&mut self) -> Option<u8> {
        let value = self.chunk().first().copied()?;
        self.advance(1);
        Some(value)
    }

    fn u16(&mut self) -> Option<usize> {
        let chunk = self.chunk();
        let value = if chunk.len() >= 2 {
            let value = u16::from_be_bytes([chunk[0], chunk[1]]);
            self.advance(2);
            value
        } else {
            u16::from_be_bytes([self.byte()?, self.byte()?])
        };
        Some(usize::from(value))
    }

    fn hostname(&mut self, bytes: usize) -> Option<String> {
        if bytes > self.remaining {
            return None;
        }
        let chunk = self.chunk();
        if chunk.len() >= bytes {
            let host = std::str::from_utf8(&chunk[..bytes])
                .ok()?
                .to_ascii_lowercase();
            self.advance(bytes);
            return Some(host);
        }
        // Only a hostname crossing records needs collection, never the whole
        // ClientHello or its padding. Reuse this allocation for the final String.
        let mut hostname = Vec::with_capacity(bytes);
        while hostname.len() < bytes {
            let chunk = self.chunk();
            let count = chunk.len().min(bytes - hostname.len());
            hostname.extend_from_slice(&chunk[..count]);
            self.advance(count);
        }
        let mut hostname = String::from_utf8(hostname).ok()?;
        hostname.make_ascii_lowercase();
        Some(hostname)
    }
}

fn parse_fragmented_hello_sni(mut hello: RecordPayload<'_>) -> Option<String> {
    hello.skip(34)?; // legacy_version and random
    let session_len = usize::from(hello.byte()?);
    hello.skip(session_len)?;
    let cipher_len = hello.u16()?;
    hello.skip(cipher_len)?;
    let compression_len = usize::from(hello.byte()?);
    hello.skip(compression_len)?;
    let mut extensions_left = hello.u16()?;
    if extensions_left > hello.remaining {
        return None;
    }
    while extensions_left >= 4 {
        let ext_type = hello.u16()?;
        let ext_len = hello.u16()?;
        extensions_left -= 4;
        if ext_len > extensions_left {
            return None;
        }
        if ext_type == 0 {
            if ext_len < 2 {
                return None;
            }
            let list_len = hello.u16()?;
            if list_len > ext_len - 2 {
                return None;
            }
            let mut name_pos = 2usize;
            while name_pos + 3 <= 2 + list_len {
                let name_type = hello.byte()?;
                let name_len = hello.u16()?;
                name_pos += 3;
                // Match the existing contiguous parser's extension bounds,
                // including its handling of a name extending past list_len.
                if name_len > ext_len - name_pos {
                    return None;
                }
                if name_type == 0 {
                    return hello.hostname(name_len);
                }
                hello.skip(name_len)?;
                name_pos += name_len;
            }
            hello.skip(ext_len - name_pos)?;
        } else {
            hello.skip(ext_len)?;
        }
        extensions_left -= ext_len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![22, 3, 3];
        bytes.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn handshake() -> Vec<u8> {
        handshake_with_host(b"SPLIT.EXAMPLE")
    }

    fn handshake_with_host(host: &[u8]) -> Vec<u8> {
        let mut body = vec![3, 3];
        body.extend_from_slice(&[0; 32]);
        body.extend_from_slice(&[0, 0, 2, 0x13, 1, 1, 0]);
        body.extend_from_slice(&((9 + host.len()) as u16).to_be_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&((5 + host.len()) as u16).to_be_bytes());
        body.extend_from_slice(&((3 + host.len()) as u16).to_be_bytes());
        body.push(0);
        body.extend_from_slice(&(host.len() as u16).to_be_bytes());
        body.extend_from_slice(host);
        let mut hello = vec![1, 0, 0, body.len() as u8];
        hello.extend_from_slice(&body);
        hello
    }

    #[test]
    fn every_supported_record_split_preserves_sni() {
        let hello = handshake();
        for split in 4..hello.len() {
            let mut wire = record(&hello[..split]);
            wire.extend_from_slice(&record(&hello[split..]));
            assert_eq!(
                parse_tls_client_hello_sni(&wire),
                ClientHelloParse::Found("split.example".into()),
                "record split {split}"
            );
        }
    }

    #[test]
    fn fragmented_utf8_empty_and_invalid_names_match_contiguous_parser() {
        for host in [&b""[..], "ÖEXAMPLE.COM".as_bytes(), &b"\xffINVALID"[..]] {
            let hello = handshake_with_host(host);
            let expected = parse_tls_client_hello_sni(&record(&hello));
            for split in 4..hello.len() {
                let mut wire = record(&hello[..split]);
                wire.extend_from_slice(&record(&[]));
                wire.extend_from_slice(&record(&hello[split..]));
                assert_eq!(parse_tls_client_hello_sni(&wire), expected, "split {split}");
            }
        }
    }

    #[test]
    fn one_payload_byte_per_record_after_header_preserves_sni() {
        let hello = handshake();
        let mut wire = record(&hello[..4]);
        for byte in &hello[4..] {
            wire.extend_from_slice(&record(&[]));
            wire.extend_from_slice(&record(std::slice::from_ref(byte)));
        }
        assert_eq!(
            parse_tls_client_hello_sni(&wire),
            ClientHelloParse::Found("split.example".into())
        );
    }

    #[test]
    fn fragmented_name_keeps_existing_server_name_list_boundary_behavior() {
        let mut hello = handshake();
        // The existing parser bounds a name by the extension, even when the
        // declared list ends after its three-byte name header. Preserve that
        // classification in this performance change rather than tightening it.
        hello[51..53].copy_from_slice(&3u16.to_be_bytes());
        let expected = parse_tls_client_hello_sni(&record(&hello));
        assert_eq!(expected, ClientHelloParse::Found("split.example".into()));
        for split in 4..hello.len() {
            let mut wire = record(&hello[..split]);
            wire.extend_from_slice(&record(&hello[split..]));
            assert_eq!(parse_tls_client_hello_sni(&wire), expected);
        }
    }

    #[test]
    fn incomplete_fragments_preserve_need_more() {
        let hello = handshake();
        let mut wire = record(&hello[..12]);
        wire.extend_from_slice(&record(&hello[12..]));
        for end in 0..wire.len() {
            assert_eq!(
                parse_tls_client_hello_sni(&wire[..end]),
                ClientHelloParse::NeedMore
            );
        }
    }

    #[test]
    fn declared_24_bit_length_does_not_imply_buffered_payload() {
        for length in [16_384u32, 1 << 20, 0xff_ffff] {
            let hello = [1, (length >> 16) as u8, (length >> 8) as u8, length as u8];
            assert_eq!(
                parse_tls_client_hello_sni(&record(&hello)),
                ClientHelloParse::NeedMore
            );
        }
    }

    #[test]
    fn unexpected_following_record_stays_invalid() {
        let hello = handshake();
        let mut wire = record(&hello[..12]);
        let mut second = record(&hello[12..]);
        second[0] = 23;
        wire.extend_from_slice(&second);
        assert_eq!(
            parse_tls_client_hello_sni(&wire),
            ClientHelloParse::NotClientHello
        );
    }

    #[test]
    fn trailing_handshake_bytes_do_not_change_first_sni() {
        let hello = handshake();
        let mut wire = record(&hello[..12]);
        let mut tail = hello[12..].to_vec();
        tail.extend_from_slice(&[0xa5; 8192]);
        wire.extend_from_slice(&record(&tail));
        assert_eq!(
            parse_tls_client_hello_sni(&wire),
            ClientHelloParse::Found("split.example".into())
        );
    }
}
