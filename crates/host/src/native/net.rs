//! Network for the native host: a `reqwest` client (rustls) behind `NetPolicy`.
//!
//! - **Proxy:** reqwest's system-proxy default honors `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY`
//!   (and their lower-case forms); [`NetConfig::system_proxy`] turns that off.
//! - **CA bundle:** [`NetConfig::from_env`] reads `GRIST_CA_BUNDLE`, else `SSL_CERT_FILE`; the
//!   PEM bundle at that path is added to the client's root certificates (on top of the built-in
//!   roots). A bundle that cannot be read or parsed makes `Host::network` fail with
//!   `HostError::Net` rather than silently connecting without it.
//! - **Allowlist:** the request URL's host is checked with `host[:port]` semantics
//!   ([`host_allowed`]) before any I/O.

use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use futures_util::StreamExt;
use kernel::{HostError, HttpMethod, HttpRequest, HttpResponse, NetAllow, NetHandle, PolicyError};

/// Client settings for `NativeHost::network`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetConfig {
    /// Extra root certificates (PEM bundle path).
    pub ca_bundle: Option<PathBuf>,
    /// Honor the proxy environment variables (`true`) or connect directly (`false`).
    pub system_proxy: bool,
    /// TCP connect timeout.
    pub connect_timeout: Duration,
}

impl Default for NetConfig {
    /// No extra CA bundle, system proxy on, 30 s connect timeout.
    fn default() -> Self {
        NetConfig {
            ca_bundle: None,
            system_proxy: true,
            connect_timeout: Duration::from_secs(30),
        }
    }
}

impl NetConfig {
    /// `Default` plus the CA bundle named by `GRIST_CA_BUNDLE`, else `SSL_CERT_FILE` (if set and
    /// non-empty).
    pub fn from_env() -> NetConfig {
        let ca_bundle = ["GRIST_CA_BUNDLE", "SSL_CERT_FILE"]
            .iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .find(|p| !p.as_os_str().is_empty());
        NetConfig {
            ca_bundle,
            ..NetConfig::default()
        }
    }

    /// Connect directly, ignoring proxy environment variables.
    pub fn without_proxy(mut self) -> NetConfig {
        self.system_proxy = false;
        self
    }

    /// Use this PEM bundle as extra roots.
    pub fn with_ca_bundle(mut self, path: impl Into<PathBuf>) -> NetConfig {
        self.ca_bundle = Some(path.into());
        self
    }
}

pub(crate) fn build_client(cfg: &NetConfig) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder()
        .use_rustls_tls()
        .connect_timeout(cfg.connect_timeout);
    if !cfg.system_proxy {
        b = b.no_proxy();
    }
    if let Some(path) = &cfg.ca_bundle {
        let pem = std::fs::read(path)
            .map_err(|e| format!("cannot read CA bundle `{}`: {e}", path.display()))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| format!("cannot parse CA bundle `{}`: {e}", path.display()))?;
        for c in certs {
            b = b.add_root_certificate(c);
        }
    }
    b.build()
        .map_err(|e| format!("cannot build http client: {e}"))
}

/// Lowercase, one trailing `.` stripped (the same normalization as capability hosts).
fn normalize_host(h: &str) -> String {
    h.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether a request to `host` (already lowercased) on `port` is allowed: anything under
/// `NetAllow::Any`; otherwise iff the bare host is in the set or `host:port` is.
pub fn host_allowed(allow: &NetAllow, host: &str, port: Option<u16>) -> bool {
    match allow {
        NetAllow::Any => true,
        NetAllow::Hosts(set) => {
            let host = normalize_host(host);
            set.contains(&host) || port.is_some_and(|p| set.contains(&format!("{host}:{p}")))
        }
    }
}

fn net_err(e: impl std::fmt::Display) -> HostError {
    HostError::Net(e.to_string())
}

/// A `NetHandle` over one `reqwest::Client` and one allowlist.
pub(crate) struct NativeNet {
    client: reqwest::Client,
    allow: NetAllow,
}

impl NativeNet {
    pub(crate) fn new(client: reqwest::Client, allow: NetAllow) -> NativeNet {
        NativeNet { client, allow }
    }
}

#[async_trait]
impl NetHandle for NativeNet {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HostError> {
        let url = reqwest::Url::parse(&req.url)
            .map_err(|e| HostError::Net(format!("invalid url `{}`: {e}", req.url)))?;
        let host = url
            .host_str()
            .map(normalize_host)
            .ok_or_else(|| HostError::Net(format!("url `{}` has no host", req.url)))?;
        if !host_allowed(&self.allow, &host, url.port_or_known_default()) {
            let shown = match url.port() {
                Some(p) => format!("{host}:{p}"),
                None => host,
            };
            return Err(HostError::Denied(PolicyError::NetDenied(shown)));
        }
        let method = match req.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Delete => reqwest::Method::DELETE,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Head => reqwest::Method::HEAD,
        };
        let mut r = self.client.request(method, url);
        for (k, v) in &req.headers {
            r = r.header(k.as_str(), v.as_str());
        }
        if !req.body.is_empty() {
            r = r.body(req.body);
        }
        if let Some(t) = req.timeout {
            r = r.timeout(t);
        }
        let resp = r.send().await.map_err(net_err)?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_owned(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body: Pin<Box<dyn Stream<Item = Result<Vec<u8>, HostError>> + Send>> = Box::pin(
            resp.bytes_stream()
                .map(|chunk| chunk.map(|b| b.to_vec()).map_err(net_err)),
        );
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}
