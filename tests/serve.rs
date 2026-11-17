use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

use trug::serve::{Site, bind};

fn start(dist: &std::path::Path) -> (SocketAddr, Site) {
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
    let headers = BTreeMap::from([("Cache-Control".to_string(), "no-cache".to_string())]);
    let site = Site::new(dist.to_owned(), &headers).unwrap();
    let listeners = rt.block_on(bind(&["127.0.0.1".to_string()], 0)).unwrap();
    let addr = listeners[0].local_addr().unwrap();
    let serving = site.clone();
    rt.spawn(async move { serving.serve(listeners).await });
    (addr, site)
}

fn request(addr: SocketAddr, method: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    out
}

fn dist(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("trug-serve-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("index.html"), "<p>index</p>").unwrap();
    std::fs::write(dir.join("app.js"), "let a;").unwrap();
    std::fs::write(dir.join("sub/index.html"), "<p>sub</p>").unwrap();
    dir
}

#[test]
fn serves_files_with_headers_and_types() {
    let dir = dist("files");
    let (addr, _site) = start(&dir);
    let res = request(addr, "GET", "/app.js");
    assert!(res.starts_with("HTTP/1.1 200"), "{res}");
    assert!(
        res.to_lowercase().contains("cache-control: no-cache"),
        "{res}"
    );
    assert!(res.contains("content-type: text/javascript"), "{res}");
    assert!(res.ends_with("let a;"), "{res}");
    assert!(request(addr, "GET", "/sub/").ends_with("<p>sub</p>"));
    let head = request(addr, "HEAD", "/app.js");
    assert!(
        head.contains("content-length: 6") && !head.ends_with("let a;"),
        "{head}"
    );
    assert!(request(addr, "POST", "/").starts_with("HTTP/1.1 405"));
}

#[test]
fn spa_fallback_and_missing_files() {
    let dir = dist("spa");
    let (addr, _site) = start(&dir);
    let res = request(addr, "GET", "/users/42");
    assert!(
        res.starts_with("HTTP/1.1 200") && res.ends_with("<p>index</p>"),
        "{res}"
    );
    assert!(res.contains("text/html"), "{res}");
    let res = request(addr, "GET", "/missing.js");
    assert!(res.starts_with("HTTP/1.1 404"), "{res}");
    assert!(
        res.to_lowercase().contains("cache-control: no-cache"),
        "{res}"
    );
    assert!(request(addr, "GET", "/%2e%2e/x.toml").starts_with("HTTP/1.1 400"));
}

#[test]
fn follows_dist_replaced_by_rename() {
    let dir = dist("swap");
    let (addr, site) = start(&dir);
    assert!(request(addr, "GET", "/").ends_with("<p>index</p>"));
    let new = dir.with_extension("new");
    std::fs::create_dir_all(&new).unwrap();
    std::fs::write(new.join("index.html"), "<p>new</p>").unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::rename(&new, &dir).unwrap();
    assert!(request(addr, "GET", "/").ends_with("<p>new</p>"));
    let other = dist("other");
    std::fs::write(other.join("index.html"), "<p>other</p>").unwrap();
    site.set_dist(other);
    assert!(request(addr, "GET", "/").ends_with("<p>other</p>"));
}

mod proxy {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};

    use trug::config::Proxy;
    use trug::serve::{Site, bind};

    /// Replies 200 echoing the request line and Host header; `/redir` gives a 302 to `/landed`.
    fn backend() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut host = String::new();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("host:") {
                        host = v.trim().to_string();
                    }
                }
                let target = line.split(' ').nth(1).unwrap().to_string();
                let reply = if target.ends_with("/redir") {
                    "HTTP/1.1 302 Found\r\nLocation: /landed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else if target == "/landed" {
                    "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nlanded"
                        .to_string()
                } else {
                    let body = format!("{target} host={host}");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        addr
    }

    fn start(proxies: &[Proxy]) -> SocketAddr {
        let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("trug-proxy-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<p>index</p>").unwrap();
        let site = Site::new(dir, &Default::default())
            .unwrap()
            .with_proxies(proxies)
            .unwrap();
        let listeners = rt.block_on(bind(&["127.0.0.1".to_string()], 0)).unwrap();
        let addr = listeners[0].local_addr().unwrap();
        rt.spawn(async move { site.serve(listeners).await });
        addr
    }

    fn get(addr: SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: front\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    }

    fn proxy(backend: String, rewrite: Option<&str>, no_redirect: bool) -> Proxy {
        Proxy {
            backend,
            rewrite: rewrite.map(Into::into),
            no_redirect,
            ws: false,
        }
    }

    #[test]
    fn backend_path_is_the_prefix() {
        let b = backend();
        let addr = start(&[proxy(format!("http://{b}/api/"), None, false)]);
        let out = get(addr, "/api/users?x=1");
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.ends_with(&format!("/api/users?x=1 host={b}")), "{out}");
        assert!(get(addr, "/").ends_with("<p>index</p>"));
    }

    #[test]
    fn rewrite_replaces_the_prefix() {
        let b = backend();
        let addr = start(&[proxy(format!("http://{b}/v2/"), Some("/api/"), false)]);
        assert!(get(addr, "/api/users").contains("\r\n\r\n/v2/users host="));
        assert!(get(addr, "/v2/users").ends_with("<p>index</p>"));
    }

    #[test]
    fn redirects_are_followed_unless_no_redirect() {
        let b = backend();
        let addr = start(&[
            proxy(format!("http://{b}/follow/"), None, false),
            proxy(format!("http://{b}/keep/"), None, true),
        ]);
        assert!(get(addr, "/follow/redir").ends_with("landed"));
        let out = get(addr, "/keep/redir");
        assert!(out.starts_with("HTTP/1.1 302"), "{out}");
        assert!(out.to_lowercase().contains("location: /landed"), "{out}");
    }

    #[test]
    fn unreachable_backend_is_bad_gateway() {
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let b = free.local_addr().unwrap();
        drop(free);
        let addr = start(&[proxy(format!("http://{b}/api/"), None, false)]);
        assert!(get(addr, "/api/x").starts_with("HTTP/1.1 502"));
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn websocket_round_trip() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let std_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let b = std_listener.local_addr().unwrap();
        let mut p = proxy(format!("ws://{b}/ws"), Some("/socket"), false);
        p.ws = true;
        let addr = start(&[p]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |req: &tokio_tungstenite::tungstenite::handshake::server::Request, res| {
                        assert_eq!(req.uri().path(), "/ws");
                        assert_eq!(req.uri().query(), Some("a=1"));
                        Ok(res)
                    },
                )
                .await
                .unwrap();
                while let Some(Ok(message)) = ws.next().await {
                    if message.is_text() || message.is_binary() {
                        ws.send(message).await.unwrap();
                    }
                }
            });
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/socket?a=1"))
                .await
                .unwrap();
            ws.send(Message::text("hello")).await.unwrap();
            assert_eq!(ws.next().await.unwrap().unwrap(), Message::text("hello"));
            ws.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
            assert_eq!(
                ws.next().await.unwrap().unwrap(),
                Message::binary(vec![1u8, 2, 3])
            );
            ws.close(None).await.unwrap();
            assert!(
                ws.next()
                    .await
                    .is_none_or(|m| m.is_err() || m.unwrap().is_close())
            );
        });
    }

    #[test]
    fn websocket_backend_down_is_bad_gateway() {
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let b = free.local_addr().unwrap();
        drop(free);
        let mut p = proxy(format!("ws://{b}/ws"), None, false);
        p.ws = true;
        let addr = start(&[p]);
        let out = get(addr, "/ws");
        assert!(out.starts_with("HTTP/1.1 502"), "{out}");
    }
}

#[test]
fn reload_events_follow_notify_and_inject_script() {
    let dir = dist("reload");
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
    let site = Site::new(dir.clone(), &BTreeMap::new())
        .unwrap()
        .with_reload();
    let listeners = rt.block_on(bind(&["127.0.0.1".to_string()], 0)).unwrap();
    let addr = listeners[0].local_addr().unwrap();
    let serving = site.clone();
    rt.spawn(async move { serving.serve(listeners).await });

    let page = request(addr, "GET", "/");
    assert!(page.contains("/_trug/reload"), "{page}");
    assert!(
        !std::fs::read_to_string(dir.join("index.html"))
            .unwrap()
            .contains("_trug")
    );
    assert!(request(addr, "GET", "/_trug/other").starts_with("HTTP/1.1 404"));

    let mut stream = TcpStream::connect(addr).unwrap();
    write!(stream, "GET /_trug/reload HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    let mut seen = String::new();
    let mut buf = [0u8; 1024];
    let mut read = |seen: &mut String| {
        if let Ok(n) = stream.read(&mut buf) {
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    };
    while !seen.contains(": connected") {
        read(&mut seen);
    }
    assert!(seen.contains("text/event-stream"), "{seen}");
    read(&mut seen);
    assert!(!seen.contains("data: reload"), "{seen}");
    site.notify_reload();
    while !seen.contains("data: reload") {
        read(&mut seen);
    }
}

#[test]
fn plain_site_has_no_reload() {
    let dir = dist("noreload");
    let (addr, _site) = start(&dir);
    assert!(request(addr, "GET", "/_trug/reload").starts_with("HTTP/1.1 404"));
    assert!(request(addr, "GET", "/").ends_with("<p>index</p>"));
}

#[test]
fn serves_dist_under_the_public_url() {
    let dir = dist("public-url");
    let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
    let site = Site::new(dir, &BTreeMap::new())
        .unwrap()
        .with_public_url("/app/");
    let listeners = rt.block_on(bind(&["127.0.0.1".to_string()], 0)).unwrap();
    let addr = listeners[0].local_addr().unwrap();
    rt.spawn(async move { site.serve(listeners).await });

    assert!(request(addr, "GET", "/app/app.js").ends_with("let a;"));
    assert!(request(addr, "GET", "/app/").ends_with("<p>index</p>"));
    assert!(request(addr, "GET", "/app").ends_with("<p>index</p>"));
    assert!(request(addr, "GET", "/app/sub/").ends_with("<p>sub</p>"));
    assert!(request(addr, "GET", "/app.js").ends_with("let a;"));
}
