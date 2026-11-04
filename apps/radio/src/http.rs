//! Fetching a stream: just enough HTTP for an Icecast or Shoutcast server.
//!
//! One GET, over TCP or TLS, following redirects, and asking for the station's
//! titles in the stream (`Icy-MetaData: 1`). HTTP/1.0 on purpose: a server may not
//! answer it in chunks, so the body is the stream and nothing else. Shoutcast
//! answers `ICY 200 OK` rather than an HTTP status line, which is taken as the same
//! thing.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Per address tried. A station that cannot be reached in this long is not there.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A stream that stalls for this long has stopped.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 5;
/// The longest header block read before giving up on a server.
const MAX_HEADER: usize = 16 * 1024;

/// A stream's body, and how often a title block comes in it.
pub struct Response {
    pub body: BufReader<Box<dyn Read + Send>>,
    pub metaint: Option<usize>,
}

/// Where a URL points.
#[derive(Debug, PartialEq)]
struct Url {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse(url: &str) -> io::Result<Url> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("not a URL: {url}"));
    let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(bad());
    };
    let (authority, path) = rest.find('/').map_or((rest, "/"), |at| (&rest[..at], &rest[at..]));
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| bad())?),
        None => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(bad());
    }
    Ok(Url { tls, host: host.to_string(), port, path: path.to_string() })
}

/// Open `url`. `connected` is handed each TCP connection as it is made, so the
/// caller can shut it to stop a read that would otherwise wait for the server.
pub fn get(url: &str, connected: &dyn Fn(&TcpStream)) -> io::Result<Response> {
    let mut url = parse(url)?;
    for _ in 0..=MAX_REDIRECTS {
        let mut tcp = connect(&url.host, url.port)?;
        connected(&tcp);
        let request = format!(
            "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: flipctl-radio/1.0\r\nAccept: */*\r\nIcy-MetaData: 1\r\n\r\n",
            url.path, url.host
        );
        let stream: Box<dyn Read + Send> = if url.tls {
            let mut tls = tls(&url.host, tcp)?;
            tls.write_all(request.as_bytes())?;
            Box::new(tls)
        } else {
            tcp.write_all(request.as_bytes())?;
            Box::new(tcp)
        };
        let mut body = BufReader::new(stream);

        let head = headers(&mut body)?;
        match head.status {
            200 => return Ok(Response { body, metaint: head.metaint }),
            301 | 302 | 303 | 307 | 308 => {
                let to = head.location.ok_or_else(|| io::Error::other("redirect to nowhere"))?;
                url = if to.starts_with('/') { Url { path: to, ..url } } else { parse(&to)? };
            }
            status => return Err(io::Error::other(format!("HTTP {status}"))),
        }
    }
    Err(io::Error::other("too many redirects"))
}

fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut last = io::Error::other(format!("{host} has no address"));
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(tcp) => {
                tcp.set_read_timeout(Some(READ_TIMEOUT))?;
                tcp.set_write_timeout(Some(READ_TIMEOUT))?;
                return Ok(tcp);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn tls(host: &str, tcp: TcpStream) -> io::Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(config)
    });
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let conn = rustls::ClientConnection::new(Arc::clone(config), name).map_err(io::Error::other)?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}

/// What the header block says that matters here.
#[derive(Debug, Default, PartialEq)]
struct Head {
    status: u16,
    location: Option<String>,
    metaint: Option<usize>,
}

fn headers(r: &mut impl BufRead) -> io::Result<Head> {
    let mut head = Head::default();
    let mut read = 0;
    let mut first = true;
    loop {
        let mut line = Vec::new();
        let n = r.take((MAX_HEADER - read) as u64).read_until(b'\n', &mut line)?;
        read += n;
        if n == 0 || read >= MAX_HEADER {
            return Err(io::Error::other("no end to the headers"));
        }
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end();
        if first {
            // "HTTP/1.1 200 OK" or "ICY 200 OK".
            head.status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            first = false;
            continue;
        }
        if line.is_empty() {
            return Ok(head);
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "location" => head.location = Some(value.to_string()),
            "icy-metaint" => head.metaint = value.parse().ok().filter(|&n| n > 0),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_their_parts() {
        assert_eq!(
            parse("http://media-ice.musicradio.com/CapitalMP3").unwrap(),
            Url { tls: false, host: "media-ice.musicradio.com".into(), port: 80, path: "/CapitalMP3".into() }
        );
        assert_eq!(
            parse("https://quincy.torontocast.com:2070/stream.mp3").unwrap(),
            Url { tls: true, host: "quincy.torontocast.com".into(), port: 2070, path: "/stream.mp3".into() }
        );
        assert_eq!(
            parse("http://naxi128.streaming.rs:9150").unwrap(),
            Url { tls: false, host: "naxi128.streaming.rs".into(), port: 9150, path: "/".into() }
        );
        assert!(parse("ftp://example.com/").is_err());
        assert!(parse("http://:80/").is_err());
    }

    #[test]
    fn a_header_block_gives_its_status_and_interval() {
        let block = b"HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\nicy-metaint: 16000\r\n\r\nMP3DATA";
        let mut r = &block[..];
        assert_eq!(headers(&mut r).unwrap(), Head { status: 200, location: None, metaint: Some(16000) });
        assert_eq!(r, b"MP3DATA", "the body is left where it starts");
    }

    #[test]
    fn shoutcast_and_redirects_are_understood() {
        let mut r = &b"ICY 200 OK\r\nicy-name: Rage FM\r\n\r\n"[..];
        assert_eq!(headers(&mut r).unwrap().status, 200);
        let mut r = &b"HTTP/1.1 302 Found\r\nLocation: https://cdn.example/live\r\n\r\n"[..];
        let head = headers(&mut r).unwrap();
        assert_eq!((head.status, head.location.as_deref()), (302, Some("https://cdn.example/live")));
    }

    #[test]
    fn a_server_that_never_ends_its_headers_is_refused() {
        let mut r = &b"HTTP/1.0 200 OK\r\nicy-name: x\r\n"[..];
        assert!(headers(&mut r).is_err());
    }
}
