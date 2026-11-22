//! Minimal MessagePack encoder for Perps signable operations.
//!
//! Perps signs `keccak256(msgpack(compact_op))`. The compact operations only
//! contain booleans, non-negative integers, strings and arrays, and the bytes
//! must match the reference SDK's `@msgpack/msgpack` output exactly, so the
//! encoder always picks the smallest representation the spec allows.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    UInt(u64),
    Str(String),
    Array(Vec<Value>),
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::UInt(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::Array(v)
    }
}

pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Bool(false) => out.push(0xc2),
        Value::Bool(true) => out.push(0xc3),
        Value::UInt(n) => write_uint(out, *n),
        Value::Str(s) => {
            let bytes = s.as_bytes();
            let len = bytes.len();
            if len < 32 {
                out.push(0xa0 | len as u8);
            } else if len <= 0xff {
                out.push(0xd9);
                out.push(len as u8);
            } else if len <= 0xffff {
                out.push(0xda);
                out.extend_from_slice(&(len as u16).to_be_bytes());
            } else {
                out.push(0xdb);
                out.extend_from_slice(&(len as u32).to_be_bytes());
            }
            out.extend_from_slice(bytes);
        }
        Value::Array(items) => {
            let len = items.len();
            if len < 16 {
                out.push(0x90 | len as u8);
            } else if len <= 0xffff {
                out.push(0xdc);
                out.extend_from_slice(&(len as u16).to_be_bytes());
            } else {
                out.push(0xdd);
                out.extend_from_slice(&(len as u32).to_be_bytes());
            }
            for item in items {
                write_value(out, item);
            }
        }
    }
}

fn write_uint(out: &mut Vec<u8>, n: u64) {
    if n < 0x80 {
        out.push(n as u8);
    } else if n <= 0xff {
        out.push(0xcc);
        out.push(n as u8);
    } else if n <= 0xffff {
        out.push(0xcd);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= 0xffff_ffff {
        out.push(0xce);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(0xcf);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: impl Into<Value>) -> Vec<u8> {
        encode(&v.into())
    }

    #[test]
    fn integers_use_smallest_encoding() {
        assert_eq!(enc(0u64), [0x00]);
        assert_eq!(enc(127u64), [0x7f]);
        assert_eq!(enc(128u64), [0xcc, 0x80]);
        assert_eq!(enc(255u64), [0xcc, 0xff]);
        assert_eq!(enc(256u64), [0xcd, 0x01, 0x00]);
        assert_eq!(enc(65_535u64), [0xcd, 0xff, 0xff]);
        assert_eq!(enc(65_536u64), [0xce, 0x00, 0x01, 0x00, 0x00]);
        assert_eq!(enc(1_234_567_890u64), [0xce, 0x49, 0x96, 0x02, 0xd2]);
        assert_eq!(enc(4_294_967_296u64), [0xcf, 0, 0, 0, 0x01, 0, 0, 0, 0]);
    }

    #[test]
    fn booleans() {
        assert_eq!(enc(true), [0xc3]);
        assert_eq!(enc(false), [0xc2]);
    }

    #[test]
    fn strings_switch_to_str8_at_32_bytes() {
        assert_eq!(enc("tp"), [0xa2, b't', b'p']);
        let s31 = "a".repeat(31);
        assert_eq!(enc(s31.as_str())[0], 0xbf);
        // 32-char client order IDs need str8.
        let s32 = "7f9e4a2b6c8d0e1f1234567890abcdef";
        let bytes = enc(s32);
        assert_eq!(&bytes[..2], &[0xd9, 32]);
        assert_eq!(&bytes[2..], s32.as_bytes());
        assert_eq!(enc("x".repeat(256).as_str())[..3], [0xda, 0x01, 0x00]);
    }

    #[test]
    fn arrays_switch_to_array16_at_16_items() {
        let small: Vec<Value> = (0..15u64).map(Value::from).collect();
        assert_eq!(enc(small)[0], 0x9f);
        let big: Vec<Value> = (0..16u64).map(Value::from).collect();
        assert_eq!(enc(big)[..3], [0xdc, 0x00, 0x10]);
    }

    #[test]
    fn nested_cancel_operation() {
        // ["cancelOrders", [1234567890]]
        let op = Value::Array(vec![
            "cancelOrders".into(),
            vec![Value::from(1_234_567_890u64)].into(),
        ]);
        let mut expected = vec![0x92, 0xac];
        expected.extend_from_slice(b"cancelOrders");
        expected.extend_from_slice(&[0x91, 0xce, 0x49, 0x96, 0x02, 0xd2]);
        assert_eq!(encode(&op), expected);
    }
}
