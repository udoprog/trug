//! `trug serve`: serves the dist directory over HTTP.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use crate::config;
use crate::proxy::Proxy;
use crate::report::Reporter;

/// Reserved by trug; handled before proxies and files.
pub const RELOAD_PATH: &str = "/_trug/reload";

/// Reloads on `reload`; on `build-error` shows the error over the page until the next reload
/// or a click.
const RELOAD_SCRIPT: &str = concat!(
    "<script>(()=>{const s=new EventSource(\"/_trug/reload\");",
    "s.onmessage=()=>location.reload();",
    "s.addEventListener(\"build-error\",e=>{",
    "let o=document.getElementById(\"trug-error\");",
    "if(!o){o=document.createElement(\"pre\");o.id=\"trug-error\";",
    "o.style.cssText=\"position:fixed;inset:0;z-index:2147483647;margin:0;padding:2em;",
    "overflow:auto;background:rgba(24,0,0,.92);color:#fdd;font:13px/1.5 monospace;",
    "white-space:pre-wrap;cursor:pointer\";",
    "o.title=\"Click to dismiss\";o.onclick=()=>o.remove();document.body.append(o)}",
    "o.textContent=\"trug: build failed; serving the last good build\\n\\n\"+e.data})})()",
    "</script>"
);

/// What the reload stream sends.
#[derive(Clone, Debug)]
enum Message {
    Reload,
    Error(Arc<str>),
}

#[derive(Debug)]
pub enum Error {
    Header(String),
    Address(String),
    Proxy(crate::proxy::Error),
    Bind(SocketAddr, std::io::Error),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Header(name) => write!(f, "invalid header `{name}`"),
            Error::Address(address) => write!(f, "invalid address `{address}`"),
            Error::Proxy(error) => error.fmt(f),
            Error::Bind(addr, error) => write!(f, "cannot bind {addr}: {error}"),
            Error::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

/// The dist directory is looked up on every request, so it may be replaced
/// by rename, or pointed elsewhere with `set_dist`, while serving.
#[derive(Clone)]
pub struct Site {
    inner: Arc<Inner>,
}

struct Inner {
    dist: RwLock<PathBuf>,
    headers: HeaderMap,
    proxies: Vec<Proxy>,
    reload: Option<broadcast::Sender<Message>>,
    /// The failed build pages are told about when they connect, until the next good one.
    error: RwLock<Option<Arc<str>>>,
    public_prefix: Option<String>,
    reporter: Reporter,
}

impl Site {
    pub fn new(dist: PathBuf, headers: &BTreeMap<String, String>) -> Result<Self, Error> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let invalid = || Error::Header(name.clone());
            map.insert(
                HeaderName::try_from(name.as_str()).map_err(|_| invalid())?,
                HeaderValue::try_from(value.as_str()).map_err(|_| invalid())?,
            );
        }
        Ok(Self {
            inner: Arc::new(Inner {
                dist: RwLock::new(dist),
                headers: map,
                proxies: Vec::new(),
                reload: None,
                error: RwLock::new(None),
                public_prefix: None,
                reporter: Reporter::default(),
            }),
        })
    }

    /// Must be called before the site is cloned.
    pub fn with_proxies(mut self, proxies: &[config::Proxy]) -> Result<Self, Error> {
        let proxies = proxies
            .iter()
            .map(Proxy::new)
            .collect::<Result<_, _>>()
            .map_err(Error::Proxy)?;
        Arc::get_mut(&mut self.inner)
            .expect("site not yet shared")
            .proxies = proxies;
        Ok(self)
    }

    /// Serves files requested under the build's public URL path (`/app/`) from
    /// the dist root. Must be called before the site is cloned.
    pub fn with_public_url(mut self, public_url: &str) -> Self {
        let path = public_url.strip_prefix('/').map(|_| public_url);
        Arc::get_mut(&mut self.inner)
            .expect("site not yet shared")
            .public_prefix = path.filter(|p| *p != "/").map(str::to_owned);
        self
    }

    /// Serves `RELOAD_PATH` and injects the reload script into index pages.
    /// Must be called before the site is cloned.
    pub fn with_reload(mut self) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("site not yet shared")
            .reload = Some(broadcast::channel(16).0);
        self
    }

    /// Where proxy errors go; silent by default. Must be called before the site is cloned.
    pub fn with_reporter(mut self, reporter: Reporter) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("site not yet shared")
            .reporter = reporter;
        self
    }

    /// Tells connected pages to reload, which clears an error overlay; call after a successful
    /// swap. Returns how many pages were told.
    pub fn notify_reload(&self) -> usize {
        *self.inner.error.write().unwrap() = None;
        self.send(Message::Reload)
    }

    /// Shows `error` over connected pages, and pages that connect later, until the next
    /// [`Site::notify_reload`]. Returns how many pages were told.
    pub fn notify_error(&self, error: &str) -> usize {
        let error: Arc<str> = error.into();
        *self.inner.error.write().unwrap() = Some(error.clone());
        self.send(Message::Error(error))
    }

    fn send(&self, message: Message) -> usize {
        self.inner
            .reload
            .as_ref()
            .and_then(|tx| tx.send(message).ok())
            .unwrap_or(0)
    }

    pub fn set_dist(&self, dist: PathBuf) {
        *self.inner.dist.write().unwrap() = dist;
    }

    pub fn router(&self) -> Router {
        Router::new().fallback(handle).with_state(self.clone())
    }

    pub async fn serve(&self, listeners: Vec<TcpListener>) -> Result<(), Error> {
        let mut tasks = tokio::task::JoinSet::new();
        for listener in listeners {
            let router = self.router();
            tasks.spawn(async move { axum::serve(listener, router).await });
        }
        while let Some(result) = tasks.join_next().await {
            result
                .map_err(std::io::Error::other)
                .map_err(Error::Io)?
                .map_err(Error::Io)?;
        }
        Ok(())
    }
}

pub async fn bind(addresses: &[String], port: u16) -> Result<Vec<TcpListener>, Error> {
    let mut listeners = Vec::new();
    for address in addresses {
        let ip: IpAddr = address
            .parse()
            .map_err(|_| Error::Address(address.clone()))?;
        let addr = SocketAddr::new(ip, port);
        listeners.push(
            TcpListener::bind(addr)
                .await
                .map_err(|e| Error::Bind(addr, e))?,
        );
    }
    Ok(listeners)
}

fn response(status: StatusCode, site: &Site, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().extend(
        site.inner
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    response
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

fn relative(uri_path: &str) -> Option<PathBuf> {
    let bytes = uri_path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    let mut out = PathBuf::new();
    for part in decoded.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if part == ".." || part.contains(['\\', '\0']) {
            return None;
        }
        out.push(part);
    }
    debug_assert!(out.components().all(|c| matches!(c, Component::Normal(_))));
    Some(out)
}

async fn read_file(path: &Path) -> Option<Vec<u8>> {
    tokio::fs::read(path).await.ok()
}

fn reload_events(tx: &broadcast::Sender<Message>, error: Option<Arc<str>>) -> Body {
    let rx = tx.subscribe();
    let mut first = String::from(": connected\n\n");
    if let Some(error) = error {
        first.push_str(&error_event(&error));
    }
    let first =
        futures_util::stream::once(async { Ok::<_, std::convert::Infallible>(Bytes::from(first)) });
    let events = futures_util::stream::unfold(rx, |mut rx| async move {
        let message = match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
            Err(_) => Bytes::from_static(b": keep-alive\n\n"),
            Ok(Ok(Message::Reload)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                Bytes::from_static(b"data: reload\n\n")
            }
            Ok(Ok(Message::Error(error))) => Bytes::from(error_event(&error)),
            Ok(Err(broadcast::error::RecvError::Closed)) => return None,
        };
        Some((Ok(message), rx))
    });
    Body::from_stream(futures_util::StreamExt::chain(first, events))
}

/// A `build-error` event, one `data:` line per line of `error`.
fn error_event(error: &str) -> String {
    let mut event = String::from("event: build-error\n");
    for line in error.lines() {
        event.push_str("data: ");
        event.push_str(line.trim_end_matches('\r'));
        event.push('\n');
    }
    if error.is_empty() {
        event.push_str("data: \n");
    }
    event.push('\n');
    event
}

fn inject_script(html: Vec<u8>) -> Vec<u8> {
    let script = RELOAD_SCRIPT.as_bytes();
    let at = html
        .windows(7)
        .rposition(|w| w.eq_ignore_ascii_case(b"</body>"))
        .unwrap_or(html.len());
    let mut out = Vec::with_capacity(html.len() + script.len());
    out.extend_from_slice(&html[..at]);
    out.extend_from_slice(script);
    out.extend_from_slice(&html[at..]);
    out
}

async fn handle(State(site): State<Site>, request: Request) -> Response {
    let path = request.uri().path();
    if path == RELOAD_PATH || path.starts_with("/_trug/") {
        return match (&site.inner.reload, path == RELOAD_PATH) {
            (Some(tx), true) => {
                let error = site.inner.error.read().unwrap().clone();
                let mut response = response(StatusCode::OK, &site, reload_events(tx, error));
                let headers = response.headers_mut();
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
                headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                response
            }
            _ => response(StatusCode::NOT_FOUND, &site, Body::from("not found")),
        };
    }
    if let Some(proxy) = site
        .inner
        .proxies
        .iter()
        .find(|p| p.matches(request.uri().path()))
    {
        return proxy.forward(request, &site.inner.reporter).await;
    }
    let head = match *request.method() {
        Method::GET => false,
        Method::HEAD => true,
        _ => return response(StatusCode::METHOD_NOT_ALLOWED, &site, Body::empty()),
    };
    let uri_path = request.uri().path();
    let uri_path = match &site.inner.public_prefix {
        Some(prefix) => match uri_path.strip_prefix(prefix.as_str()) {
            Some(rest) => &uri_path[uri_path.len() - rest.len() - 1..],
            None if uri_path == prefix.trim_end_matches('/') => "/",
            None => uri_path,
        },
        None => uri_path,
    };
    let Some(rel) = relative(uri_path) else {
        return response(StatusCode::BAD_REQUEST, &site, Body::empty());
    };
    let dist = site.inner.dist.read().unwrap().clone();
    let mut path = dist.join(&rel);
    if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
        path.push("index.html");
    }
    let mut found = read_file(&path).await.map(|data| (path.clone(), data));
    if found.is_none() && rel.extension().is_none() {
        let index = dist.join("index.html");
        found = read_file(&index).await.map(|data| (index, data));
    }
    let Some((path, data)) = found else {
        return response(StatusCode::NOT_FOUND, &site, Body::from("not found"));
    };
    let data = if site.inner.reload.is_some() && content_type(&path).starts_with("text/html") {
        inject_script(data)
    } else {
        data
    };
    let len = data.len();
    let body = if head {
        Body::empty()
    } else {
        Body::from(data)
    };
    let mut response = response(StatusCode::OK, &site, body);
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type(&path)));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_rejects_traversal() {
        assert_eq!(relative("/a/b.js"), Some(PathBuf::from("a/b.js")));
        assert_eq!(relative("/a%20b"), Some(PathBuf::from("a b")));
        assert_eq!(relative("/"), Some(PathBuf::new()));
        assert_eq!(relative("/../x"), None);
        assert_eq!(relative("/%2e%2e/x"), None);
        assert_eq!(relative("/a%2Fb"), Some(PathBuf::from("a/b")));
    }

    #[test]
    fn error_events_carry_each_line() {
        assert_eq!(
            error_event("a\r\n  b"),
            "event: build-error\ndata: a\ndata:   b\n\n"
        );
        assert_eq!(error_event(""), "event: build-error\ndata: \n\n");
    }

    #[test]
    fn injects_before_body_end() {
        let out = inject_script(b"<body>x</BODY>".to_vec());
        assert_eq!(out, format!("<body>x{RELOAD_SCRIPT}</BODY>").into_bytes());
        assert!(inject_script(b"<p>".to_vec()).ends_with(RELOAD_SCRIPT.as_bytes()));
    }
}
