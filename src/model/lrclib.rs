//! A tiny [LRCLIB](https://lrclib.net) client.
//!
//! Lyrics are looked up from the song's tags — title, artist, album, and
//! duration. LRCLIB's `/api/get` is an exact-match endpoint; when it has no
//! entry (which is common for differently-tagged releases) we fall back to
//! `/api/search` and pick the result whose duration is closest.
//!
//! This uses `rustls` directly rather than an HTTP crate: a couple of GETs to
//! one host don't justify pulling a full async HTTP stack into the tree. The
//! request is blocking, so callers run it on the background executor.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, RootCertStore, StreamOwned};

use super::lyrics::{parse_lrc, Lyrics};
use super::song::SongMetadata;

const HOST: &str = "lrclib.net";
const PORT: u16 = 443;
const TIMEOUT: Duration = Duration::from_secs(12);

/// Look up lyrics for `song` on LRCLIB.
///
/// `Ok(None)` means the lookup succeeded but LRCLIB has no lyrics for this
/// track (or only an instrumental entry). An `Err` is a transport or protocol
/// failure, which the UI shows as a retryable message.
pub fn lookup(song: &SongMetadata) -> anyhow::Result<Option<Lyrics>> {
    let track = song.display_title();
    let artist = song.primary_artist();
    let duration = song.duration.as_secs().max(1);

    // Exact match first. Album is sent when the tag has one; LRCLIB treats it
    // as another matching constraint, not a required field.
    let mut path = format!(
        "/api/get?track_name={}&artist_name={}",
        encode(&track),
        encode(artist)
    );
    if let Some(album) = song.album_name.as_deref().filter(|album| !album.trim().is_empty()) {
        path.push_str(&format!("&album_name={}", encode(album)));
    }
    path.push_str(&format!("&duration={duration}"));

    let (status, body) = get_with_retry(&path)?;
    if status == 200 {
        if let Some(value) = serde_json::from_slice::<serde_json::Value>(&body).ok() {
            if let Some(lyrics) = lyrics_from_entry(&value) {
                return Ok(Some(lyrics));
            }
        }
    }
    // Remember a server-side failure from the exact endpoint. It doesn't stop
    // the search fallback: LRCLIB sometimes returns 503 for an exact-match
    // request (for example when the album tag doesn't line up) while the same
    // track is perfectly searchable.
    let exact_error = (status != 200).then(|| http_error("exact lookup", status, &body));

    // No exact entry: search and take the nearest-duration result. This is
    // what catches releases whose duration is rounded differently, or whose
    // album title isn't tagged.
    let path = format!(
        "/api/search?track_name={}&artist_name={}",
        encode(&track),
        encode(artist)
    );
    let (status, body) = get_with_retry(&path)?;
    if status != 200 {
        let search_error = http_error("search", status, &body);
        return Err(match exact_error {
            Some(exact) => anyhow!("{search_error:#}; {exact:#}"),
            None => search_error,
        });
    }
    let value: serde_json::Value =
        serde_json::from_slice(&body).context("invalid LRCLIB response")?;
    let Some(results) = value.as_array() else {
        return Ok(None);
    };

    let best = results
        .iter()
        .filter_map(|entry| {
            let candidate = lyrics_from_entry(entry)?;
            let candidate_duration =
                entry.get("duration").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
            let distance = (candidate_duration - duration as f64).abs();
            // Prefer a synced result over a plain one at the same distance.
            let synced_penalty = if matches!(candidate, Lyrics::Synced(_)) { 0.0 } else { 0.001 };
            Some((distance + synced_penalty, candidate))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0));

    // A result half a minute off is almost certainly a different recording
    // (a live version, say). Better to report "not found" than show them.
    Ok(best.filter(|(distance, _)| *distance <= 5.0).map(|(_, lyrics)| lyrics))
}

/// Pull lyrics out of one `/api/get` or `/api/search` entry. Synced lyrics win;
/// plain text is the fallback. Instrumental entries return `None`.
fn lyrics_from_entry(entry: &serde_json::Value) -> Option<Lyrics> {
    if entry.get("instrumental").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    if let Some(synced) = entry.get("syncedLyrics").and_then(serde_json::Value::as_str) {
        if !synced.trim().is_empty() {
            let lines = parse_lrc(synced);
            if !lines.is_empty() {
                return Some(Lyrics::Synced(lines));
            }
        }
    }
    if let Some(plain) = entry.get("plainLyrics").and_then(serde_json::Value::as_str) {
        if !plain.trim().is_empty() {
            return Some(Lyrics::Plain(plain.to_string()));
        }
    }
    None
}

/// GET with two short retries for transient 5xx responses. LRCLIB sits behind
/// a CDN that occasionally answers 503; a retry usually lands on a healthy
/// instance, and a persistent failure still falls through to the search path.
fn get_with_retry(path: &str) -> anyhow::Result<(u16, Vec<u8>)> {
    let mut response = get(path)?;
    for attempt in 0..2 {
        if response.0 < 500 {
            return Ok(response);
        }
        std::thread::sleep(Duration::from_millis(250 * (attempt as u64 + 1)));
        response = get(path)?;
    }
    Ok(response)
}

/// Turn a failed response into an error carrying a short piece of its body, so
/// a CDN block page or server error is diagnosable instead of just a number.
fn http_error(what: &str, status: u16, body: &[u8]) -> anyhow::Error {
    let body = String::from_utf8_lossy(body);
    let body = body.trim();
    let body: String = body.chars().take(200).collect();
    if body.is_empty() {
        anyhow!("LRCLIB {what} returned HTTP {status}")
    } else {
        anyhow!("LRCLIB {what} returned HTTP {status}: {body}")
    }
}

/// Issue one HTTPS GET on `HOST` and return the status and body.
fn get(path: &str) -> anyhow::Result<(u16, Vec<u8>)> {
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {HOST}\r\n\
         User-Agent: larkspur/0.0.1\r\n\
         Accept: application/json\r\n\
         Accept-Language: en-US,en;q=0.9\r\n\
         Accept-Encoding: identity\r\n\
         Connection: close\r\n\r\n"
    );

    let tcp = connect()?;
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(native.certs);
    if roots.is_empty() {
        return Err(anyhow!("no trusted TLS roots available"));
    }
    // Safe to ignore: another component may already have installed a provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Advertise HTTP/1.1 explicitly; some CDN edges are happier when ALPN is
    // present than when the client offers no protocol at all.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let server_name =
        ServerName::try_from(HOST.to_string()).map_err(|_| anyhow!("invalid TLS server name"))?;
    let connection = ClientConnection::new(Arc::new(config), server_name)?;
    let mut stream = StreamOwned::new(connection, tcp);
    stream.write_all(request.as_bytes()).context("sending LRCLIB request")?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).context("reading LRCLIB response")?;
    parse_response(&response)
}

/// Connect to LRCLIB, trying each resolved address.
fn connect() -> anyhow::Result<TcpStream> {
    let addresses = (HOST, PORT)
        .to_socket_addrs()
        .with_context(|| format!("resolving {HOST}"))?;
    let mut last = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(TIMEOUT))?;
                stream.set_write_timeout(Some(TIMEOUT))?;
                return Ok(stream);
            }
            Err(error) => last = Some(error),
        }
    }
    Err(last
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow!("could not resolve {HOST}"))
        .context("connecting to LRCLIB"))
}

/// Parse an HTTP/1.1 response into a status and a decoded body.
///
/// Handles both `Content-Length` and chunked responses and ignores any other
/// framing headers we don't ask for (`Accept-Encoding: identity` means the
/// body is uncompressed).
fn parse_response(raw: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let head = std::str::from_utf8(&raw[..split]).context("invalid HTTP headers")?;
    let body = &raw[split + 4..];

    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| anyhow!("missing HTTP status"))?;

    let mut chunked = false;
    let mut length = None;
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        }
    }

    let body = if chunked {
        decode_chunked(body)?
    } else if let Some(length) = length {
        body.get(..length).unwrap_or(body).to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// Decode an HTTP/1.1 chunked body.
fn decode_chunked(mut rest: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| anyhow!("malformed chunked body"))?;
        let size_text = std::str::from_utf8(&rest[..line_end]).context("invalid chunk size")?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).context("invalid chunk size")?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            break;
        }
        let chunk = rest.get(..size).ok_or_else(|| anyhow!("truncated chunk"))?;
        out.extend_from_slice(chunk);
        rest = rest.get(size..).unwrap_or_default();
        // Drop the CRLF after the chunk.
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    Ok(out)
}

/// Percent-encode a query value. Unreserved characters pass through; UTF-8 is
/// encoded byte by byte.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("AC/DC & friends"), "AC%2FDC%20%26%20friends");
        assert_eq!(encode("café"), "caf%C3%A9");
        assert_eq!(encode("safe-._~"), "safe-._~");
    }

    #[test]
    fn decodes_chunked_bodies() {
        let body = b"4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(body).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn parses_content_length_and_chunked_responses() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_response(raw).unwrap(), (200, b"{}".to_vec()));

        let raw = b"HTTP/1.1 404 Not Found\r\n\
                    Transfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), (404, b"{}".to_vec()));
    }

    #[test]
    fn synced_lyrics_win_over_plain() {
        let entry = serde_json::json!({
            "syncedLyrics": "[00:01.00]line one\n[00:02.00]line two",
            "plainLyrics": "line one\nline two",
        });
        match lyrics_from_entry(&entry).unwrap() {
            Lyrics::Synced(lines) => assert_eq!(lines.len(), 2),
            other => panic!("expected synced lyrics, got {other:?}"),
        }
    }

    #[test]
    fn plain_and_instrumental_entries() {
        let plain = serde_json::json!({ "plainLyrics": "a line" });
        assert_eq!(lyrics_from_entry(&plain), Some(Lyrics::Plain("a line".into())));

        let instrumental = serde_json::json!({ "instrumental": true, "plainLyrics": "ignored" });
        assert_eq!(lyrics_from_entry(&instrumental), None);
    }
}
