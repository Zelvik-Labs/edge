mod quiche_tls {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;
    use zelvik_edge_h3_quiche::boring::{
        ssl::{Ssl, SslConnector, SslContext, SslMethod, SslStream, StatusType},
        x509::X509,
    };

    // Exercise the exact BoringSSL context callbacks over real TLS 1.3,
    // independently of QUIC packet scheduling. Hostname/trust verification
    // stays enabled; the supplied OCSP bytes test transport/expiry, not an
    // OCSP responder's signature validation.
    fn handshake(context: &SslContext, host: &str, sni: bool) -> (Vec<u8>, Option<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let context = context.clone();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = SslStream::new(Ssl::new(&context).unwrap(), socket).unwrap();
            stream.accept().unwrap();
            let mut request = [0; 1];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(request, [0x42]);
            stream.write_all(&[0x24]).unwrap();
        });
        let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
        for pem in [CERT_A_PEM, CERT_B_PEM] {
            connector
                .cert_store_mut()
                .add_cert(X509::from_pem(pem.as_bytes()).unwrap())
                .unwrap();
        }
        let connector = connector.build();
        let mut config = connector
            .configure()
            .unwrap()
            .use_server_name_indication(sni);
        config.set_status_type(StatusType::OCSP).unwrap();
        let socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut stream = config.connect(host, socket).unwrap();
        assert_eq!(stream.ssl().version_str(), "TLSv1.3");
        let leaf = stream.ssl().peer_certificate().unwrap().to_der().unwrap();
        let ocsp = stream.ssl().ocsp_status().map(<[u8]>::to_vec);
        stream.write_all(&[0x42]).unwrap();
        let mut response = [0; 1];
        stream.read_exact(&mut response).unwrap();
        assert_eq!(response, [0x24]);
        server.join().unwrap();
        (leaf, ocsp)
    }

    #[tokio::test]
    async fn quiche_context_keeps_sni_default_ocsp_and_hot_reload_atomic() {
        let selector = DynamicCertSelector::new();
        let a = cert(5101, CERT_A_PEM, KEY_A_PEM, &[], true);
        let b = cert(5102, CERT_B_PEM, KEY_B_PEM, &[], false);
        let site = server(
            5100,
            &["*.openrusty.org", "cat.com"],
            vec![a.clone(), b.clone()],
        );
        sync_certs_for_servers(
            &selector,
            &[a.clone(), b.clone()],
            std::slice::from_ref(&site),
        )
        .await;
        let (builder, _) = selector.build_quiche_ssl_builder().unwrap();
        let context = builder.build();
        let a_der = X509::from_pem(CERT_A_PEM.as_bytes())
            .unwrap()
            .to_der()
            .unwrap();
        let b_der = X509::from_pem(CERT_B_PEM.as_bytes())
            .unwrap()
            .to_der()
            .unwrap();
        assert_eq!(
            handshake(&context, "api.openrusty.org", true),
            (a_der.clone(), None)
        );
        assert_eq!(handshake(&context, "cat.com", true), (b_der.clone(), None));
        assert_eq!(
            handshake(&context, "api.openrusty.org", false),
            (a_der.clone(), None)
        );

        let staple = b"fixture opaque OCSP transport bytes".to_vec();
        selector
            .update_ocsp_with_expiry(a.id, staple.clone(), chrono::Utc::now().timestamp() + 60)
            .await
            .unwrap();
        assert_eq!(
            handshake(&context, "api.openrusty.org", true),
            (a_der.clone(), Some(staple))
        );
        selector
            .update_ocsp_with_expiry(a.id, vec![1, 2, 3], chrono::Utc::now().timestamp() - 1)
            .await
            .unwrap();
        assert_eq!(
            handshake(&context, "api.openrusty.org", true),
            (a_der.clone(), None)
        );

        let mut new_a = a;
        let mut new_b = b;
        new_a.is_default = false;
        new_b.is_default = true;
        let reloaded = server(
            5100,
            &["*.openrusty.org", "cat.com"],
            vec![new_a.clone(), new_b.clone()],
        );
        sync_certs_for_servers(&selector, &[new_a, new_b], &[reloaded]).await;
        // Reuse the original TLS context: no listener recreation may be needed.
        assert_eq!(handshake(&context, "cat.com", false), (b_der, None));
        assert_eq!(
            handshake(&context, "api.openrusty.org", true),
            (a_der, None)
        );
    }
}
