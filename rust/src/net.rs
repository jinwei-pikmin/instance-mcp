//! Minimal one-shot HTTP/1.1 client over plain TCP or rustls: one connection per request.
//! Enough for the two outbound calls the daemon makes — minting at an openab-pty admin
//! plane, and relaying to a loopback upstream MCP server — without an HTTP client crate.

use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::http::{HeaderMap, Uri};
use hyper_util::rt::TokioIo;

pub struct Response {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// POST `body` to `url` with `headers`. `Host` defaults to the URL's authority; pass one in
/// `headers` to override it (Playwright MCP's `--allowed-hosts` wants the bare host).
pub async fn post(url: &str, headers: &[(&str, String)], body: String) -> Result<Response, String> {
    let uri: Uri = url.parse().map_err(|e| format!("bad url: {e}"))?;
    let https = uri.scheme_str() == Some("https");
    let host = uri.host().ok_or("no host")?.to_string();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });

    let mut req = hyper::Request::post(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"));
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req = req.header("host", uri.authority().map(|a| a.as_str()).unwrap_or(&host));
    }
    for (k, v) in headers {
        req = req.header(*k, v.as_str());
    }
    let req = req
        .header("content-length", body.len().to_string())
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| e.to_string())?;

    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| e.to_string())?;

    async fn send<S>(io: S, req: hyper::Request<Full<Bytes>>) -> Result<Response, String>
    where
        S: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let (mut tx, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        let resp = tx.send_request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok(Response {
            status,
            headers,
            body: body.to_vec(),
        })
    }

    if https {
        use tokio_rustls::rustls;
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name =
            rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| e.to_string())?;
        send(TokioIo::new(tls), req).await
    } else {
        send(TokioIo::new(tcp), req).await
    }
}
