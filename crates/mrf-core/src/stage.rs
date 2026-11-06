//! Turning a `Source` into a local, hashed input ready to parse.
//!
//! Every output row carries `file_id` (derived from the SHA-256 of the raw
//! bytes), so the hash must be known *before* parsing starts. For local
//! files that costs one extra sequential read, which is cheap. For URLs a
//! second download is not cheap, so we stream the body once to a temp file,
//! hashing as we go, then parse the temp file. Memory stays flat either way;
//! URL inputs need disk space equal to the (compressed) download.
//!
//! Zip archives (detected by the `PK\x03\x04` magic, never the extension;
//! several hospital vendors serve zips from extension-less URLs) are
//! expanded by [`StagedInput::stage_all`]: each file entry is extracted to a
//! temp file while being hashed, so it becomes its own input with its own
//! `file_id` and a `source_uri` of `<archive>!/<entry name>`. Extraction
//! streams, so memory stays flat; disk use is the entry's uncompressed size.

use std::fs::File;
use std::io::{self, BufWriter, Read};
use std::path::{Path, PathBuf};

use tempfile::TempPath;

use crate::ids::{file_id_from_sha256, sha256_hex_of, HashingReader};
use crate::input::{open, open_raw, InputError, Source};

/// A source with its hash computed and its bytes available locally.
pub struct StagedInput {
    /// What the user asked for.
    pub source: Source,
    /// Recorded as `source_uri`: the source's URI, or
    /// `<archive uri>!/<entry name>` for a zip entry.
    pub uri: String,
    pub sha256: String,
    pub file_id: String,
    /// Raw size in bytes (compressed size if gzipped).
    pub raw_bytes: u64,
    local: PathBuf,
    // Keeps a downloaded temp file alive; deleted when this is dropped.
    _temp: Option<TempPath>,
}

impl StagedInput {
    /// Hash a local file, or download a URL into `tmp_dir` (or the system
    /// temp dir when `None`) while hashing.
    pub fn stage(source: &Source, tmp_dir: Option<&Path>) -> Result<StagedInput, InputError> {
        match source {
            Source::Path(p) => {
                let sha256 = sha256_hex_of(source)?;
                let raw_bytes =
                    std::fs::metadata(p)
                        .map(|m| m.len())
                        .map_err(|e| InputError::Io {
                            uri: source.uri(),
                            source: e,
                        })?;
                Ok(StagedInput {
                    source: source.clone(),
                    uri: source.uri(),
                    file_id: file_id_from_sha256(&sha256),
                    sha256,
                    raw_bytes,
                    local: p.clone(),
                    _temp: None,
                })
            }
            Source::Url(_) => {
                let io_err = |e: io::Error| InputError::Io {
                    uri: source.uri(),
                    source: e,
                };
                let tmp = match tmp_dir {
                    Some(d) => {
                        std::fs::create_dir_all(d).map_err(io_err)?;
                        tempfile::Builder::new().prefix("mrf-dl-").tempfile_in(d)
                    }
                    None => tempfile::Builder::new().prefix("mrf-dl-").tempfile(),
                }
                .map_err(io_err)?;
                let (file, path) = tmp.into_parts();
                let mut hashing = HashingReader::new(open_raw(source)?);
                let mut w = BufWriter::new(file);
                io::copy(&mut hashing, &mut w).map_err(io_err)?;
                w.into_inner().map_err(|e| io_err(e.into_error()))?;
                let raw_bytes = hashing.bytes_read();
                let sha256 = hashing.finish_hex();
                Ok(StagedInput {
                    source: source.clone(),
                    uri: source.uri(),
                    file_id: file_id_from_sha256(&sha256),
                    sha256,
                    raw_bytes,
                    local: path.to_path_buf(),
                    _temp: Some(path),
                })
            }
        }
    }

    /// Open the (decompressed) content for parsing.
    pub fn open(&self) -> Result<Box<dyn io::BufRead + Send>, InputError> {
        open(&Source::Path(self.local.clone())).map_err(|e| match e {
            // Report errors against the user's URI, not the temp path.
            InputError::Io { source, .. } => InputError::Io {
                uri: self.uri.clone(),
                source,
            },
            other => other,
        })
    }

    /// Local path of the raw bytes (the temp file for URL inputs).
    pub fn local_path(&self) -> &Path {
        &self.local
    }

    /// Like [`stage`](Self::stage), but a zip archive is expanded into one
    /// staged input per file entry, in archive order. Directory entries,
    /// `__MACOSX/` metadata and dot-files are skipped. Nested archives are
    /// not expanded (a zip inside a zip is passed through as an entry).
    pub fn stage_all(
        source: &Source,
        tmp_dir: Option<&Path>,
    ) -> Result<Vec<StagedInput>, InputError> {
        let staged = StagedInput::stage(source, tmp_dir)?;
        if !is_zip(&staged.local).map_err(|e| staged.io_err(e))? {
            return Ok(vec![staged]);
        }
        staged.expand_zip(tmp_dir)
    }

    fn io_err(&self, e: io::Error) -> InputError {
        InputError::Io {
            uri: self.uri.clone(),
            source: e,
        }
    }

    fn expand_zip(&self, tmp_dir: Option<&Path>) -> Result<Vec<StagedInput>, InputError> {
        let zip_err = |e: zip::result::ZipError| InputError::Zip {
            uri: self.uri.clone(),
            message: e.to_string(),
        };
        let file = File::open(&self.local).map_err(|e| self.io_err(e))?;
        let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
        let mut out = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(zip_err)?;
            if !entry.is_file() {
                continue;
            }
            let name = entry.name().map_err(zip_err)?.to_string();
            let base = name.rsplit('/').next().unwrap_or(&name);
            if name.starts_with("__MACOSX/") || base.starts_with('.') || base.is_empty() {
                continue;
            }
            let uri = format!("{}!/{}", self.uri, name);
            let io_err = |e: io::Error| InputError::Io {
                uri: uri.clone(),
                source: e,
            };
            let tmp = match tmp_dir {
                Some(d) => {
                    std::fs::create_dir_all(d).map_err(io_err)?;
                    tempfile::Builder::new().prefix("mrf-zip-").tempfile_in(d)
                }
                None => tempfile::Builder::new().prefix("mrf-zip-").tempfile(),
            }
            .map_err(io_err)?;
            let (f, path) = tmp.into_parts();
            // `entry` yields decompressed bytes; hash them as they are written.
            let mut hashing = HashingReader::new(&mut entry);
            let mut w = BufWriter::new(f);
            io::copy(&mut hashing, &mut w).map_err(io_err)?;
            w.into_inner().map_err(|e| io_err(e.into_error()))?;
            let raw_bytes = hashing.bytes_read();
            let sha256 = hashing.finish_hex();
            out.push(StagedInput {
                source: self.source.clone(),
                uri,
                file_id: file_id_from_sha256(&sha256),
                sha256,
                raw_bytes,
                local: path.to_path_buf(),
                _temp: Some(path),
            });
        }
        if out.is_empty() {
            return Err(InputError::Zip {
                uri: self.uri.clone(),
                message: "zip archive contains no files".into(),
            });
        }
        Ok(out)
    }
}

/// `PK\x03\x04` (local file header) or `PK\x05\x06` (empty archive).
fn is_zip(path: &Path) -> io::Result<bool> {
    let mut head = [0u8; 4];
    let mut f = File::open(path)?;
    let mut n = 0;
    while n < 4 {
        match f.read(&mut head[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n == 4 && (head == *b"PK\x03\x04" || head == *b"PK\x05\x06"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, body) in entries {
                if name.ends_with('/') {
                    z.add_directory(*name, opts).unwrap();
                } else {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(body).unwrap();
                }
            }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn zip_entries_become_separate_inputs() {
        let dir = tempfile::tempdir().unwrap();
        // No .zip extension: detected by magic bytes.
        let p = dir.path().join("download");
        std::fs::write(
            &p,
            make_zip(&[
                ("a.csv", b"abc"),
                ("sub/", b""),
                ("sub/b.json", b"{}"),
                ("__MACOSX/._a.csv", b"junk"),
                (".DS_Store", b"junk"),
            ]),
        )
        .unwrap();
        let src = Source::Path(p.clone());
        let staged = StagedInput::stage_all(&src, Some(dir.path())).unwrap();
        let uris: Vec<_> = staged.iter().map(|s| s.uri.clone()).collect();
        assert_eq!(
            uris,
            vec![
                format!("{}!/a.csv", p.display()),
                format!("{}!/sub/b.json", p.display())
            ]
        );
        // file_id is the hash of the entry's (decompressed) bytes.
        assert_eq!(staged[0].file_id, "ba7816bf8f01cfea");
        let mut s = String::new();
        staged[0].open().unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "abc");
    }

    #[test]
    fn non_zip_and_empty_zip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("plain.csv");
        std::fs::write(&p, b"PK is not enough").unwrap();
        let staged = StagedInput::stage_all(&Source::Path(p), None).unwrap();
        assert_eq!(staged.len(), 1);

        let z = dir.path().join("empty.zip");
        std::fs::write(&z, make_zip(&[("only_dir/", b"")])).unwrap();
        let err = StagedInput::stage_all(&Source::Path(z), None)
            .err()
            .unwrap();
        assert!(matches!(err, InputError::Zip { .. }), "{err}");
    }

    #[test]
    fn local_and_url_stage_identically() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc");
        std::fs::write(&p, b"abc").unwrap();
        let local = StagedInput::stage(&Source::Path(p), None).unwrap();

        let server = crate::testutil::TestServer::start(vec![("/abc".into(), b"abc".to_vec())]);
        let remote =
            StagedInput::stage(&Source::Url(server.url("/abc")), Some(dir.path())).unwrap();
        assert_eq!(local.sha256, remote.sha256);
        assert_eq!(remote.file_id, "ba7816bf8f01cfea");
        assert_eq!(remote.raw_bytes, 3);
        let mut s = String::new();
        remote.open().unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "abc");

        let tmp = remote.local_path().to_path_buf();
        assert!(tmp.exists());
        drop(remote);
        assert!(!tmp.exists(), "temp download removed on drop");
    }
}
