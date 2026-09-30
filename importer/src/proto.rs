//! Protocol Buffers wire format, as far as OSM PBF files need it.
//!
//! A message is a sequence of fields, each a varint key (field number << 3 |
//! wire type) followed by its value: a varint (type 0), 8 bytes (1), a
//! length-prefixed byte string (2) or 4 bytes (5). Repeated scalars may be
//! packed into one length-prefixed field or appear one per field; decoders
//! must accept both.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    Truncated,
    VarintTooLong,
    UnsupportedWireType(u64),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Truncated => write!(f, "truncated protobuf message"),
            ProtoError::VarintTooLong => write!(f, "varint longer than 10 bytes"),
            ProtoError::UnsupportedWireType(t) => write!(f, "unsupported protobuf wire type {t}"),
        }
    }
}

impl std::error::Error for ProtoError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// Reads a varint at the start of `data`, returning it and the rest.
pub fn varint(data: &[u8]) -> Result<(u64, &[u8]), ProtoError> {
    let mut value = 0u64;
    for (i, &byte) in data.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((value, &data[i + 1..]));
        }
    }
    Err(if data.len() < 10 {
        ProtoError::Truncated
    } else {
        ProtoError::VarintTooLong
    })
}

pub fn zigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn split(data: &[u8], n: usize) -> Result<(&[u8], &[u8]), ProtoError> {
    (data.len() >= n)
        .then(|| data.split_at(n))
        .ok_or(ProtoError::Truncated)
}

/// The fields of a message, in order.
pub fn fields(data: &[u8]) -> Fields<'_> {
    Fields { rest: data }
}

pub struct Fields<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Value<'a>), ProtoError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let parsed = (|| {
            let (key, rest) = varint(self.rest)?;
            let number = (key >> 3) as u32;
            let (value, rest) = match key & 7 {
                0 => {
                    let (v, rest) = varint(rest)?;
                    (Value::Varint(v), rest)
                }
                1 => {
                    let (bytes, rest) = split(rest, 8)?;
                    (
                        Value::Fixed64(u64::from_le_bytes(bytes.try_into().expect("8 bytes"))),
                        rest,
                    )
                }
                2 => {
                    let (len, rest) = varint(rest)?;
                    let (bytes, rest) = split(
                        rest,
                        usize::try_from(len).map_err(|_| ProtoError::Truncated)?,
                    )?;
                    (Value::Bytes(bytes), rest)
                }
                5 => {
                    let (bytes, rest) = split(rest, 4)?;
                    (
                        Value::Fixed32(u32::from_le_bytes(bytes.try_into().expect("4 bytes"))),
                        rest,
                    )
                }
                other => return Err(ProtoError::UnsupportedWireType(other)),
            };
            Ok(((number, value), rest))
        })();
        match parsed {
            Ok((field, rest)) => {
                self.rest = rest;
                Some(Ok(field))
            }
            Err(e) => {
                // Stop after an error; the message is unusable.
                self.rest = &[];
                Some(Err(e))
            }
        }
    }
}

/// All varints in a packed field.
pub fn packed_varints(mut data: &[u8]) -> Result<Vec<u64>, ProtoError> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let (v, rest) = varint(data)?;
        out.push(v);
        data = rest;
    }
    Ok(out)
}

/// Appends a repeated varint field's values, packed or not.
pub fn push_varints(value: Value<'_>, out: &mut Vec<u64>) -> Result<(), ProtoError> {
    match value {
        Value::Varint(v) => out.push(v),
        Value::Bytes(b) => out.extend(packed_varints(b)?),
        Value::Fixed64(_) | Value::Fixed32(_) => {}
    }
    Ok(())
}

pub fn as_bytes(value: Value<'_>) -> Option<&[u8]> {
    match value {
        Value::Bytes(b) => Some(b),
        _ => None,
    }
}

pub fn as_varint(value: Value<'_>) -> Option<u64> {
    match value {
        Value::Varint(v) => Some(v),
        _ => None,
    }
}

/// Undoes the delta coding of a packed signed field.
pub fn undelta(values: &[u64]) -> Vec<i64> {
    values
        .iter()
        .scan(0i64, |acc, &v| {
            *acc = acc.wrapping_add(zigzag(v));
            Some(*acc)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_and_zigzag() {
        assert_eq!(varint(&[0x96, 0x01, 0xff]), Ok((150, &[0xff][..])));
        assert_eq!(varint(&[0x80]), Err(ProtoError::Truncated));
        assert_eq!(varint(&[0xff; 11]), Err(ProtoError::VarintTooLong));
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(1), -1);
        assert_eq!(zigzag(2), 1);
        assert_eq!(zigzag(u64::MAX), i64::MIN);
    }

    #[test]
    fn reads_each_wire_type() {
        // 1: varint 5; 2: bytes "hi"; 3: fixed32 7; 4: fixed64 9.
        let msg = [
            0x08, 5, 0x12, 2, b'h', b'i', 0x1d, 7, 0, 0, 0, 0x21, 9, 0, 0, 0, 0, 0, 0, 0,
        ];
        let got: Vec<_> = fields(&msg).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            got,
            vec![
                (1, Value::Varint(5)),
                (2, Value::Bytes(b"hi")),
                (3, Value::Fixed32(7)),
                (4, Value::Fixed64(9))
            ]
        );
    }

    #[test]
    fn rejects_truncated_and_unknown_fields() {
        assert!(fields(&[0x12, 5, b'a']).any(|f| f == Err(ProtoError::Truncated)));
        assert!(fields(&[0x0b]).any(|f| f == Err(ProtoError::UnsupportedWireType(3))));
    }

    #[test]
    fn repeated_fields_packed_or_not() {
        let mut out = Vec::new();
        push_varints(Value::Bytes(&[1, 2, 0x96, 0x01]), &mut out).unwrap();
        push_varints(Value::Varint(7), &mut out).unwrap();
        assert_eq!(out, vec![1, 2, 150, 7]);
        assert_eq!(undelta(&[2, 2, 3]), vec![1, 2, 0]);
    }
}
