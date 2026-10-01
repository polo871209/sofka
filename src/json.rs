//! JSON Pointer lookup without per-segment allocation.

use std::borrow::Cow;

use serde_json::Value;

/// RFC 6901 lookup with the same results as `Value::pointer`. serde_json
/// 1.0.151 runs two `str::replace` calls per segment, so every lookup
/// allocates, and cell extraction does thousands of them per redraw.
pub trait Pointer {
    fn at(&self, pointer: &str) -> Option<&Value>;
}

impl Pointer for Value {
    fn at(&self, pointer: &str) -> Option<&Value> {
        if pointer.is_empty() {
            return Some(self);
        }
        pointer
            .strip_prefix('/')?
            .split('/')
            .try_fold(self, |target, token| {
                let token = unescape(token);
                match target {
                    Value::Object(map) => map.get(token.as_ref()),
                    Value::Array(list) => parse_index(&token).and_then(|i| list.get(i)),
                    _ => None,
                }
            })
    }
}

fn unescape(token: &str) -> Cow<'_, str> {
    if token.contains('~') {
        Cow::Owned(token.replace("~1", "/").replace("~0", "~"))
    } else {
        Cow::Borrowed(token)
    }
}

/// Mirrors serde_json: no sign and no leading zero.
fn parse_index(s: &str) -> Option<usize> {
    if s.starts_with('+') || (s.starts_with('0') && s.len() != 1) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[allow(clippy::disallowed_methods)] // the reference implementation
    fn matches_serde_json_pointer() {
        let doc = json!({
            "a": {"b": [10, {"c": "x"}]},
            "a/b": 1,
            "m~n": 2,
            "~1": 3,
            "": {"": 4},
            "0": "key",
            "s": "str",
        });
        for p in [
            "",
            "/",
            "//",
            "/a",
            "/a/b",
            "/a/b/0",
            "/a/b/1/c",
            "/a/b/2",
            "/a/b/01",
            "/a/b/+1",
            "/a/b/-1",
            "/a/b/x",
            "/a~1b",
            "/m~0n",
            "/~01",
            "/~1",
            "/0",
            "/s/0",
            "a",
            "/missing/deep",
            "/a/b/1/c/d",
        ] {
            assert_eq!(doc.at(p), doc.pointer(p), "pointer {p:?}");
        }
    }
}
