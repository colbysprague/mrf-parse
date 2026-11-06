//! Small building blocks for streaming JSON with `serde_json`.
//!
//! `serde_json` can only stream if we drive it with our own visitors. These
//! helpers keep that boilerplate in one place:
//!
//! * [`drive_object`] walks a top-level object and calls a handler for each
//!   key with access to the value, so the handler decides whether to read
//!   it whole (`next_value::<Value>()`), skip it (`IgnoredAny`), or stream
//!   it (`next_value_seed(...)`).
//! * [`EachElement`] is a seed that streams an array one element at a time,
//!   calling a closure with each element as a `serde_json::Value` and
//!   dropping it before reading the next.
//!
//! Errors from our own code (I/O, sinks) cannot travel through serde's
//! error type, so closures stash them in [`Abort`] and return `false`; the
//! seed then raises a generic serde error to unwind, and the caller checks
//! the stash first.

use std::fmt;
use std::io::Read;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// Message used for the serde error raised when our own code aborts.
pub const ABORTED: &str = "__mrf_aborted__";

/// Stash for an error raised by our own code inside a visitor.
pub struct Abort<E>(pub Option<E>);

impl<E> Abort<E> {
    pub fn new() -> Self {
        Abort(None)
    }

    /// Store `r`'s error (if any) and report whether to continue.
    pub fn keep_going(&mut self, r: Result<(), E>) -> bool {
        match r {
            Ok(()) => true,
            Err(e) => {
                self.0 = Some(e);
                false
            }
        }
    }
}

impl<E> Default for Abort<E> {
    fn default() -> Self {
        Self::new()
    }
}

/// Called for each top-level key. Implementors read the value through
/// `map`; they must consume exactly one value per call.
pub trait KeyHandler {
    fn on_key<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<(), A::Error>;
}

/// Walk the top-level JSON object of `input`, calling `h` for each key in
/// file order.
pub fn drive_object<R: Read, H: KeyHandler>(input: R, h: &mut H) -> Result<(), serde_json::Error> {
    // serde_json reads its input a byte at a time, so an unbuffered reader
    // (e.g. a bare `File`) would cost one syscall per byte. Always buffer;
    // for already-buffered inputs this is one extra memcpy per 64KB.
    let input = std::io::BufReader::with_capacity(64 * 1024, input);
    let mut de = serde_json::Deserializer::from_reader(input);
    de.deserialize_map(ObjectVisitor { h })?;
    // Trailing bytes after the top-level object are tolerated (logged).
    if let Err(e) = de.end() {
        tracing::warn!(error = %e, "trailing data after top-level JSON object (ignored)");
    }
    Ok(())
}

/// Like [`drive_object`], but the top level may also be an **array of
/// objects**, each walked in turn as if it were its own top-level object
/// (`h` sees every key of every object, in file order).
///
/// Real-world deviation: CareFirst publishes some tables of contents as
/// `[ {toc}, {toc} ]` instead of one object.
pub fn drive_objects<R: Read, H: KeyHandler>(input: R, h: &mut H) -> Result<(), serde_json::Error> {
    let input = std::io::BufReader::with_capacity(64 * 1024, input);
    let mut de = serde_json::Deserializer::from_reader(input);
    de.deserialize_any(ObjectOrArrayVisitor { h })?;
    if let Err(e) = de.end() {
        tracing::warn!(error = %e, "trailing data after top-level JSON value (ignored)");
    }
    Ok(())
}

struct ObjectVisitor<'h, H> {
    h: &'h mut H,
}

/// Top level that may be an object or an array of objects.
struct ObjectOrArrayVisitor<'h, H> {
    h: &'h mut H,
}

impl<'de, H: KeyHandler> Visitor<'de> for ObjectOrArrayVisitor<'_, H> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON object or an array of objects")
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<(), A::Error> {
        ObjectVisitor { h: self.h }.visit_map(map)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        // Each element is driven through the same handler, one at a time.
        while seq
            .next_element_seed(ObjectSeed { h: &mut *self.h })?
            .is_some()
        {}
        Ok(())
    }
}

/// Seed that walks one object with a handler (used for array elements).
struct ObjectSeed<'h, H> {
    h: &'h mut H,
}

impl<'de, H: KeyHandler> DeserializeSeed<'de> for ObjectSeed<'_, H> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(ObjectVisitor { h: self.h })
    }
}

impl<'de, H: KeyHandler> Visitor<'de> for ObjectVisitor<'_, H> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            self.h.on_key(&key, &mut map)?;
        }
        Ok(())
    }
}

/// Seed that streams an array, calling `f(index, element)` per element.
/// `f` returns `false` to abort (after stashing its error somewhere).
///
/// A non-array value (e.g. `null`) is treated as an empty array, so one odd
/// value does not end the stream.
pub struct EachElement<F> {
    pub f: F,
}

impl<'de, F: FnMut(usize, Value) -> bool> DeserializeSeed<'de> for EachElement<F> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de, F: FnMut(usize, Value) -> bool> Visitor<'de> for EachElement<F> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an array")
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> Result<(), A::Error> {
        let mut i = 0;
        while let Some(v) = seq.next_element::<Value>()? {
            if !(self.f)(i, v) {
                return Err(de::Error::custom(ABORTED));
            }
            i += 1;
        }
        Ok(())
    }

    // Non-array values: tolerated and ignored.
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while map
            .next_entry::<de::IgnoredAny, de::IgnoredAny>()?
            .is_some()
        {}
        Ok(())
    }
}

// --- value helpers --------------------------------------------------------

/// Trimmed non-empty string; numbers/bools rendered as text (NPIs, ids and
/// codes are often written as JSON numbers). Null/empty → `None`.
pub fn str_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `obj[key]` as a string.
pub fn get_str(obj: &Value, key: &str) -> Option<String> {
    obj.get(key).and_then(str_of)
}

/// Array of strings (or a single string → one-element list). Elements that
/// are not scalars are dropped. Empty → `None`.
pub fn str_list(v: Option<&Value>) -> Option<Vec<String>> {
    let out: Vec<String> = match v? {
        Value::Array(items) => items.iter().filter_map(str_of).collect(),
        other => str_of(other).into_iter().collect(),
    };
    (!out.is_empty()).then_some(out)
}

/// Snippet of a value for `rejects.raw_snippet`. Serializes at most a
/// bounded prefix so a huge value cannot blow memory just to be truncated.
pub fn snippet(v: &Value) -> String {
    struct Limited {
        buf: Vec<u8>,
    }
    impl std::io::Write for Limited {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let room = 2048usize.saturating_sub(self.buf.len());
            if room == 0 {
                // Pretend success but stop collecting; serde_json treats a
                // write error as fatal, so we must not return one.
                return Ok(b.len());
            }
            self.buf.extend_from_slice(&b[..b.len().min(room)]);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut w = Limited { buf: Vec::new() };
    let _ = serde_json::to_writer(&mut w, v);
    String::from_utf8_lossy(&w.buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Collect {
        keys: Vec<String>,
        items: Vec<Value>,
        abort: Abort<String>,
    }

    impl KeyHandler for Collect {
        fn on_key<'de, A: MapAccess<'de>>(
            &mut self,
            key: &str,
            map: &mut A,
        ) -> Result<(), A::Error> {
            self.keys.push(key.to_string());
            if key == "items" {
                let items = &mut self.items;
                let abort = &mut self.abort;
                map.next_value_seed(EachElement {
                    f: |_, v: Value| {
                        let r = if v == json!("stop") {
                            Err("stopped".to_string())
                        } else {
                            items.push(v);
                            Ok(())
                        };
                        abort.keep_going(r)
                    },
                })
            } else {
                map.next_value::<de::IgnoredAny>().map(|_| ())
            }
        }
    }

    fn run(doc: &str) -> (Collect, Result<(), serde_json::Error>) {
        let mut c = Collect {
            keys: vec![],
            items: vec![],
            abort: Abort::new(),
        };
        let r = drive_object(doc.as_bytes(), &mut c);
        (c, r)
    }

    #[test]
    fn streams_elements_and_keeps_key_order() {
        let (c, r) = run(r#"{"a": 1, "items": [1, {"x": 2}, [3]], "z": {"big": [1,2,3]}}"#);
        r.unwrap();
        assert_eq!(c.keys, vec!["a", "items", "z"]);
        assert_eq!(c.items, vec![json!(1), json!({"x": 2}), json!([3])]);
    }

    #[test]
    fn abort_carries_our_error() {
        let (c, r) = run(r#"{"items": [1, "stop", 3]}"#);
        assert!(r.unwrap_err().to_string().contains(ABORTED));
        assert_eq!(c.abort.0.as_deref(), Some("stopped"));
        assert_eq!(c.items, vec![json!(1)]);
    }

    #[test]
    fn top_level_array_of_objects() {
        let mut c = Collect {
            keys: vec![],
            items: vec![],
            abort: Abort::new(),
        };
        drive_objects(
            r#"[{"a": 1, "items": [1]}, {"b": 2, "items": [2, 3]}]"#.as_bytes(),
            &mut c,
        )
        .unwrap();
        assert_eq!(c.keys, vec!["a", "items", "b", "items"]);
        assert_eq!(c.items, vec![json!(1), json!(2), json!(3)]);
        // A plain object still works.
        let mut c2 = Collect {
            keys: vec![],
            items: vec![],
            abort: Abort::new(),
        };
        drive_objects(r#"{"items": [9]}"#.as_bytes(), &mut c2).unwrap();
        assert_eq!(c2.items, vec![json!(9)]);
    }

    #[test]
    fn non_array_is_empty() {
        let (c, r) = run(r#"{"items": null}"#);
        r.unwrap();
        assert!(c.items.is_empty());
    }

    #[test]
    fn helpers() {
        assert_eq!(str_of(&json!(1234567890)), Some("1234567890".into()));
        assert_eq!(str_of(&json!("  ")), None);
        assert_eq!(
            str_list(Some(&json!([1, "a", null]))),
            Some(vec!["1".into(), "a".into()])
        );
        assert_eq!(str_list(Some(&json!("x"))), Some(vec!["x".into()]));
        let big = Value::String("x".repeat(10_000));
        assert!(snippet(&big).len() <= 2048);
    }
}
