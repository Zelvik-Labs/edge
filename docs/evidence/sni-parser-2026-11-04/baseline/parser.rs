#[derive(Debug)]
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
        let mut handshake = Vec::with_capacity(needed_handshake_len);
        let mut pos = 0usize;
        while pos + 5 <= buf.len() && handshake.len() < needed_handshake_len {
            if buf[pos] != 22 {
                return ClientHelloParse::NotClientHello;
            }
            let record_len = usize::from(u16::from_be_bytes([buf[pos + 3], buf[pos + 4]]));
            if buf.len() < pos + 5 + record_len {
                return ClientHelloParse::NeedMore;
            }
            handshake.extend_from_slice(&buf[pos + 5..pos + 5 + record_len]);
            pos += 5 + record_len;
        }
        if handshake.len() < needed_handshake_len {
            return ClientHelloParse::NeedMore;
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
