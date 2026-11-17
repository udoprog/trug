//! `[[proxy]]`: forwards requests under a path prefix to a backend.

use std::fmt;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, Request};
use axum::http::header::{HOST, HeaderName, HeaderValue};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite as ts;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::config;

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Debug)]
pub enum Error {
    Backend(String, &'static str),
    Rewrite(String),
    Client(reqwest::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Backend(backend, why) => write!(f, "proxy backend `{backend}`: {why}"),
            Error::Rewrite(rewrite) => write!(f, "proxy rewrite `{rewrite}` must start with `/`"),
            Error::Client(error) => write!(f, "proxy client: {error}"),
        }
    }
}

impl std::error::Error for Error {}

pub struct Proxy {
    mount: String,
    ws: bool,
    scheme: String,
    authority: String,
    base: String,
    client: reqwest::Client,
}

impl Proxy {
    pub fn new(config: &config::Proxy) -> Result<Self, Error> {
        let bad = |why| Error::Backend(config.backend.clone(), why);
        let uri: Uri = config.backend.parse().map_err(|_| bad("not a URL"))?;
        let scheme = uri.scheme_str().ok_or_else(|| bad("missing scheme"))?;
        let expected = if config.ws { "ws" } else { "http" };
        if scheme != expected {
            return Err(bad(if config.ws {
                "a `ws` proxy needs a ws:// backend"
            } else {
                "only http is supported"
            }));
        }
        let authority = uri.authority().ok_or_else(|| bad("missing host"))?;
        let mount = match &config.rewrite {
            Some(rewrite) if !rewrite.starts_with('/') => {
                return Err(Error::Rewrite(rewrite.clone()));
            }
            Some(rewrite) => rewrite.as_str(),
            None => uri.path(),
        };
        let mount = mount.trim_end_matches('/');
        if mount.is_empty() {
            return Err(bad("needs a path, or a `rewrite` to mount at"));
        }
        let policy = if config.no_redirect {
            reqwest::redirect::Policy::none()
        } else {
            reqwest::redirect::Policy::limited(10)
        };
        let client = reqwest::Client::builder()
            .redirect(policy)
            .no_proxy()
            .build()
            .map_err(Error::Client)?;
        Ok(Self {
            mount: mount.to_owned(),
            ws: config.ws,
            scheme: scheme.to_owned(),
            authority: authority.to_string(),
            base: uri.path().trim_start_matches('/').to_owned(),
            client,
        })
    }

    /// The URL on the backend for a request path, if it is under the mount.
    pub fn target(&self, path: &str, query: Option<&str>) -> Option<String> {
        let rest = path.strip_prefix(self.mount.as_str())?;
        if !rest.is_empty() && !rest.starts_with('/') {
            return None;
        }
        let rest = rest.trim_start_matches('/');
        let separator = if !self.base.is_empty() && !rest.is_empty() && !self.base.ends_with('/') {
            "/"
        } else {
            ""
        };
        let query = query.map(|q| format!("?{q}")).unwrap_or_default();
        Some(format!(
            "{}://{}/{}{separator}{rest}{query}",
            self.scheme, self.authority, self.base
        ))
    }

    pub fn matches(&self, path: &str) -> bool {
        self.target(path, None).is_some()
    }

    pub async fn forward(&self, request: Request) -> Response {
        let result = if self.ws {
            self.try_upgrade(request).await
        } else {
            self.try_forward(request).await
        };
        match result {
            Ok(response) => response,
            Err(error) => {
                eprintln!("trug: proxy error: {error}");
                let mut response = Response::new(Body::from("bad gateway"));
                *response.status_mut() = StatusCode::BAD_GATEWAY;
                response
            }
        }
    }

    async fn try_upgrade(&self, request: Request) -> Result<Response, Box<dyn std::error::Error>> {
        let url = self
            .target(request.uri().path(), request.uri().query())
            .ok_or("path outside the proxy mount")?;
        let headers = request.headers().clone();
        let upgrade = WebSocketUpgrade::from_request(request, &())
            .await
            .map_err(|e| e.to_string())?;
        let mut handshake = url.into_client_request()?;
        for (name, value) in &forwarded(&headers) {
            if !name.as_str().starts_with("sec-websocket-") {
                handshake.headers_mut().append(name.clone(), value.clone());
            }
        }
        let backend = match tokio_tungstenite::connect_async(handshake).await {
            Ok((backend, _)) => backend,
            Err(ts::Error::Http(refusal)) => return Ok(refused(*refusal)),
            Err(error) => return Err(error.into()),
        };
        Ok(upgrade.on_upgrade(|client| pipe(client, backend)))
    }

    async fn try_forward(&self, request: Request) -> Result<Response, Box<dyn std::error::Error>> {
        let (parts, body) = request.into_parts();
        let url = self
            .target(parts.uri.path(), parts.uri.query())
            .ok_or("path outside the proxy mount")?;
        let body = axum::body::to_bytes(body, usize::MAX).await?;
        let mut headers = forwarded(&parts.headers);
        headers.insert(HOST, HeaderValue::try_from(self.authority.as_str())?);
        let upstream = self
            .client
            .request(parts.method, url)
            .headers(headers)
            .body(body)
            .send()
            .await?;
        let status = upstream.status();
        let upstream_headers = upstream.headers().clone();
        let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
        *response.status_mut() = status;
        for (name, value) in &upstream_headers {
            if !HOP_BY_HOP.contains(&name.as_str()) {
                response.headers_mut().append(name.clone(), value.clone());
            }
        }
        Ok(response)
    }
}

/// The client's headers for the backend: hop-by-hop ones dropped, `Host` moved
/// to `X-Forwarded-Host`.
fn forwarded(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if name == HOST {
            out.insert(HeaderName::from_static("x-forwarded-host"), value.clone());
            out.insert(
                HeaderName::from_static("x-forwarded-proto"),
                HeaderValue::from_static("http"),
            );
        } else if !HOP_BY_HOP.contains(&name.as_str()) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// A backend's refusal of the upgrade, passed through to the client.
fn refused(refusal: ts::http::Response<Option<Vec<u8>>>) -> Response {
    let (parts, body) = refusal.into_parts();
    let mut response = Response::new(Body::from(body.unwrap_or_default()));
    *response.status_mut() = parts.status;
    for (name, value) in &parts.headers {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    response
}

type Backend =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn pipe(client: WebSocket, backend: Backend) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut backend_tx, mut backend_rx) = backend.split();
    let up = async {
        while let Some(Ok(message)) = client_rx.next().await {
            if backend_tx.send(to_backend(message)).await.is_err() {
                break;
            }
        }
        let _ = backend_tx.close().await;
    };
    let down = async {
        while let Some(Ok(message)) = backend_rx.next().await {
            let Some(message) = to_client(message) else {
                continue;
            };
            if client_tx.send(message).await.is_err() {
                break;
            }
        }
        let _ = client_tx.close().await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

fn to_backend(message: Message) -> ts::Message {
    match message {
        Message::Text(text) => ts::Message::text(text.as_str()),
        Message::Binary(bytes) => ts::Message::Binary(bytes),
        Message::Ping(bytes) => ts::Message::Ping(bytes),
        Message::Pong(bytes) => ts::Message::Pong(bytes),
        Message::Close(frame) => ts::Message::Close(frame.map(|f| ts::protocol::CloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        })),
    }
}

fn to_client(message: ts::Message) -> Option<Message> {
    Some(match message {
        ts::Message::Text(text) => Message::Text(text.as_str().into()),
        ts::Message::Binary(bytes) => Message::Binary(bytes),
        ts::Message::Ping(bytes) => Message::Ping(bytes),
        ts::Message::Pong(bytes) => Message::Pong(bytes),
        ts::Message::Close(frame) => Message::Close(frame.map(|f| CloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        })),
        ts::Message::Frame(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(backend: &str, rewrite: Option<&str>) -> Proxy {
        Proxy::new(&config::Proxy {
            backend: backend.into(),
            rewrite: rewrite.map(Into::into),
            no_redirect: false,
            ws: false,
        })
        .unwrap()
    }

    #[test]
    fn target_uses_backend_path_as_prefix() {
        let p = proxy("http://127.0.0.1:8150/api/", None);
        assert_eq!(
            p.target("/api/users", Some("a=1")).unwrap(),
            "http://127.0.0.1:8150/api/users?a=1"
        );
        assert_eq!(
            p.target("/api", None).unwrap(),
            "http://127.0.0.1:8150/api/"
        );
        assert_eq!(
            p.target("/api/", None).unwrap(),
            "http://127.0.0.1:8150/api/"
        );
        assert_eq!(p.target("/apix", None), None);
        assert_eq!(p.target("/other", None), None);
    }

    #[test]
    fn target_without_trailing_slash_in_backend() {
        let p = proxy("http://h:1/api", None);
        assert_eq!(p.target("/api/a/b", None).unwrap(), "http://h:1/api/a/b");
        assert_eq!(p.target("/api", None).unwrap(), "http://h:1/api");
    }

    #[test]
    fn rewrite_replaces_the_prefix() {
        let p = proxy("http://h:1/", Some("/api/"));
        assert_eq!(p.target("/api/users", None).unwrap(), "http://h:1/users");
        let p = proxy("http://h:1/v2", Some("/api"));
        assert_eq!(p.target("/api/users", None).unwrap(), "http://h:1/v2/users");
        assert_eq!(p.target("/v2/users", None), None);
    }

    #[test]
    fn rejects_unusable_config() {
        let bad = |backend: &str, rewrite: Option<&str>| {
            Proxy::new(&config::Proxy {
                backend: backend.into(),
                rewrite: rewrite.map(Into::into),
                no_redirect: false,
                ws: false,
            })
            .is_err()
        };
        assert!(bad("http://h:1", None));
        assert!(bad("http://h:1/x", Some("api")));
        assert!(bad("https://h/x", None));
        assert!(bad("/x", None));
        assert!(bad("ws://h:1/x", None));
    }
}
