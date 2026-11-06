//! Opening an input for streaming reads.
//!
//! An input is either a local path or an `http(s)://` URL. Either may be gzip
//! compressed; we detect that from the first two bytes (`1f 8b`), never from
//! the file extension, because real MRFs are routinely served as `.json`
//! with gzip bodies (and vice versa).
//!
//! Everything here returns a boxed `Read`/`BufRead` trait object. That keeps
//! the call sites simple (one concrete type regardless of path/URL/gzip) at
//! the cost of one virtual call per `read`, which is negligible next to I/O
//! and parsing. The `+ Send` bound lets the CLI move a reader into a worker
//! thread.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::read::MultiGzDecoder;

/// Buffer size for readers we construct. Large enough to amortize syscalls on
/// multi-GB files; small enough not to matter for memory.
const BUF_SIZE: usize = 256 * 1024;

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// `User-Agent` sent on HTTP requests.
pub const USER_AGENT: &str = concat!("mrf-parse/", env!("CARGO_PKG_VERSION"));

/// Where an MRF comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Source {
    Path(PathBuf),
    Url(String),
}

impl Source {
    /// Interpret a CLI argument: anything starting with `http://` or
    /// `https://` (case-insensitive) is a URL, everything else is a path.
    pub fn parse(s: &str) -> Source {
        let lower = s.get(..8).unwrap_or(s).to_ascii_lowercase();
        if lower.starts_with("https://") || lower.starts_with("http://") {
            Source::Url(s.to_string())
        } else {
            Source::Path(PathBuf::from(s))
        }
    }

    /// The string recorded as `source_uri` in output tables.
    pub fn uri(&self) -> String {
        match self {
            Source::Path(p) => p.display().to_string(),
            Source::Url(u) => u.clone(),
        }
    }
}

impl From<&Path> for Source {
    fn from(p: &Path) -> Self {
        Source::Path(p.to_path_buf())
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.uri())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("cannot open {uri}: {source}")]
    Io {
        uri: String,
        #[source]
        source: io::Error,
    },
    #[error("HTTP request for {uri} failed: {message}")]
    Http { uri: String, message: String },
    #[error("cannot read zip archive {uri}: {message}")]
    Zip { uri: String, message: String },
}

/// Open the raw bytes of a source (no decompression).
///
/// For URLs this issues a blocking GET and returns the response body as a
/// reader; bytes are pulled from the socket as the caller reads, so nothing
/// is buffered in memory beyond the socket/BufReader buffers.
pub fn open_raw(src: &Source) -> Result<Box<dyn Read + Send>, InputError> {
    match src {
        Source::Path(p) => {
            let f = File::open(p).map_err(|e| InputError::Io {
                uri: src.uri(),
                source: e,
            })?;
            Ok(Box::new(f))
        }
        Source::Url(u) => {
            let http_err = |e: reqwest::Error| InputError::Http {
                uri: u.clone(),
                message: e.to_string(),
            };
            // No overall timeout: a 50GB download legitimately takes hours.
            // The connect timeout guards against dead hosts. (The blocking
            // client has no per-read timeout; a stalled body will hang.)
            let client = reqwest::blocking::Client::builder()
                // Some payer CDNs (e.g. the shared `*.mrf.bcbs.com` host used
                // by several Blue plans) answer 403 to requests without a
                // User-Agent. Identify as a generic client.
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(30))
                .timeout(None)
                .build()
                .map_err(http_err)?;
            let resp = client
                .get(u)
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(http_err)?;
            // `reqwest::blocking::Response` implements `Read`; it streams.
            Ok(Box::new(resp))
        }
    }
}

/// Open a source for reading its *decompressed* content.
pub fn open(src: &Source) -> Result<Box<dyn BufRead + Send>, InputError> {
    let raw = open_raw(src)?;
    maybe_decompress(raw).map_err(|e| InputError::Io {
        uri: src.uri(),
        source: e,
    })
}

/// Wrap `raw` in a gzip decoder if its first two bytes are the gzip magic.
///
/// We read the first (up to) two bytes ourselves rather than relying on
/// `BufRead::fill_buf`, because a single `read` may legally return just one
/// byte (e.g. an HTTP chunk boundary). The sniffed bytes are then glued back
/// in front of the stream with `Cursor::chain`, so no data is lost.
pub fn maybe_decompress<R: Read + Send + 'static>(
    mut raw: R,
) -> io::Result<Box<dyn BufRead + Send>> {
    let mut head = [0u8; 2];
    let mut n = 0;
    while n < head.len() {
        match raw.read(&mut head[n..]) {
            Ok(0) => break, // EOF: file shorter than 2 bytes
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let is_gzip = n == 2 && head == GZIP_MAGIC;
    let rejoined = Cursor::new(head[..n].to_vec()).chain(raw);
    if is_gzip {
        // MultiGzDecoder (not GzDecoder) so concatenated gzip members, which
        // some payers produce by appending chunks, decode as one stream.
        let decoder = MultiGzDecoder::new(BufReader::with_capacity(BUF_SIZE, rejoined));
        Ok(Box::new(BufReader::with_capacity(BUF_SIZE, decoder)))
    } else {
        Ok(Box::new(BufReader::with_capacity(BUF_SIZE, rejoined)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }

    fn read_all(src: &Source) -> Vec<u8> {
        let mut out = Vec::new();
        open(src).unwrap().read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn parse_source() {
        assert_eq!(
            Source::parse("https://x.org/a.json"),
            Source::Url("https://x.org/a.json".into())
        );
        assert_eq!(
            Source::parse("HTTP://x.org/a"),
            Source::Url("HTTP://x.org/a".into())
        );
        assert_eq!(Source::parse("a.json"), Source::Path("a.json".into()));
        assert_eq!(Source::parse("h"), Source::Path("h".into()));
    }

    #[test]
    fn plain_file_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.csv");
        std::fs::write(&p, b"hello,world\n").unwrap();
        assert_eq!(read_all(&Source::Path(p)), b"hello,world\n");
    }

    #[test]
    fn gzip_detected_by_magic_not_extension() {
        let dir = tempfile::tempdir().unwrap();
        let body = br#"{"in_network":[]}"#;
        for name in ["rates.json", "rates", "rates.gz"] {
            let p = dir.path().join(name);
            std::fs::write(&p, gzip(body)).unwrap();
            assert_eq!(read_all(&Source::Path(p)), body, "{name}");
        }
        // A `.gz` name with plain content is read as plain.
        let p = dir.path().join("plain.json.gz");
        std::fs::write(&p, body).unwrap();
        assert_eq!(read_all(&Source::Path(p)), body);
    }

    #[test]
    fn concatenated_gzip_members() {
        let mut bytes = gzip(b"abc");
        bytes.extend(gzip(b"def"));
        let r = maybe_decompress(Cursor::new(bytes)).unwrap();
        let s: String = io::read_to_string(r).unwrap();
        assert_eq!(s, "abcdef");
    }

    #[test]
    fn tiny_inputs() {
        for input in [&b""[..], b"x", b"\x1f"] {
            let r = maybe_decompress(Cursor::new(input.to_vec())).unwrap();
            let mut out = Vec::new();
            { r }.read_to_end(&mut out).unwrap();
            assert_eq!(out, input);
        }
    }

    /// A reader that returns one byte per `read` call, to prove sniffing does
    /// not depend on getting both magic bytes in one read.
    struct OneByte(Cursor<Vec<u8>>);
    impl Read for OneByte {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(1);
            self.0.read(&mut buf[..n])
        }
    }

    #[test]
    fn sniff_survives_short_reads() {
        let r = maybe_decompress(OneByte(Cursor::new(gzip(b"payload")))).unwrap();
        assert_eq!(io::read_to_string(r).unwrap(), "payload");
    }

    #[test]
    fn empty_file_opens_and_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.json");
        std::fs::write(&p, b"").unwrap();
        let src = Source::Path(p);
        assert!(read_all(&src).is_empty());
        // SHA-256 of the empty string.
        assert_eq!(
            crate::ids::sha256_hex_of(&src).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn missing_file_is_error() {
        let err = open(&Source::Path("/definitely/not/here.json".into()))
            .err()
            .unwrap();
        assert!(matches!(err, InputError::Io { .. }), "{err}");
    }

    #[test]
    fn http_plain_and_gzip() {
        let body = b"code|1,code|1|type\n".to_vec();
        let server = crate::testutil::TestServer::start(vec![
            ("/plain.csv".into(), body.clone()),
            ("/gz.json".into(), gzip(&body)),
        ]);
        assert_eq!(read_all(&Source::Url(server.url("/plain.csv"))), body);
        assert_eq!(read_all(&Source::Url(server.url("/gz.json"))), body);
        let err = open(&Source::Url(server.url("/missing"))).err().unwrap();
        assert!(matches!(err, InputError::Http { .. }), "{err}");
    }
}
