use etherparse::{SlicedPacket, TransportSlice, UdpHeader};
use tracing::{debug, info, instrument, trace, warn};

use crate::{ConnectionPacket, PacketDirection};

#[instrument(skip_all)]
pub fn parse_connection_packet(port_filter: &[u16], bytes: Vec<u8>) -> Option<ConnectionPacket> {
    let (udp, payload) = parse_udp(bytes)?;
    let direction = validate_ports(port_filter, udp)?;

    if payload.len() <= 20 {
        if payload.len() < 4 {
            return None;
        }
        let code = u32::from_be_bytes(payload[..4].try_into().unwrap());
        let conv_id = payload
            .get(4..8)
            .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()));
        match code {
            0xFF if direction == PacketDirection::Sent => {
                info!("handshake requested");
                Some(ConnectionPacket::HandshakeRequested)
            },
            404 => {
                warn!("disconnected packet");
                Some(ConnectionPacket::Disconnected { conv_id })
            },
            0x145 if direction == PacketDirection::Received => {
                trace!("handshake established");
                Some(ConnectionPacket::HandshakeEstablished { conv_id })
            },
            _ => None,
        }
    } else {
        Some(ConnectionPacket::SegmentData(direction, payload))
    }
}

#[instrument(skip_all, fields(len = data.len()))]
pub fn parse_udp(data: Vec<u8>) -> Option<(UdpHeader, Vec<u8>)> {
    let packet = match SlicedPacket::from_ethernet(&data) {
        Ok(p) => p,
        Err(e) => {
            debug!("failed: {e}");
            return None;
        },
    };

    // sanity checking the pcap filters
    let Some(transport) = packet.transport else {
        debug!("transport was not present");
        return None;
    };

    let TransportSlice::Udp(udp) = transport else {
        debug!("packet was not udp");
        return None;
    };

    trace!("complete");

    Some((udp.to_header(), udp.payload().to_vec()))
}

fn validate_ports(port_filter: &[u16], udp: UdpHeader) -> Option<PacketDirection> {
    let (src, dest) = (udp.source_port, udp.destination_port);
    if port_filter.contains(&src) {
        Some(PacketDirection::Received)
    } else if port_filter.contains(&dest) {
        Some(PacketDirection::Sent)
    } else {
        trace!(src, dest, "incorrect ports");
        return None;
    }
}
