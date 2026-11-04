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

    let hello_storage;
    let hello = if first_record.len() >= needed_handshake_len {
        &first_record[4..needed_handshake_len]
    } else {
        // The 24-bit handshake length is untrusted. First establish that the
        // complete record payload is already buffered; incomplete peeks must
        // not reserve up to 16 MiB or recopy every previously received fragment.
        let mut available = 0usize;
        let mut pos = 0usize;
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
        // All ranges used below were validated above. Copy only the first
        // ClientHello, even if its last record carries additional handshakes.
        let mut handshake = Vec::with_capacity(needed_handshake_len);
        pos = 0;
        while handshake.len() < needed_handshake_len {
            let record_len = usize::from(u16::from_be_bytes([buf[pos + 3], buf[pos + 4]]));
            let take = record_len.min(needed_handshake_len - handshake.len());
            handshake.extend_from_slice(&buf[pos + 5..pos + 5 + take]);
            pos += 5 + record_len;
        }
        hello_storage = handshake;
        &hello_storage[4..needed_handshake_len]
    };
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
        let host = b"SPLIT.EXAMPLE";
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
