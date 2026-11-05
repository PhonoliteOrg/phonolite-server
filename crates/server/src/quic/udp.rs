//! Packet destination/source metadata for wildcard and dual-stack listeners.

use std::collections::VecDeque;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::sync::Mutex;

use tokio::io::Interest;
use tokio::net::UdpSocket;

pub(super) struct DatagramSocket {
    socket: UdpSocket,
    state: quinn_udp::UdpSocketState,
    bound: SocketAddr,
    // At most the remainder of one receive buffer, including GRO segments.
    pending: Mutex<VecDeque<ReceivedDatagram>>,
}

struct ReceivedDatagram {
    bytes: Vec<u8>,
    from: SocketAddr,
    to: SocketAddr,
}

impl DatagramSocket {
    pub(super) async fn bind(address: &str) -> io::Result<Self> {
        let socket = UdpSocket::bind(address).await?;
        let bound = socket.local_addr()?;
        let state = quinn_udp::UdpSocketState::new((&socket).into())?;
        Ok(Self {
            socket,
            state,
            bound,
            pending: Mutex::new(VecDeque::new()),
        })
    }

    pub(super) fn local_addr(&self) -> SocketAddr {
        self.bound
    }

    pub(super) fn receive_capacity(&self, max_datagram_size: usize) -> usize {
        receive_capacity(max_datagram_size, self.state.gro_segments())
    }

    pub(super) async fn writable(&self) -> io::Result<()> {
        self.socket.writable().await
    }

    pub(super) async fn recv(
        &self,
        output: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, SocketAddr)> {
        if let Some(packet) = self.pending.lock().unwrap().pop_front() {
            if packet.bytes.len() > output.len() {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            output[..packet.bytes.len()].copy_from_slice(&packet.bytes);
            return Ok((packet.bytes.len(), packet.from, packet.to));
        }
        loop {
            self.socket.readable().await?;
            let mut metadata = [quinn_udp::RecvMeta::default()];
            let result = self.socket.try_io(Interest::READABLE, || {
                self.state.recv(
                    (&self.socket).into(),
                    &mut [IoSliceMut::new(output)],
                    &mut metadata,
                )
            });
            match result {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error),
                Ok(0) => continue,
                Ok(_) => {
                    return split_received(
                        output,
                        metadata[0],
                        self.bound,
                        &mut self.pending.lock().unwrap(),
                    );
                }
            }
        }
    }

    pub(super) fn try_send(
        &self,
        bytes: &[u8],
        from: SocketAddr,
        to: SocketAddr,
    ) -> io::Result<usize> {
        let transmit = transmit(bytes, from, to, self.bound)?;
        self.socket.try_io(Interest::WRITABLE, || {
            self.state.try_send((&self.socket).into(), &transmit)
        })?;
        Ok(bytes.len())
    }
}

fn receive_capacity(max_datagram_size: usize, gro_segments: usize) -> usize {
    // GRO may combine many legal packets into a buffer larger than one UDP
    // datagram. Also retain capacity for a complete non-coalesced datagram.
    65_535.max(max_datagram_size.saturating_mul(gro_segments.clamp(1, 64)))
}

fn canonical_address(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(address) => address
            .ip()
            .to_ipv4_mapped()
            .map(|ip| SocketAddr::new(ip.into(), address.port()))
            .unwrap_or(SocketAddr::V6(address)),
        address => address,
    }
}

fn local_destination(bound: SocketAddr, destination: Option<IpAddr>) -> io::Result<SocketAddr> {
    let address = match destination {
        Some(ip) if !ip.is_unspecified() => {
            let mut address = SocketAddr::new(ip, bound.port());
            if let (SocketAddr::V6(address), SocketAddr::V6(bound)) = (&mut address, bound) {
                address.set_scope_id(bound.scope_id());
                if address.ip().is_unicast_link_local() && address.scope_id() == 0 {
                    // RecvMeta supplies the destination IP but not its interface
                    // index. Refuse an ambiguous scoped path instead of guessing.
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "scoped IPv6 destination requires an explicit interface bind",
                    ));
                }
            }
            address
        }
        _ if !bound.ip().is_unspecified() => bound,
        // Guessing a wildcard address would register the wrong QUIC path.
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "UDP destination address missing",
            ))
        }
    };
    Ok(canonical_address(address))
}

fn split_received(
    output: &[u8],
    metadata: quinn_udp::RecvMeta,
    bound: SocketAddr,
    pending: &mut VecDeque<ReceivedDatagram>,
) -> io::Result<(usize, SocketAddr, SocketAddr)> {
    if metadata.len > output.len()
        || metadata.stride == 0
        || metadata.len.div_ceil(metadata.stride) > 64
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let from = canonical_address(metadata.addr);
    let to = local_destination(bound, metadata.dst_ip)?;
    let mut segments = output[..metadata.len].chunks(metadata.stride);
    let first = segments.next().ok_or(io::ErrorKind::InvalidData)?.len();
    pending.extend(segments.map(|bytes| ReceivedDatagram {
        bytes: bytes.to_vec(),
        from,
        to,
    }));
    Ok((first, from, to))
}

fn transmit<'a>(
    bytes: &'a [u8],
    from: SocketAddr,
    to: SocketAddr,
    bound: SocketAddr,
) -> io::Result<quinn_udp::Transmit<'a>> {
    let from = canonical_address(from);
    let to = canonical_address(to);
    if from.port() != bound.port() || from.ip().is_unspecified() || from.is_ipv4() != to.is_ipv4() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    if !bound.ip().is_unspecified() && canonical_address(bound) != from {
        return Err(io::ErrorKind::AddrNotAvailable.into());
    }
    let destination = match (bound, to) {
        (SocketAddr::V6(_), SocketAddr::V4(to)) => {
            SocketAddr::V6(SocketAddrV6::new(to.ip().to_ipv6_mapped(), to.port(), 0, 0))
        }
        (SocketAddr::V4(_), SocketAddr::V6(_)) => {
            return Err(io::ErrorKind::AddrNotAvailable.into())
        }
        (_, to) => to,
    };
    Ok(quinn_udp::Transmit {
        destination,
        ecn: None,
        contents: bytes,
        segment_size: None,
        src_ip: Some(from.ip()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_receive_requires_destination_metadata_and_normalizes_dual_stack() {
        let bound = "[::]:3001".parse().unwrap();
        assert!(local_destination(bound, None).is_err());
        assert_eq!(
            local_destination(bound, Some("::ffff:192.0.2.8".parse().unwrap())).unwrap(),
            "192.0.2.8:3001".parse().unwrap()
        );
        assert_eq!(
            local_destination("[2001:db8::1]:3001".parse().unwrap(), None).unwrap(),
            "[2001:db8::1]:3001".parse().unwrap()
        );
    }

    #[test]
    fn gro_segments_keep_their_original_addresses_and_boundaries() {
        let bytes = [1, 2, 3, 4, 5];
        let mut pending = VecDeque::new();
        let metadata = quinn_udp::RecvMeta {
            addr: "[::ffff:198.51.100.2]:4444".parse().unwrap(),
            len: 5,
            stride: 2,
            dst_ip: Some("192.0.2.8".parse().unwrap()),
            ..Default::default()
        };
        assert_eq!(
            split_received(&bytes, metadata, "[::]:3001".parse().unwrap(), &mut pending).unwrap(),
            (
                2,
                "198.51.100.2:4444".parse().unwrap(),
                "192.0.2.8:3001".parse().unwrap()
            )
        );
        assert_eq!(
            pending
                .iter()
                .map(|packet| packet.bytes.clone())
                .collect::<Vec<_>>(),
            [vec![3, 4], vec![5]]
        );
        assert!(pending
            .iter()
            .all(|packet| packet.to == "192.0.2.8:3001".parse().unwrap()));
    }

    #[test]
    fn maximum_quic_gro_batch_exceeds_single_udp_size_without_truncation() {
        let capacity = receive_capacity(1350, 64);
        assert_eq!(capacity, 86_400);
        let bytes = vec![7; capacity];
        let mut pending = VecDeque::new();
        let metadata = quinn_udp::RecvMeta {
            addr: "198.51.100.2:4444".parse().unwrap(),
            len: capacity,
            stride: 1350,
            dst_ip: Some("192.0.2.8".parse().unwrap()),
            ..Default::default()
        };
        let (first, _, _) = split_received(
            &bytes,
            metadata,
            "0.0.0.0:3001".parse().unwrap(),
            &mut pending,
        )
        .unwrap();
        assert_eq!(first, 1350);
        assert_eq!(pending.len(), 63);
        assert!(pending.iter().all(|packet| packet.bytes.len() == 1350));
        assert_eq!(
            first
                + pending
                    .iter()
                    .map(|packet| packet.bytes.len())
                    .sum::<usize>(),
            capacity
        );
    }

    #[test]
    fn outgoing_path_preserves_source_and_maps_only_the_socket_destination() {
        let packet = transmit(
            &[1],
            "192.0.2.8:3001".parse().unwrap(),
            "198.51.100.2:4444".parse().unwrap(),
            "[::]:3001".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(packet.src_ip, Some("192.0.2.8".parse().unwrap()));
        assert_eq!(
            packet.destination,
            "[::ffff:198.51.100.2]:4444".parse().unwrap()
        );
        assert!(transmit(
            &[],
            "0.0.0.0:3001".parse().unwrap(),
            "198.51.100.2:4444".parse().unwrap(),
            "0.0.0.0:3001".parse().unwrap()
        )
        .is_err());
        assert!(transmit(
            &[],
            "192.0.2.8:3002".parse().unwrap(),
            "198.51.100.2:4444".parse().unwrap(),
            "0.0.0.0:3001".parse().unwrap()
        )
        .is_err());
    }
}
