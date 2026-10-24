//! Paths and names on the wire: a plain string field when they're valid
//! UTF-8, the same field name with `_b64` and base64 bytes otherwise. Unix
//! paths are bytes; this keeps the common case readable without losing the
//! odd one.

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

macro_rules! wire_bytes {
    ($(#[$doc:meta])* $ty:ident, $field:literal, $b64:literal, required: $required:literal) => {
        $(#[$doc])*
        #[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
        pub struct $ty(pub Option<Vec<u8>>);

        impl $ty {
            pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
                $ty(Some(bytes.into()))
            }

            pub fn bytes(&self) -> &[u8] {
                self.0.as_deref().unwrap_or_default()
            }

            pub fn to_path(&self) -> PathBuf {
                PathBuf::from(OsString::from_vec(self.bytes().to_vec()))
            }

            /// For display: lossy UTF-8.
            pub fn lossy(&self) -> String {
                String::from_utf8_lossy(self.bytes()).into_owned()
            }
        }

        impl From<&Path> for $ty {
            fn from(p: &Path) -> Self {
                $ty::new(p.as_os_str().as_bytes())
            }
        }

        impl From<PathBuf> for $ty {
            fn from(p: PathBuf) -> Self {
                $ty::new(p.into_os_string().into_vec())
            }
        }

        impl From<&str> for $ty {
            fn from(s: &str) -> Self {
                $ty::new(s.as_bytes())
            }
        }

        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let Some(b) = &self.0 else { return s.serialize_map(Some(0))?.end() };
                let mut m = s.serialize_map(Some(1))?;
                match std::str::from_utf8(b) {
                    Ok(text) => m.serialize_entry($field, text)?,
                    Err(_) => m.serialize_entry($b64, &base64_encode(b))?,
                }
                m.end()
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = $ty;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        write!(f, "a `{}` or `{}` field", $field, $b64)
                    }
                    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<$ty, A::Error> {
                        let mut out = None;
                        while let Some(k) = map.next_key::<String>()? {
                            if k == $field {
                                out = Some(map.next_value::<String>()?.into_bytes());
                            } else if k == $b64 {
                                let s = map.next_value::<String>()?;
                                out = Some(base64_decode(&s).ok_or_else(|| de::Error::custom("bad base64"))?);
                            } else {
                                map.next_value::<de::IgnoredAny>()?;
                            }
                        }
                        if $required && out.is_none() {
                            return Err(de::Error::missing_field($field));
                        }
                        Ok($ty(out))
                    }
                }
                d.deserialize_map(V)
            }
        }
    };
}

wire_bytes!(
    /// A `path` (or `path_b64`) field.
    WirePath, "path", "path_b64", required: true
);
wire_bytes!(
    /// A `name` (or `name_b64`) field.
    WireName, "name", "name_b64", required: true
);
wire_bytes!(
    /// An optional `under` (or `under_b64`) field.
    WireUnder, "under", "under_b64", required: false
);

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in s {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for n in 0..40u8 {
            let data: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37).wrapping_add(200)).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
    }

    #[test]
    fn paths_pick_their_field() {
        let p = WirePath::new(b"/home/s/\xff\xfe".to_vec());
        let j = serde_json::to_string(&p).unwrap();
        assert!(j.contains("path_b64"), "{j}");
        assert_eq!(serde_json::from_str::<WirePath>(&j).unwrap(), p);
        let q = WirePath::from("/home/sigma/Été");
        let j = serde_json::to_string(&q).unwrap();
        assert_eq!(j, r#"{"path":"/home/sigma/Été"}"#);
        assert_eq!(serde_json::from_str::<WirePath>(&j).unwrap(), q);
    }
}
