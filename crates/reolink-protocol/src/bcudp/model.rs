pub const MAGIC_HEADER_UDP_NEGO: u32 = 0x2a87cf3a;
pub const MAGIC_HEADER_UDP_ACK: u32 = 0x2a87cf20;
pub const MAGIC_HEADER_UDP_DATA: u32 = 0x2a87cf10;

/// A negotiation packet, exchanged with relays/devices during UID resolution
/// and connection setup. `tid` doubles as the XML encryption offset.
#[derive(Debug, Clone, PartialEq)]
pub struct UdpDiscovery {
    pub tid: u32,
    pub payload: crate::bcudp::xml::UdpXml,
}

/// Acknowledges receipt of data packets up to and including `packet_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpAck {
    pub connection_id: i32,
    pub group_id: u32,
    pub packet_id: u32,
    pub maybe_latency: u32,
    pub payload: Vec<u8>,
}

/// One (possibly fragmented) chunk of a Bc message sent over the negotiated
/// UDP connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpData {
    pub connection_id: i32,
    pub packet_id: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BcUdp {
    Discovery(UdpDiscovery),
    Ack(UdpAck),
    Data(UdpData),
}
