//! `Host::network`: disabled policy → Denied; allowlist checked before any I/O; an allowed
//! request streams the body.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::host;
use futures_util::StreamExt;
use host::host_allowed;
use kernel::{Host, HostError, HttpMethod, HttpRequest, NetAllow, NetPolicy, PolicyError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn hosts(list: &[&str]) -> NetAllow {
    NetAllow::Hosts(list.iter().map(|s| (*s).to_owned()).collect())
}

fn req(url: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: url.to_owned(),
        headers: vec![("x-probe".to_owned(), "1".to_owned())],
        body: Vec::new(),
        timeout: Some(Duration::from_secs(5)),
    }
}

/// Serves one canned HTTP response, counting accepted connections.
async fn canned_server(body: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 4096];
            let mut n = 0;
            while !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf[n..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(k) => n += k,
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Served: yes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}/path"), accepted)
}

#[test]
fn disabled_policy_is_denied_without_a_request() {
    let policy = NetPolicy {
        enabled: false,
        allow: NetAllow::Any,
    };
    match host().network(&policy) {
        Err(HostError::Denied(PolicyError::NetDenied(h))) => assert_eq!(h, "*"),
        other => panic!("expected NetDenied(*), got {:?}", other.map(|_| ())),
    }
}

#[test]
fn host_allowed_semantics() {
    let set = hosts(&["api.example.com", "db.example.com:5432"]);
    assert!(host_allowed(&NetAllow::Any, "anything", None));
    assert!(host_allowed(&set, "api.example.com", None));
    assert!(host_allowed(&set, "api.example.com", Some(443)));
    assert!(host_allowed(&set, "API.example.com.", Some(8443)));
    assert!(host_allowed(&set, "db.example.com", Some(5432)));
    assert!(!host_allowed(&set, "db.example.com", Some(5433)));
    assert!(!host_allowed(&set, "db.example.com", None));
    assert!(!host_allowed(&set, "evil.example.com", Some(443)));
    assert!(!host_allowed(&hosts(&[]), "api.example.com", Some(443)));
}

#[tokio::test]
async fn non_allowed_host_is_denied_before_any_io() {
    let (url, accepted) = canned_server("never").await;
    let net = host()
        .network(&NetPolicy {
            enabled: true,
            allow: hosts(&["example.com"]),
        })
        .unwrap();
    match net.send(req(&url)).await {
        Err(HostError::Denied(PolicyError::NetDenied(h))) => {
            assert!(h.starts_with("127.0.0.1:"), "{h}");
        }
        other => panic!("expected NetDenied, got {:?}", other.map(|_| ())),
    }
    // A port-qualified entry for a different port is also denied.
    let net = host()
        .network(&NetPolicy {
            enabled: true,
            allow: hosts(&["127.0.0.1:1"]),
        })
        .unwrap();
    assert!(matches!(
        net.send(req(&url)).await,
        Err(HostError::Denied(PolicyError::NetDenied(_)))
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "no connection may be made"
    );
}

#[tokio::test]
async fn allowed_request_streams_the_body() {
    let (url, accepted) = canned_server("hello world").await;
    let net = host()
        .network(&NetPolicy {
            enabled: true,
            allow: hosts(&["127.0.0.1"]),
        })
        .unwrap();
    let resp = net.send(req(&url)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert!(
        resp.headers
            .iter()
            .any(|(k, v)| k == "x-served" && v == "yes"),
        "{:?}",
        resp.headers
    );
    let mut body = Vec::new();
    let mut stream = resp.body;
    while let Some(chunk) = stream.next().await {
        body.extend(chunk.unwrap());
    }
    assert_eq!(body, b"hello world");
    assert_eq!(accepted.load(Ordering::SeqCst), 1);

    // `NetAllow::Any` and a port-qualified entry also allow it.
    let port = url.split(':').nth(2).unwrap().split('/').next().unwrap();
    let with_port = format!("127.0.0.1:{port}");
    for allow in [NetAllow::Any, hosts(&[with_port.as_str()])] {
        let net = host()
            .network(&NetPolicy {
                enabled: true,
                allow,
            })
            .unwrap();
        assert_eq!(net.send(req(&url)).await.unwrap().status, 200);
    }
}

#[tokio::test]
async fn connection_failure_is_a_net_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let net = host()
        .network(&NetPolicy {
            enabled: true,
            allow: NetAllow::Any,
        })
        .unwrap();
    let r = net.send(req(&format!("http://{addr}/"))).await;
    assert!(matches!(r, Err(HostError::Net(_))), "{:?}", r.map(|_| ()));
    let r = net.send(req("not a url")).await;
    assert!(matches!(r, Err(HostError::Net(_))), "{:?}", r.map(|_| ()));
}

#[test]
fn missing_ca_bundle_fails_network_not_construction() {
    let h = host().with_net_config(
        host::NetConfig::default()
            .without_proxy()
            .with_ca_bundle("/definitely/missing/ca.pem"),
    );
    let r = h.network(&NetPolicy {
        enabled: true,
        allow: NetAllow::Any,
    });
    assert!(matches!(r, Err(HostError::Net(_))), "{:?}", r.map(|_| ()));
}

#[test]
fn net_config_default_and_env() {
    let d = host::NetConfig::default();
    assert!(d.system_proxy && d.ca_bundle.is_none());
    let e = host::NetConfig::from_env();
    let expect = std::env::var_os("GRIST_CA_BUNDLE")
        .or_else(|| std::env::var_os("SSL_CERT_FILE"))
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from);
    assert_eq!(e.ca_bundle, expect);
}
