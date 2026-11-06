//! Text encoding repair.
//!
//! MRFs are supposed to be UTF-8, but real files contain stray Windows-1252
//! bytes (an en dash `0x96`, curly quotes `0x93`/`0x94`, ...) pasted from
//! Office documents. Seen in the CMS v2 wide hospital example and in a UCare
//! TiC file. A strict JSON parser rejects the whole file over one such byte,
//! so JSON inputs are passed through [`Utf8Repair`], which leaves valid UTF-8
//! untouched and decodes each invalid byte as Windows-1252.

use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Windows-1252 → `char`. Bytes 0x80–0x9F map to the cp1252 punctuation
/// table; the five bytes cp1252 leaves undefined become U+FFFD. Everything
/// else is identical to Latin-1 (byte value = code point).
pub fn cp1252_char(b: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

/// Decode a whole byte slice as Windows-1252.
pub fn decode_cp1252(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| cp1252_char(b)).collect()
}

/// A `Read` adapter that outputs valid UTF-8: valid sequences pass through
/// unchanged; each byte that is not part of a valid sequence is replaced by
/// its Windows-1252 character (UTF-8 encoded). The number of replaced bytes
/// is available through the shared counter from [`Utf8Repair::counter`].
///
/// Streaming: input is processed in chunks; a multi-byte sequence split
/// across two chunks is carried over (at most 3 bytes) rather than being
/// mistaken for invalid input.
pub struct Utf8Repair<R> {
    inner: R,
    /// Raw bytes read but not yet processed (an incomplete trailing sequence).
    carry: Vec<u8>,
    /// Processed UTF-8 waiting to be handed to the caller.
    out: Vec<u8>,
    out_pos: usize,
    eof: bool,
    replaced: Arc<AtomicU64>,
}

const CHUNK: usize = 64 * 1024;

impl<R: Read> Utf8Repair<R> {
    pub fn new(inner: R) -> Self {
        Utf8Repair {
            inner,
            carry: Vec::with_capacity(4),
            out: Vec::with_capacity(CHUNK + 16),
            out_pos: 0,
            eof: false,
            replaced: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Shared count of bytes replaced so far (readable after the adapter has
    /// been moved into a parser).
    pub fn counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.replaced)
    }

    /// Read the next chunk and convert it into `self.out`.
    fn refill(&mut self) -> io::Result<()> {
        self.out.clear();
        self.out_pos = 0;
        let mut raw = std::mem::take(&mut self.carry);
        let start = raw.len();
        raw.resize(start + CHUNK, 0);
        let n = loop {
            match self.inner.read(&mut raw[start..]) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };
        raw.truncate(start + n);
        if n == 0 {
            self.eof = true;
        }
        let mut rest: &[u8] = &raw;
        let mut replaced = 0u64;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    self.out.extend_from_slice(s.as_bytes());
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    self.out.extend_from_slice(&rest[..valid]);
                    match e.error_len() {
                        // Incomplete sequence at the end of the chunk: keep it
                        // for the next read (unless the input has ended).
                        None if !self.eof => {
                            self.carry.extend_from_slice(&rest[valid..]);
                            break;
                        }
                        // A bad byte: decode it as cp1252 and continue after it.
                        _ => {
                            let mut buf = [0u8; 4];
                            let c = cp1252_char(rest[valid]);
                            self.out
                                .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                            replaced += 1;
                            rest = &rest[valid + 1..];
                        }
                    }
                }
            }
        }
        if replaced > 0 {
            self.replaced.fetch_add(replaced, Ordering::Relaxed);
        }
        Ok(())
    }
}

impl<R: Read> Read for Utf8Repair<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Loop: a chunk can produce no output (e.g. it was only a carried
        // partial sequence), so keep refilling until there is output or EOF.
        while self.out_pos == self.out.len() {
            if self.eof && self.carry.is_empty() {
                return Ok(0);
            }
            self.refill()?;
        }
        let n = buf.len().min(self.out.len() - self.out_pos);
        buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
        self.out_pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repair(input: &[u8]) -> (String, u64) {
        let mut r = Utf8Repair::new(input);
        let counter = r.counter();
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        (s, counter.load(Ordering::Relaxed))
    }

    /// Feeds input one byte per `read` to exercise split sequences.
    struct OneByte<'a>(&'a [u8]);
    impl Read for OneByte<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    #[test]
    fn valid_utf8_untouched() {
        let s = "plain ascii, café, 😀, — dash";
        assert_eq!(repair(s.as_bytes()), (s.to_string(), 0));
    }

    #[test]
    fn cp1252_bytes_decoded() {
        // "a – b" with a cp1252 en dash, plus curly quotes.
        let (s, n) = repair(b"a \x96 b \x93q\x94");
        assert_eq!(s, "a \u{2013} b \u{201C}q\u{201D}");
        assert_eq!(n, 3);
    }

    #[test]
    fn split_multibyte_sequences_survive_chunking() {
        let s = "€uro 😀 ok";
        let mut r = Utf8Repair::new(OneByte(s.as_bytes()));
        let c = r.counter();
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        assert_eq!(out, s);
        assert_eq!(c.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn truncated_sequence_at_eof() {
        // Lone lead byte of a 3-byte sequence at end of input.
        let (s, n) = repair(b"ab\xe2\x82");
        assert_eq!(s, "ab\u{e2}\u{201A}");
        assert_eq!(n, 2);
    }
}
