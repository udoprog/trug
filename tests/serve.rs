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
