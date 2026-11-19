//! HTTPS trust: shlv trusts the system's CA bundle, here a test CA given as `SSL_CERT_FILE`, and
//! explains an unknown issuer. A local rustls server; no internet.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Serves one page over HTTPS with the test CA's localhost certificate; returns the port.
fn https_server() -> u16 {
    let certs: Vec<CertificateDer> = CertificateDer::pem_file_iter(fixture("server.pem"))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let key = PrivateKeyDer::from_pem_file(fixture("server.key")).unwrap();
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let config = config.clone();
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(config).unwrap();
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                let mut buf = [0u8; 4096];
                let mut req = Vec::new();
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let body = "<title>Over TLS</title>";
                let _ = write!(
                    tls,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.flush();
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    port
}

fn add_fetch(port: u16, ca: Option<&str>) -> (String, String) {
    let dir = std::env::temp_dir().join(format!(
        "shlv-tls-{}-{port}-{}",
        std::process::id(),
        ca.is_some()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut c = Command::new(env!("CARGO_BIN_EXE_shlv"));
    c.args(["add", &format!("https://localhost:{port}/"), "--fetch"])
        .env("SHLV_DB", dir.join("shlv.db"))
        .env("SHLV_CONFIG", dir.join("config.toml"))
        .env_remove("NIX_SSL_CERT_FILE");
    match ca {
        Some(ca) => c.env("SSL_CERT_FILE", ca),
        // A bundle without the test CA (the bundled roots would do the same).
        None => c.env("SSL_CERT_FILE", Path::new(&fixture("server.key"))),
    };
    let o = c.output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(o.status.success());
    (
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
    )
}

#[test]
fn trusts_the_system_bundle() {
    let port = https_server();
    let (out, err) = add_fetch(port, Some(&fixture("ca.pem")));
    assert!(out.contains("Over TLS"), "{out} {err}");
    assert!(err.is_empty(), "{err}");
}

#[test]
fn explains_an_unknown_issuer() {
    let port = https_server();
    let (out, err) = add_fetch(port, None);
    assert!(!out.contains("Over TLS"), "{out}");
    assert!(err.contains("UnknownIssuer"), "{err}");
    assert!(err.contains("point SSL_CERT_FILE at a bundle"), "{err}");
}
