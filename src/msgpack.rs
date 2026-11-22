//! Minimal MessagePack encoder used for signing Perps operations.
//!
//! Polymarket hashes the MessagePack encoding of a compact operation array
//! with keccak256. We only need a tiny subset of MessagePack (arrays, strings,
//! bools and non-negative integers), so this module keeps the encoding explicit
//! and dependency-free instead of pulling in a general-purpose codec.

/// A value in the compact Perps operation representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mp {
    Int(u64),
    Bool(bool),
    Str(String),
    Arr(Vec<Mp>),
}

impl From<u64> for Mp {
    fn from(value: u64) -> Self {
        Mp::Int(value)
    }
}

impl From<u32> for Mp {
    fn from(value: u32) -> Self {
        Mp::Int(u64::from(value))
    }
}

impl From<bool> for Mp {
    fn from(value: bool) -> Self {
        Mp::Bool(value)
    }
}

impl From<&str> for Mp {
    fn from(value: &str) -> Self {
        Mp::Str(value.to_owned())
    }
}

impl From<String> for Mp {
    fn from(value: String) -> Self {
        Mp::Str(value)
    }
}

impl From<Vec<Mp>> for Mp {
    fn from(value: Vec<Mp>) -> Self {
        Mp::Arr(value)
    }
}

/// Encodes a value using the canonical MessagePack representation produced by
/// `@msgpack/msgpack` for the same input.
pub fn encode(value: &Mp) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

fn encode_into(value: &Mp, out: &mut Vec<u8>) {
    match value {
        Mp::Int(number) => encode_uint(*number, out),
        Mp::Bool(true) => out.push(0xc3),
        Mp::Bool(false) => out.push(0xc2),
        Mp::Str(text) => encode_str(text, out),
        Mp::Arr(items) => {
            encode_array_len(items.len(), out);
            for item in items {
                encode_into(item, out);
            }
        }
    }
}

fn encode_uint(number: u64, out: &mut Vec<u8>) {
    if number < 128 {
        out.push(number as u8);
    } else if u8::try_from(number).is_ok() {
        out.push(0xcc);
        out.push(number as u8);
    } else if u16::try_from(number).is_ok() {
        out.push(0xcd);
        out.extend_from_slice(&(number as u16).to_be_bytes());
    } else if u32::try_from(number).is_ok() {
        out.push(0xce);
        out.extend_from_slice(&(number as u32).to_be_bytes());
    } else {
        out.push(0xcf);
        out.extend_from_slice(&number.to_be_bytes());
    }
}

fn encode_str(text: &str, out: &mut Vec<u8>) {
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len < 32 {
        out.push(0xa0 | len as u8);
    } else if len < 256 {
        out.push(0xd9);
        out.push(len as u8);
    } else if len < 65_536 {
        out.push(0xda);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0xdb);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

fn encode_array_len(len: usize, out: &mut Vec<u8>) {
    if len < 16 {
        out.push(0x90 | len as u8);
    } else if len < 65_536 {
        out.push(0xdc);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0xdd);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_fixarray_fixstr_bool_and_fixint() {
        let value = Mp::Arr(vec![
            Mp::from("createOrders"),
            Mp::Arr(vec![Mp::Arr(vec![
                Mp::from(1u32),
                Mp::Bool(false),
                Mp::from("0"),
                Mp::Bool(false),
                Mp::Bool(true),
                Mp::Arr(vec![Mp::Bool(true), Mp::from("70000"), Mp::from("tp")]),
            ])]),
            Mp::from("position"),
        ]);
        let encoded = encode(&value);
        assert_eq!(encoded[0], 0x93); // array of 3
        assert_eq!(encoded[1], 0xac); // "createOrders" (12 chars)
        assert!(encoded.ends_with(b"position"));
        // The nested order array has six compact fields.
        let expected_prefix = [
            0x93, 0xac, b'c', b'r', b'e', b'a', b't', b'e', b'O', b'r', b'd', b'e', b'r', b's',
            0x91,
        ];
        assert_eq!(&encoded[..expected_prefix.len()], &expected_prefix);
    }

    #[test]
    fn encodes_reference_operation_bytes() {
        let value = Mp::Arr(vec![
            Mp::Str("createOrders".to_owned()),
            Mp::Arr(vec![
                Mp::Arr(vec![
                    Mp::Int(1),
                    Mp::Bool(false),
                    Mp::Str("0".to_owned()),
                    Mp::Bool(false),
                    Mp::Bool(true),
                    Mp::Arr(vec![
                        Mp::Bool(true),
                        Mp::Str("70000".to_owned()),
                        Mp::Str("tp".to_owned()),
                    ]),
                ]),
                Mp::Arr(vec![
                    Mp::Int(1),
                    Mp::Bool(false),
                    Mp::Str("0".to_owned()),
                    Mp::Bool(false),
                    Mp::Bool(true),
                    Mp::Arr(vec![
                        Mp::Bool(true),
                        Mp::Str("62000".to_owned()),
                        Mp::Str("sl".to_owned()),
                    ]),
                ]),
            ]),
            Mp::Str("position".to_owned()),
        ]);
        assert_eq!(
            hex::encode(encode(&value)),
            "93ac6372656174654f7264657273929601c2a130c2c393c3a53730303030a27470\
             9601c2a130c2c393c3a53632303030a2736ca8706f736974696f6e"
        );
    }

    #[test]
    fn encodes_uint_boundaries() {
        let mut out = Vec::new();
        encode_uint(127, &mut out);
        assert_eq!(out, [0x7f]);
        out.clear();
        encode_uint(128, &mut out);
        assert_eq!(out, [0xcc, 0x80]);
        out.clear();
        encode_uint(65_535, &mut out);
        assert_eq!(out, [0xcd, 0xff, 0xff]);
        out.clear();
        encode_uint(65_536, &mut out);
        assert_eq!(out, [0xce, 0x00, 0x01, 0x00, 0x00]);
    }
}
