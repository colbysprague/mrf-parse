//! Stable identifiers.
//!
//! * `file_id` = first 16 hex chars of the SHA-256 of the input's *raw* bytes
//!   (i.e. the compressed bytes if the file is gzipped), so it matches what
//!   `sha256sum` reports for the file on disk.
//! * `stable_id` hashes a sequence of fields into a short deterministic id.
//!   Used for item ids and synthetic provider-group ids so reruns produce
//!   identical output.

use std::io::{self, Read};

use sha2::{Digest, Sha256};

use crate::input::{open_raw, InputError, Source};

/// Length of `file_id` in hex characters (64 bits).
pub const FILE_ID_LEN: usize = 16;

/// `file_id` from a full hex SHA-256 digest.
pub fn file_id_from_sha256(sha256_hex: &str) -> String {
    sha256_hex[..FILE_ID_LEN].to_string()
}

/// Stream a source's raw bytes through SHA-256 and return the lowercase hex
/// digest. Constant memory regardless of input size.
pub fn sha256_hex_of(src: &Source) -> Result<String, InputError> {
    let raw = open_raw(src)?;
    let mut hashing = HashingReader::new(raw);
    io::copy(&mut hashing, &mut io::sink()).map_err(|e| InputError::Io {
        uri: src.uri(),
        source: e,
    })?;
    Ok(hashing.finish_hex())
}

/// A `Read` adapter that hashes every byte that passes through it.
///
/// Wrap the *raw* reader with this (before gzip decoding) to compute the
/// file's SHA-256 in the same pass as parsing, e.g. for URL inputs where a
/// second download would be expensive.
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    /// Number of bytes read so far.
    pub fn bytes_read(&self) -> u64 {
        self.bytes
    }

    /// Consume the adapter and return the hex digest of everything read.
    pub fn finish_hex(self) -> String {
        hex::encode(self.hasher.finalize())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

/// Deterministic 16-hex-char id from a sequence of fields.
///
/// Each field is length-prefixed before hashing so that `["ab", "c"]` and
/// `["a", "bc"]` hash differently. `None` is encoded distinctly from
/// `Some("")`, because "missing" and "empty" are different facts in MRFs.
pub fn stable_id<'a, I>(fields: I) -> String
where
    I: IntoIterator<Item = Option<&'a str>>,
{
    let mut h = Sha256::new();
    for f in fields {
        match f {
            None => h.update([0u8]),
            Some(s) => {
                h.update([1u8]);
                h.update((s.len() as u64).to_le_bytes());
                h.update(s.as_bytes());
            }
        }
    }
    let digest = h.finalize();
    hex::encode(&digest[..FILE_ID_LEN / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    // SHA-256("abc") from FIPS 180-2.
    const ABC_SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn sha_of_local_file_matches_vector() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc");
        std::fs::write(&p, b"abc").unwrap();
        let sha = sha256_hex_of(&Source::Path(p)).unwrap();
        assert_eq!(sha, ABC_SHA);
        assert_eq!(file_id_from_sha256(&sha), "ba7816bf8f01cfea");
    }

    #[test]
    fn sha_of_url_matches_local() {
        let server = crate::testutil::TestServer::start(vec![("/abc".into(), b"abc".to_vec())]);
        assert_eq!(
            sha256_hex_of(&Source::Url(server.url("/abc"))).unwrap(),
            ABC_SHA
        );
    }

    #[test]
    fn hashing_reader_counts_and_hashes() {
        let mut r = HashingReader::new(&b"abc"[..]);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(r.bytes_read(), 3);
        assert_eq!(r.finish_hex(), ABC_SHA);
    }

    #[test]
    fn stable_id_is_deterministic_and_unambiguous() {
        let a = stable_id([Some("ab"), Some("c")]);
        assert_eq!(a, stable_id([Some("ab"), Some("c")]));
        assert_eq!(a.len(), 16);
        assert_ne!(a, stable_id([Some("a"), Some("bc")]));
        assert_ne!(stable_id([None]), stable_id([Some("")]));
    }
}
