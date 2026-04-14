use std::{
    collections::hash_map::DefaultHasher,
    fmt,
    hash::{Hash, Hasher},
};

use semver::Version;
use serde::{Deserialize, Serialize};

pub const DISCOVERY_PREFIX: [u8; 16] = *b"ALVR\0\0\0\0\0\0\0\0\0\0\0\0";
pub const DISCOVERY_PACKET_LEN: usize = 16 + 8 + 32;
pub const DISCOVERY_PORT: u16 = 9943;
pub const STREAM_PORT: u16 = 9944;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProtocolId(pub [u8; 8]);

impl ProtocolId {
    pub fn from_version(version: &str) -> Self {
        let protocol_string = Version::parse(version)
            .map(|parsed| {
                if parsed.pre.is_empty() {
                    parsed.major.to_string()
                } else {
                    format!("{}-{}", parsed.major, parsed.pre)
                }
            })
            .unwrap_or_else(|_| version.to_string());

        Self(hash_string(&protocol_string).to_le_bytes())
    }

    pub fn as_bytes(self) -> [u8; 8] {
        self.0
    }

    pub fn as_u64(self) -> u64 {
        u64::from_le_bytes(self.0)
    }
}

pub(crate) fn hash_string(string: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    string.hash(&mut hasher);
    hasher.finish()
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryPacket {
    pub protocol_id: ProtocolId,
    pub hostname: String,
}

impl DiscoveryPacket {
    pub fn encode(&self) -> [u8; DISCOVERY_PACKET_LEN] {
        let mut out = [0_u8; DISCOVERY_PACKET_LEN];
        out[..16].copy_from_slice(&DISCOVERY_PREFIX);
        out[16..24].copy_from_slice(&self.protocol_id.as_bytes());

        let hostname = normalize_hostname(&self.hostname);
        out[24..56].copy_from_slice(&hostname);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < DISCOVERY_PACKET_LEN {
            return None;
        }
        if bytes[..16] != DISCOVERY_PREFIX {
            return None;
        }

        let mut protocol_id = [0_u8; 8];
        protocol_id.copy_from_slice(&bytes[16..24]);

        let hostname_raw = &bytes[24..56];
        let hostname = String::from_utf8(
            hostname_raw
                .iter()
                .copied()
                .take_while(|byte| *byte != 0)
                .collect(),
        )
        .ok()?;

        Some(Self {
            protocol_id: ProtocolId(protocol_id),
            hostname,
        })
    }
}

pub fn normalize_hostname(hostname: &str) -> [u8; 32] {
    let mut out = [0_u8; 32];
    let bytes = hostname.as_bytes();
    let len = bytes.len().min(out.len());
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_id_is_deterministic() {
        let a = ProtocolId::from_version("1.2.3");
        let b = ProtocolId::from_version("1.2.3");
        assert_eq!(a, b);
    }

    #[test]
    fn discovery_packet_round_trips() {
        let packet = DiscoveryPacket {
            protocol_id: ProtocolId::from_version("1.2.3"),
            hostname: "pimax-crystal".to_string(),
        };

        let bytes = packet.encode();
        let decoded = DiscoveryPacket::decode(&bytes).expect("decode");
        assert_eq!(decoded, packet);
    }
}
