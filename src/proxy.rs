//! `[[proxy]]`: forwards requests under a path prefix to a backend.

use std::fmt;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{HOST, HeaderName, HeaderValue};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;

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
        if scheme != "http" {
            return Err(bad("only http is supported"));
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
        match self.try_forward(request).await {
            Ok(response) => response,
            Err(error) => {
                eprintln!("trug: proxy error: {error}");
                let mut response = Response::new(Body::from("bad gateway"));
                *response.status_mut() = StatusCode::BAD_GATEWAY;
                response
            }
        }
    }

    async fn try_forward(&self, request: Request) -> Result<Response, Box<dyn std::error::Error>> {
        let (parts, body) = request.into_parts();
        let url = self
            .target(parts.uri.path(), parts.uri.query())
            .ok_or("path outside the proxy mount")?;
        let body = axum::body::to_bytes(body, usize::MAX).await?;
        let mut headers = HeaderMap::new();
        for (name, value) in &parts.headers {
            if name == HOST {
                headers.insert(HeaderName::from_static("x-forwarded-host"), value.clone());
                headers.insert(
                    HeaderName::from_static("x-forwarded-proto"),
                    HeaderValue::from_static("http"),
                );
            } else if !HOP_BY_HOP.contains(&name.as_str()) {
                headers.append(name.clone(), value.clone());
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(backend: &str, rewrite: Option<&str>) -> Proxy {
        Proxy::new(&config::Proxy {
            backend: backend.into(),
            rewrite: rewrite.map(Into::into),
            no_redirect: false,
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
            })
            .is_err()
        };
        assert!(bad("http://h:1", None));
        assert!(bad("http://h:1/x", Some("api")));
        assert!(bad("https://h/x", None));
        assert!(bad("/x", None));
    }
}
