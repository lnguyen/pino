//! Decode an HTTP response body per its content-encoding, and pull the billed
//! model id out of it. Shared by the live proxy tap and the log backfill.
//! Mirrors src/http-decode.js.

use std::io::Read;
use std::sync::LazyLock;

use regex::Regex;

/// Decode a (possibly compressed) body to UTF-8. Returns "" when undecodable —
/// callers treat "" as "no usage". `encoding` is the content-encoding header.
pub fn decode_body(buf: &[u8], encoding: &str) -> String {
    let enc = encoding.trim().to_lowercase();
    let result: std::io::Result<Vec<u8>> = match enc.as_str() {
        "gzip" => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(buf).read_to_end(&mut out).map(|_| out)
        }
        "br" => {
            let mut out = Vec::new();
            brotli::Decompressor::new(buf, 4096)
                .read_to_end(&mut out)
                .map(|_| out)
        }
        "deflate" => {
            let mut out = Vec::new();
            // Node's zlib.inflateSync expects zlib-wrapped deflate.
            flate2::read::ZlibDecoder::new(buf).read_to_end(&mut out).map(|_| out)
        }
        "zstd" => zstd::stream::decode_all(buf),
        "" => return String::from_utf8_lossy(buf).into_owned(),
        _ => return String::new(),
    };
    match result {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

static RE_MODEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""model":"([^"]+)""#).unwrap());

/// The model the request was actually billed as (source of truth for pricing).
pub fn model_from_response(text: &str, fallback: &str) -> String {
    RE_MODEL
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| fallback.to_string())
}
