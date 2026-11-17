//! `trug serve`: serves the dist directory over HTTP.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use tokio::net::TcpListener;

#[derive(Debug)]
pub enum Error {
    Header(String),
    Address(String),
    Bind(SocketAddr, std::io::Error),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Header(name) => write!(f, "invalid header `{name}`"),
            Error::Address(address) => write!(f, "invalid address `{address}`"),
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
            }),
        })
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

async fn handle(State(site): State<Site>, request: Request) -> Response {
    let head = match *request.method() {
        Method::GET => false,
        Method::HEAD => true,
        _ => return response(StatusCode::METHOD_NOT_ALLOWED, &site, Body::empty()),
    };
    let Some(rel) = relative(request.uri().path()) else {
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
}
