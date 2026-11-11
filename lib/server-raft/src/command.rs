//! The replicated payload and its binary codec (plan 0028, "The payload").
//!
//! A [`Command`] is one replicated write: the entry at Raft index `i`
//! becomes `Db::apply(i, ops)` (plan 0025, decision 1). Blank and membership
//! entries carry no command and leave gaps in the applied sequence.
//!
//! The encoding is ours, not serde: the Raft log (plan 0025, S5) stores
//! these bytes, and the peer API carries them unchanged, so they must stay
//! stable across openraft and serde upgrades.
//!
//! ```text
//! version u8 = 1 | count u32 | op*
//! op = put:    kind u8 = 1 | klen u32 | key | vlen u32 | value
//!    | delete: kind u8 = 2 | klen u32 | key
//! ```
//!
//! Integers are little-endian. Keys already carry the server's namespace
//! byte (plan 0025, decision 8).

use std::fmt;

const VERSION: u8 = 1;
const PUT: u8 = 1;
const DELETE: u8 = 2;

/// One operation of a [`Command`]. Keys are stored keys, with their
/// namespace byte.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Op {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl Op {
    pub fn key(&self) -> &[u8] {
        match self {
            Op::Put { key, .. } | Op::Delete { key } => key,
        }
    }
}

/// One replicated write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Command {
    pub ops: Vec<Op>,
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Command({} ops)", self.ops.len())
    }
}

/// Why bytes are not a valid [`Command`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("command encoding ends early")]
    Truncated,
    #[error("unknown command encoding version {0}")]
    UnknownVersion(u8),
    #[error("unknown command operation kind {0}")]
    UnknownKind(u8),
    #[error("{0} bytes follow the last command operation")]
    TrailingBytes(usize),
    #[error("a command field is larger than u32::MAX bytes")]
    TooLarge,
}

impl Command {
    /// Append the encoding of this command to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), CodecError> {
        out.push(VERSION);
        put_len(out, self.ops.len())?;
        for op in &self.ops {
            match op {
                Op::Put { key, value } => {
                    out.push(PUT);
                    put_bytes(out, key)?;
                    put_bytes(out, value)?;
                }
                Op::Delete { key } => {
                    out.push(DELETE);
                    put_bytes(out, key)?;
                }
            }
        }
        Ok(())
    }

    /// The encoding of this command.
    pub fn to_bytes(&self) -> Result<Vec<u8>, CodecError> {
        let mut out = Vec::new();
        self.encode(&mut out)?;
        Ok(out)
    }

    /// Decode exactly one command from `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader { bytes };
        let version = reader.u8()?;
        if version != VERSION {
            return Err(CodecError::UnknownVersion(version));
        }
        let count = reader.u32()?;
        // Each operation takes at least 5 bytes, so a count beyond that is
        // a truncation; checking it first bounds the allocation.
        if count as usize > reader.bytes.len() / 5 {
            return Err(CodecError::Truncated);
        }
        let ops = (0..count)
            .map(|_| match reader.u8()? {
                PUT => Ok(Op::Put {
                    key: reader.bytes()?,
                    value: reader.bytes()?,
                }),
                DELETE => Ok(Op::Delete {
                    key: reader.bytes()?,
                }),
                kind => Err(CodecError::UnknownKind(kind)),
            })
            .collect::<Result<_, _>>()?;
        match reader.bytes.len() {
            0 => Ok(Command { ops }),
            n => Err(CodecError::TrailingBytes(n)),
        }
    }
}

fn put_len(out: &mut Vec<u8>, len: usize) -> Result<(), CodecError> {
    let len: u32 = len.try_into().map_err(|_| CodecError::TooLarge)?;
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CodecError> {
    put_len(out, bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], CodecError> {
        if self.bytes.len() < n {
            return Err(CodecError::Truncated);
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CodecError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| CodecError::Truncated)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, CodecError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    fn random_command(rng: &mut Rng) -> Command {
        let bytes = |rng: &mut Rng| -> Vec<u8> {
            let len = rng.below(12) as usize;
            (0..len).map(|_| rng.below(256) as u8).collect()
        };
        let ops = (0..rng.below(6))
            .map(|_| match rng.below(2) {
                0 => Op::Put {
                    key: bytes(rng),
                    value: bytes(rng),
                },
                _ => Op::Delete { key: bytes(rng) },
            })
            .collect();
        Command { ops }
    }

    #[test]
    fn commands_round_trip() {
        let mut rng = Rng::new(1);
        for _ in 0..2000 {
            let command = random_command(&mut rng);
            let bytes = command.to_bytes().unwrap();
            assert_eq!(Command::decode(&bytes), Ok(command));
        }
    }

    #[test]
    fn the_layout_is_fixed() {
        let command = Command {
            ops: vec![
                Op::Put {
                    key: b"\x01k".to_vec(),
                    value: b"v".to_vec(),
                },
                Op::Delete {
                    key: b"\x01d".to_vec(),
                },
            ],
        };
        let expected: &[u8] = b"\x01\x02\x00\x00\x00\
            \x01\x02\x00\x00\x00\x01k\x01\x00\x00\x00v\
            \x02\x02\x00\x00\x00\x01d";
        assert_eq!(command.to_bytes().unwrap(), expected);
    }

    #[test]
    fn every_truncation_is_rejected() {
        let mut rng = Rng::new(2);
        for _ in 0..200 {
            let bytes = random_command(&mut rng).to_bytes().unwrap();
            for len in 0..bytes.len() {
                assert_eq!(
                    Command::decode(&bytes[..len]),
                    Err(CodecError::Truncated),
                    "prefix of {len} of {bytes:?}"
                );
            }
        }
    }

    #[test]
    fn bad_encodings_are_rejected() {
        assert_eq!(
            Command::decode(b"\x02\x00\x00\x00\x00"),
            Err(CodecError::UnknownVersion(2))
        );
        assert_eq!(
            Command::decode(b"\x01\x01\x00\x00\x00\x07\x00\x00\x00\x00"),
            Err(CodecError::UnknownKind(7))
        );
        assert_eq!(
            Command::decode(b"\x01\x00\x00\x00\x00\xff"),
            Err(CodecError::TrailingBytes(1))
        );
        assert_eq!(
            Command::decode(b"\x01\xff\xff\xff\xff"),
            Err(CodecError::Truncated),
            "a huge count must not allocate"
        );
    }
}
