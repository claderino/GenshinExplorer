//! Minimal schema-less protobuf wire-format walker.
//!
//! Genshin scrambles command IDs and has reshuffled field numbers between
//! game versions, so we never decode into generated types. Instead we walk
//! the raw wire format and match packets by their *value patterns*.

/// A single decoded protobuf field value.
#[allow(dead_code)] // Fixed64 is parsed for completeness (double/fixed64 fields)
#[derive(Debug, Clone)]
pub enum Value {
    Varint(u64),
    Fixed32(u32),
    Fixed64(u64),
    Bytes(Vec<u8>),
}

impl Value {
    pub fn as_varint(&self) -> Option<u64> {
        match self {
            Value::Varint(v) => Some(*v),
            _ => None,
        }
    }

    /// Interpret a fixed32 field as an IEEE-754 float (protobuf `float`).
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            Value::Fixed32(bits) => Some(f32::from_bits(*bits)),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

pub type Fields = Vec<(u32, Value)>;

/// Parse a buffer as a sequence of protobuf fields.
/// Returns `None` if the buffer is not valid protobuf (strict: the entire
/// buffer must be consumed).
pub fn parse(buf: &[u8]) -> Option<Fields> {
    let (fields, consumed) = parse_partial(buf)?;
    if consumed == buf.len() {
        Some(fields)
    } else {
        None
    }
}

/// Tolerant parse: returns whatever fields parsed cleanly before the first
/// malformed/truncated field, plus how many bytes were consumed. Real
/// captures can carry trailing anomalies; a single bad byte must not
/// discard a message full of valid data.
pub fn parse_partial(buf: &[u8]) -> Option<(Fields, usize)> {
    if buf.len() > 8 * 1024 * 1024 {
        return None;
    }

    let mut fields = Vec::new();
    let mut i = 0usize;

    while i < buf.len() {
        let start = i;
        let Some((key, n)) = read_varint(&buf[i..]) else {
            return Some((fields, start));
        };
        i += n;

        let field_number = (key >> 3) as u32;
        let wire_type = (key & 0x7) as u8;
        if field_number == 0 || field_number > 2_000_000 {
            return Some((fields, start));
        }

        let value = match wire_type {
            0 => {
                let Some((v, n)) = read_varint(&buf[i..]) else {
                    return Some((fields, start));
                };
                i += n;
                Value::Varint(v)
            }
            1 => {
                if i + 8 > buf.len() {
                    return Some((fields, start));
                }
                // protobuf wire format: fixed64 is little-endian
                let v = u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
                i += 8;
                Value::Fixed64(v)
            }
            2 => {
                let Some((len, n)) = read_varint(&buf[i..]) else {
                    return Some((fields, start));
                };
                i += n;
                let len = len as usize;
                if i.checked_add(len).map(|end| end > buf.len()).unwrap_or(true) {
                    return Some((fields, start));
                }
                let b = buf[i..i + len].to_vec();
                i += len;
                Value::Bytes(b)
            }
            5 => {
                if i + 4 > buf.len() {
                    return Some((fields, start));
                }
                // protobuf wire format: fixed32 is little-endian
                let v = u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
                i += 4;
                Value::Fixed32(v)
            }
            // SGROUP/EGROUP are deprecated and unused here.
            _ => return Some((fields, start)),
        };

        fields.push((field_number, value));
    }

    Some((fields, buf.len()))
}

pub fn read_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 64 {
            return None;
        }
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
        shift += 7;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_message() {
        // field 1 varint 202, field 2 varint 1500 (1500 = 0xDC 0x0B)
        let bytes = [0x08, 0xCA, 0x01, 0x10, 0xDC, 0x0B];
        let fields = parse(&bytes).unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, 1);
        assert_eq!(fields[0].1.as_varint(), Some(202));
        assert_eq!(fields[1].0, 2);
        assert_eq!(fields[1].1.as_varint(), Some(1500));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(&[0xFF, 0xFF]).is_none());
    }
}
