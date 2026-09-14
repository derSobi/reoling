use reolink_protocol::bc::codec::{read_bc, write_bc};
use reolink_protocol::bc::model::Bc;
use reolink_protocol::bcudp::codec::{read_bcudp, write_bcudp};
use reolink_protocol::bcudp::model::{BcUdp, UdpAck, UdpData};
use reolink_protocol::crypto::EncryptionProtocol;
use crate::transport::discovery::PeerHandle;
use crate::Error;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// Leaves headroom under the negotiated 1350-byte MTU for the UdpData
/// header (20 bytes) and IP/UDP overhead. Only meaningful for the UDP
/// variant — TCP has no such envelope and sends `write_bc`'s output
/// unchunked.
const MAX_FRAGMENT_SIZE: usize = 1300;

/// Upper bound on how many entries an ack's missing-packet bitmap covers
/// (see `build_ack_payload`) — a stray/malicious `packet_id` far ahead of
/// `next_expected_packet_id` must not drive a multi-GiB allocation here.
const ACK_BITMAP_CAP: u32 = 4096;

/// `recv_bc`'s receive loop skips discovery-channel packets it doesn't act
/// on (see below) rather than erroring — without a bound on the whole loop,
/// a peer that only ever sends chatter (or nothing at all) on that channel
/// hangs the call forever. Matches `transport::discovery`'s own
/// `OVERALL_TIMEOUT`. Applies to both variants.
const RECV_TIMEOUT: Duration = Duration::from_secs(15);

/// Builds the ack payload documented in `UdpAck`: a `00`/`01` truth table
/// for every packet_id after the one this ack covers, saying which of them
/// have *already* been received out of order. Real hardware resends
/// anything past the acked `packet_id` it doesn't otherwise hear was
/// received — without this, our own acks (previously always empty) gave it
/// no way to tell an already-reassembled out-of-order packet apart from a
/// genuinely lost one, which is one plausible explanation for the roughly
/// one-second bursty stutter seen on the relay-only (non-direct) path;
/// matches `bairelay`'s own `build_send_ack`, the only reference among the
/// three read for this project that actually populates this field instead
/// of leaving it empty like `neolink` does.
fn build_ack_payload(next_expected: u32, out_of_order: &BTreeMap<u32, Vec<u8>>) -> Vec<u8> {
    let Some(&highest) = out_of_order.keys().next_back() else {
        return Vec::new();
    };
    let end_exclusive = highest.saturating_add(1).min(next_expected.saturating_add(ACK_BITMAP_CAP));
    (next_expected..end_exclusive).map(|id| u8::from(out_of_order.contains_key(&id))).collect()
}

/// The two ways a `BcConnection` can actually be talking to a device.
/// `Bc`-level framing (`read_bc`/`write_bc`) and encryption are identical
/// either way — only the raw bytes-on-the-wire mechanics differ, so this
/// enum is the only place that knows the difference.
enum Socket {
    /// P2P (UID-resolved, direct or relay). Needs the `BcUdp` envelope:
    /// fragmentation into `MAX_FRAGMENT_SIZE` chunks, ACKs, out-of-order
    /// reassembly by `packet_id`, and a negotiated peer to validate
    /// incoming datagrams against.
    Udp {
        socket: Arc<UdpSocket>,
        peer: PeerHandle,
        send_packet_id: u32,
        next_expected_packet_id: u32,
        out_of_order: BTreeMap<u32, Vec<u8>>,
    },
    /// Direct TCP (Baichuan's "Basic Service", typically port 9000). A
    /// plain ordered byte stream — `write_bc`'s output goes straight on
    /// the wire with no envelope, and `read_bc` already knows how to wait
    /// for "not enough bytes yet" (`Ok(None)`), which is exactly what an
    /// incrementally-filled TCP buffer needs.
    Tcp { stream: TcpStream },
}

pub struct BcConnection {
    socket: Socket,
    reassembly: Vec<u8>,
    // msg_nums a prior message told us (via <binaryData>) are mid video/audio
    // stream — see `read_bc`'s own doc comment for why this matters for
    // decryption of the chunks that follow.
    bin_mode: HashSet<u16>,
}

impl BcConnection {
    pub fn new(socket: Arc<UdpSocket>, peer: PeerHandle) -> Self {
        Self {
            socket: Socket::Udp {
                socket,
                peer,
                send_packet_id: 0,
                next_expected_packet_id: 0,
                out_of_order: BTreeMap::new(),
            },
            reassembly: Vec::new(),
            bin_mode: HashSet::new(),
        }
    }

    /// The login nonce delivered during the relay handshake, if any — see
    /// [`PeerHandle::nonce`]. Always `None` on a direct TCP connection —
    /// that path has no P2P handshake to carry one; `login()` falls back
    /// to the legacy nonce exchange exactly as it does for a nonce-less
    /// UDP connection.
    pub fn peer_nonce(&self) -> Option<&str> {
        match &self.socket {
            Socket::Udp { peer, .. } => peer.nonce.as_deref(),
            Socket::Tcp { .. } => None,
        }
    }

    /// The peer's address this connection is actually talking to —
    /// diagnostic only, to tell a direct connection apart from a relay
    /// one. `TcpStream::peer_addr()` succeeds on any connected socket —
    /// every real construction of `Socket::Tcp` (via `from_tcp`, whether
    /// fed a `TcpStream::connect` result in `ReolinkClient::connect_by_ip`
    /// or a `TcpListener::accept()` result in tests) already holds a
    /// connected stream, so the `.expect()` here can't actually fail.
    pub fn peer_addr(&self) -> std::net::SocketAddr {
        match &self.socket {
            Socket::Udp { peer, .. } => peer.addr,
            Socket::Tcp { stream } => stream
                .peer_addr()
                .expect("peer_addr always succeeds after a successful TcpStream::connect"),
        }
    }

    /// Wraps an already-connected `TcpStream` (Baichuan's direct TCP
    /// "Basic Service", typically port 9000) as a `BcConnection`.
    pub fn from_tcp(stream: TcpStream) -> Self {
        Self {
            socket: Socket::Tcp { stream },
            reassembly: Vec::new(),
            bin_mode: HashSet::new(),
        }
    }

    /// On a direct (non-relay) **UDP** connection, spawns a background task
    /// that sends `C2D_HB` to the device once a second for as long as the
    /// returned handle is held. No-op (returns `None`) on a relay
    /// connection, and always `None` on TCP — `C2D_HB` is a P2P/UDP NAT
    /// keepalive with no TCP equivalent; a stable TCP connection needs no
    /// such mechanism and must not send one. Real hardware, confirmed
    /// 2026-09-13: without this on the UDP direct path, the device just
    /// keeps retransmitting its `D2C_C_R` handshake reply every ~500ms and
    /// never processes any BC data sent to it — see `UdpXml::C2dHb`.
    pub fn spawn_direct_keepalive(&self) -> Option<tokio::task::JoinHandle<()>> {
        let Socket::Udp { socket, peer, .. } = &self.socket else {
            return None;
        };
        if !peer.is_direct {
            return None;
        }
        let socket = socket.clone();
        let addr = peer.addr;
        let cid = peer.local_connection_id;
        let did = peer.remote_connection_id;
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
        match &mut self.socket {
            Socket::Udp { socket, peer, send_packet_id, .. } => {
                for chunk in bytes.chunks(MAX_FRAGMENT_SIZE) {
                    let packet = BcUdp::Data(UdpData {
                        connection_id: peer.remote_connection_id,
                        packet_id: *send_packet_id,
                        payload: chunk.to_vec(),
                    });
                    socket.send_to(&write_bcudp(&packet), peer.addr).await?;
                    *send_packet_id += 1;
                }
                Ok(())
            }
            Socket::Tcp { stream } => {
                stream.write_all(&bytes).await?;
                Ok(())
            }
        }
    }

    pub async fn recv_bc(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        // A message already fully reassembled from a previous call?
        if let Some((bc, used)) = read_bc(&self.reassembly, enc, &mut self.bin_mode)? {
            self.reassembly.drain(..used);
            return Ok(bc);
        }

        tokio::time::timeout(RECV_TIMEOUT, self.recv_bc_loop(enc))
            .await
            .map_err(|_| Error::ProtocolError("timed out waiting for a reply".to_string()))?
    }

    async fn recv_bc_loop(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        if matches!(self.socket, Socket::Tcp { .. }) {
            self.recv_bc_loop_tcp(enc).await
        } else {
            self.recv_bc_loop_udp(enc).await
        }
    }

    async fn recv_bc_loop_tcp(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        let mut buf = [0u8; 2048];
        loop {
            // Scoped to just this read: the mutable borrow of `self.socket`
            // must not overlap the `self.reassembly` access below it, or
            // the borrow checker sees two live mutable borrows of `self`
            // across the same `.await`.
            let n = {
                let Socket::Tcp { stream } = &mut self.socket else {
                    unreachable!("recv_bc_loop_tcp called on a non-TCP connection");
                };
                stream.read(&mut buf).await?
            };
            if n == 0 {
                // The peer closed the connection — there is no BC-level
                // "goodbye" on this transport the way `D2C_DISC` is one on
                // UDP; a clean TCP EOF just means the session is over.
                return Err(Error::ConnectionLost);
            }
            self.reassembly.extend_from_slice(&buf[..n]);
            if let Some((bc, used)) = read_bc(&self.reassembly, enc, &mut self.bin_mode)? {
                self.reassembly.drain(..used);
                return Ok(bc);
            }
        }
    }

    async fn recv_bc_loop_udp(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        let mut buf = [0u8; 2048];
        loop {
            let (n, from) = {
                let Socket::Udp { socket, .. } = &self.socket else {
                    unreachable!("recv_bc_loop_udp called on a non-UDP connection");
                };
                socket.recv_from(&mut buf).await?
            };
            let Socket::Udp { peer, .. } = &self.socket else {
                unreachable!("recv_bc_loop_udp called on a non-UDP connection");
            };
            if from != peer.addr {
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
                    let Socket::Udp { socket, peer, next_expected_packet_id, out_of_order, .. } =
                        &mut self.socket
                    else {
                        unreachable!("recv_bc_loop_udp called on a non-UDP connection");
                    };
                    out_of_order.insert(data.packet_id, data.payload);
                    while let Some(chunk) = out_of_order.remove(next_expected_packet_id) {
                        self.reassembly.extend_from_slice(&chunk);
                        *next_expected_packet_id += 1;
                    }

                    let ack = BcUdp::Ack(UdpAck {
                        connection_id: peer.remote_connection_id,
                        group_id: 0,
                        packet_id: next_expected_packet_id.wrapping_sub(1),
                        maybe_latency: 0,
                        payload: build_ack_payload(*next_expected_packet_id, out_of_order),
                    });
                    socket.send_to(&write_bcudp(&ack), peer.addr).await?;

                    if let Some((bc, used)) = read_bc(&self.reassembly, enc, &mut self.bin_mode)? {
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

    #[test]
    fn ack_payload_marks_out_of_order_packets_received_and_gaps_missing() {
        // next_expected_packet_id is 6 (the gap); 7 and 9 arrived out of
        // order, 8 did not.
        let mut out_of_order = BTreeMap::new();
        out_of_order.insert(7u32, vec![]);
        out_of_order.insert(9u32, vec![]);
        assert_eq!(build_ack_payload(6, &out_of_order), vec![0, 1, 0, 1]);
    }

    #[test]
    fn ack_payload_is_empty_when_nothing_arrived_out_of_order() {
        assert_eq!(build_ack_payload(6, &BTreeMap::new()), Vec::<u8>::new());
    }

    #[test]
    fn ack_payload_is_capped_against_a_wild_far_ahead_packet_id() {
        let mut out_of_order = BTreeMap::new();
        out_of_order.insert(6u32, vec![]);
        out_of_order.insert(u32::MAX, vec![]); // absurdly far ahead
        assert_eq!(build_ack_payload(6, &out_of_order).len(), ACK_BITMAP_CAP as usize);
    }

    #[tokio::test]
    async fn two_tcp_connections_round_trip_a_bc_message() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = BcConnection::from_tcp(stream);
            conn.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap()
        });

        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut client_conn = BcConnection::from_tcp(client_stream);

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
        client_conn.send_bc(&bc, &EncryptionProtocol::Unencrypted).await.unwrap();

        let received = server.await.unwrap();
        assert_eq!(received, bc);
    }

    #[tokio::test]
    async fn a_large_tcp_message_is_read_incrementally_and_reassembled() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = BcConnection::from_tcp(stream);
            conn.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap()
        });

        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut client_conn = BcConnection::from_tcp(client_stream);

        // Several times the TCP read buffer (2048 bytes in recv_bc_loop_tcp),
        // forcing multiple `stream.read()` calls through the reassembly loop
        // — the same value the UDP fragmentation test uses.
        let big_payload = vec![0xABu8; 5000];
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
        client_conn.send_bc(&bc, &EncryptionProtocol::Unencrypted).await.unwrap();

        let received = server.await.unwrap();
        assert_eq!(received, bc);
    }

    #[tokio::test]
    async fn tcp_connection_closed_by_peer_surfaces_as_connection_lost() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream); // close without sending anything
        });

        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut client_conn = BcConnection::from_tcp(client_stream);

        server.await.unwrap();
        let result = client_conn.recv_bc(&EncryptionProtocol::Unencrypted).await;
        assert!(matches!(result, Err(Error::ConnectionLost)));
    }
}
