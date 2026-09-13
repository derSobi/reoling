use reolink_protocol::bc::codec::{read_bc, write_bc};
use reolink_protocol::bc::model::Bc;
use reolink_protocol::bcudp::codec::{read_bcudp, write_bcudp};
use reolink_protocol::bcudp::model::{BcUdp, UdpAck, UdpData};
use reolink_protocol::crypto::EncryptionProtocol;
use crate::transport::discovery::PeerHandle;
use crate::Error;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

/// Leaves headroom under the negotiated 1350-byte MTU for the UdpData
/// header (20 bytes) and IP/UDP overhead.
const MAX_FRAGMENT_SIZE: usize = 1300;

/// `recv_bc`'s receive loop skips discovery-channel packets it doesn't act
/// on (see below) rather than erroring — without a bound on the whole loop,
/// a peer that only ever sends chatter (or nothing at all) on that channel
/// hangs the call forever. Matches `transport::discovery`'s own
/// `OVERALL_TIMEOUT`.
const RECV_TIMEOUT: Duration = Duration::from_secs(15);

pub struct BcConnection {
    socket: Arc<UdpSocket>,
    peer: PeerHandle,
    send_packet_id: u32,
    next_expected_packet_id: u32,
    out_of_order: BTreeMap<u32, Vec<u8>>,
    reassembly: Vec<u8>,
}

impl BcConnection {
    pub fn new(socket: Arc<UdpSocket>, peer: PeerHandle) -> Self {
        Self {
            socket,
            peer,
            send_packet_id: 0,
            next_expected_packet_id: 0,
            out_of_order: BTreeMap::new(),
            reassembly: Vec::new(),
        }
    }

    /// The login nonce delivered during the relay handshake, if any — see
    /// [`PeerHandle::nonce`].
    pub fn peer_nonce(&self) -> Option<&str> {
        self.peer.nonce.as_deref()
    }

    /// The peer's address this connection is actually talking to —
    /// diagnostic only, to tell a direct connection apart from a relay one.
    pub fn peer_addr(&self) -> std::net::SocketAddr {
        self.peer.addr
    }

    /// On a direct (non-relay) connection, spawns a background task that
    /// sends `C2D_HB` to the device once a second for as long as the
    /// returned handle is held. No-op (returns `None`) on a relay
    /// connection. Real hardware, confirmed 2026-09-13: without this, the
    /// device just keeps retransmitting its `D2C_C_R` handshake reply every
    /// ~500ms and never processes any BC data sent to it — see
    /// `UdpXml::C2dHb`.
    pub fn spawn_direct_keepalive(&self) -> Option<tokio::task::JoinHandle<()>> {
        if !self.peer.is_direct {
            return None;
        }
        let socket = self.socket.clone();
        let addr = self.peer.addr;
        let cid = self.peer.local_connection_id;
        let did = self.peer.remote_connection_id;
        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let hb = BcUdp::Discovery(reolink_protocol::bcudp::model::UdpDiscovery {
                    tid: rand::random::<u32>().max(1),
                    payload: reolink_protocol::bcudp::xml::UdpXml::C2dHb(reolink_protocol::bcudp::xml::C2dHb { cid, did }),
                });
                if socket.send_to(&write_bcudp(&hb), addr).await.is_err() {
                    break;
                }
            }
        }))
    }

    pub async fn send_bc(&mut self, bc: &Bc, enc: &EncryptionProtocol) -> crate::Result<()> {
        let bytes = write_bc(bc, enc);
        for chunk in bytes.chunks(MAX_FRAGMENT_SIZE) {
            let packet = BcUdp::Data(UdpData {
                connection_id: self.peer.remote_connection_id,
                packet_id: self.send_packet_id,
                payload: chunk.to_vec(),
            });
            self.socket
                .send_to(&write_bcudp(&packet), self.peer.addr)
                .await?;
            self.send_packet_id += 1;
        }
        Ok(())
    }

    pub async fn recv_bc(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        // A message already fully reassembled from a previous call?
        if let Some((bc, used)) = read_bc(&self.reassembly, enc)? {
            self.reassembly.drain(..used);
            return Ok(bc);
        }

        tokio::time::timeout(RECV_TIMEOUT, self.recv_bc_loop(enc))
            .await
            .map_err(|_| Error::ProtocolError("timed out waiting for a reply".to_string()))?
    }

    async fn recv_bc_loop(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        let mut buf = [0u8; 2048];
        loop {
            let (n, from) = self.socket.recv_from(&mut buf).await?;
            if from != self.peer.addr {
                // Ignore datagrams from anyone but our negotiated peer —
                // the socket is unconnected, so without this check any host
                // that can reach our ephemeral port could inject or
                // overwrite fragments (spoofed duplicate packet_id).
                continue;
            }
            // A discovery-channel packet we don't (yet) model fails to
            // parse inside read_bcudp itself; that shouldn't kill an
            // otherwise-healthy connection, so skip it rather than
            // propagating — real hardware sends several kinds of chatter on
            // this channel we don't need to act on. `D2C_DISC` (below) is
            // the one variant worth surfacing, since it means the session
            // is actually gone.
            let Ok(Some((msg, _))) = read_bcudp(&buf[..n]) else {
                continue;
            };
            match msg {
                BcUdp::Data(data) => {
                    self.out_of_order.insert(data.packet_id, data.payload);
                    while let Some(chunk) =
                        self.out_of_order.remove(&self.next_expected_packet_id)
                    {
                        self.reassembly.extend_from_slice(&chunk);
                        self.next_expected_packet_id += 1;
                    }

                    let ack = BcUdp::Ack(UdpAck {
                        connection_id: self.peer.remote_connection_id,
                        group_id: 0,
                        packet_id: self.next_expected_packet_id.wrapping_sub(1),
                        maybe_latency: 0,
                        payload: vec![],
                    });
                    self.socket
                        .send_to(&write_bcudp(&ack), self.peer.addr)
                        .await?;

                    if let Some((bc, used)) = read_bc(&self.reassembly, enc)? {
                        self.reassembly.drain(..used);
                        return Ok(bc);
                    }
                }
                BcUdp::Ack(_) => continue, // no retransmission tracking needed for the MVP
                BcUdp::Discovery(disc) => match disc.payload {
                    reolink_protocol::bcudp::xml::UdpXml::D2cDisc(_) => {
                        return Err(Error::ProtocolError(
                            "device disconnected the session (D2C_DISC)".to_string(),
                        ))
                    }
                    // Other discovery-channel chatter (our own echoed
                    // C2D_DISC, keepalives, etc.) — not relevant here.
                    _ => continue,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reolink_protocol::bc::model::*;
    use reolink_protocol::crypto::EncryptionProtocol;
    use crate::transport::discovery::PeerHandle;
    use std::sync::Arc;
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn two_connections_round_trip_a_bc_message() {
        let socket_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let socket_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = socket_a.local_addr().unwrap();
        let addr_b = socket_b.local_addr().unwrap();

        let mut conn_a = BcConnection::new(
            socket_a,
            PeerHandle { addr: addr_b, local_connection_id: 1, remote_connection_id: 2, nonce: None, is_direct: false },
        );
        let mut conn_b = BcConnection::new(
            socket_b,
            PeerHandle { addr: addr_a, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false },
        );

        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(b"<?xml version=\"1.0\"?><body/>".to_vec()),
            }),
        };

        conn_a.send_bc(&bc, &EncryptionProtocol::Unencrypted).await.unwrap();
        let received = conn_b.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap();
        assert_eq!(received, bc);
    }

    #[tokio::test]
    async fn a_large_message_is_fragmented_and_reassembled() {
        let socket_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let socket_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = socket_a.local_addr().unwrap();
        let addr_b = socket_b.local_addr().unwrap();

        let mut conn_a = BcConnection::new(
            socket_a,
            PeerHandle { addr: addr_b, local_connection_id: 1, remote_connection_id: 2, nonce: None, is_direct: false },
        );
        let mut conn_b = BcConnection::new(
            socket_b,
            PeerHandle { addr: addr_a, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false },
        );

        let big_payload = vec![0xABu8; 5000]; // several times MAX_FRAGMENT_SIZE
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 0,
                stream_type: 0,
                msg_num: 2,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(big_payload),
            }),
        };

        conn_a.send_bc(&bc, &EncryptionProtocol::Unencrypted).await.unwrap();
        let received = conn_b.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap();
        assert_eq!(received, bc);
    }
}
