//! Binary encodings shared by the storage adapters and models: log ids,
//! membership configurations, and the readers and writers under them.
//! Integers are little-endian; lengths are u32.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;

use crate::types::Membership;
#[cfg(any(test, feature = "sim"))]
use crate::types::OrLogId;
use crate::types::PeerInfo;

#[cfg(any(test, feature = "sim"))]
pub(crate) fn encode_log_id(log_id: &OrLogId) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&log_id.leader_id.term.to_le_bytes());
    out.extend_from_slice(&log_id.index.to_le_bytes());
    out
}

/// A membership configuration: its voter sets, then every member's id and
/// address.
pub(crate) fn encode_membership_config(out: &mut Vec<u8>, membership: &Membership) {
    let configs = membership.get_joint_config();
    put_u32(out, configs.len());
    for config in configs {
        put_u32(out, config.len());
        for id in config {
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
    let nodes: Vec<_> = membership.nodes().collect();
    put_u32(out, nodes.len());
    for (id, node) in nodes {
        out.extend_from_slice(&id.to_le_bytes());
        put_bytes(out, node.addr.as_bytes());
    }
}

pub(crate) fn decode_membership_config(reader: &mut Reader<'_>) -> Result<Membership, io::Error> {
    let configs = (0..reader.u32()?)
        .map(|_| (0..reader.u32()?).map(|_| reader.u64()).collect())
        .collect::<Result<Vec<BTreeSet<u64>>, io::Error>>()?;
    let nodes = (0..reader.u32()?)
        .map(|_| {
            let id = reader.u64()?;
            let addr = String::from_utf8(reader.bytes()?).map_err(io::Error::other)?;
            Ok((id, PeerInfo { addr }))
        })
        .collect::<Result<BTreeMap<u64, PeerInfo>, io::Error>>()?;
    Membership::new(configs, nodes).map_err(io::Error::other)
}

pub(crate) fn put_u32(out: &mut Vec<u8>, n: usize) {
    let n = u32::try_from(n).unwrap_or_else(|_| panic!("{n} does not fit in u32"));
    out.extend_from_slice(&n.to_le_bytes());
}

pub(crate) fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len());
    out.extend_from_slice(bytes);
}

#[cfg(any(test, feature = "sim"))]
pub(crate) fn decode_log_id(reader: &mut Reader<'_>) -> Result<OrLogId, io::Error> {
    let term = reader.u64()?;
    let index = reader.u64()?;
    Ok(OrLogId::new_term_index(term, index))
}

pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl Reader<'_> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&[u8], io::Error> {
        if self.0.len() < n {
            return Err(io::Error::other("a system value ends early"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    #[cfg(any(test, feature = "sim"))]
    pub(crate) fn u8(&mut self) -> Result<u8, io::Error> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32, io::Error> {
        let bytes: [u8; 4] = self.take(4)?.try_into().map_err(io::Error::other)?;
        Ok(u32::from_le_bytes(bytes))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, io::Error> {
        let bytes: [u8; 8] = self.take(8)?.try_into().map_err(io::Error::other)?;
        Ok(u64::from_le_bytes(bytes))
    }

    pub(crate) fn bytes(&mut self) -> Result<Vec<u8>, io::Error> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}
