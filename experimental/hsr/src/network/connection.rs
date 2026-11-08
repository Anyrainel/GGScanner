// Adapted from IceDynamix/reliquary d5cf3b7 (MIT); see THIRD_PARTY_NOTICES.md.
use etherparse::err::packet::SliceError;
use etherparse::{SlicedPacket, TransportSlice, UdpHeader};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use crate::network::{ConnectionPacket, PacketDirection};

#[instrument(skip_all)]
pub fn parse_connection_packet(
    port_filter: &[u16],
    bytes: Vec<u8>,
) -> Result<ConnectionPacket, ConnectionPacketError> {
    let (udp, payload) = parse_udp(bytes)?;
    let direction = validate_ports(&port_filter, udp)?;

    if payload.len() <= 20 {
        if payload.len() < 8 {
            return Err(ConnectionPacketError::PayloadTooShort);
        }

        let code = u32::from_be_bytes(payload[..4].try_into().unwrap());
        match code {
            0xFF => {
                info!("handshake requested");
                Ok(ConnectionPacket::HandshakeRequested)
            },
            404 => {
                warn!("disconnected packet");
                // The same connection ID as establishment occupies bytes 4..8.
                let conv_id = u32::from_be_bytes(payload[4..8].try_into().unwrap());
                Ok(ConnectionPacket::Disconnected { conv_id })
            },
            _ => {
                let conv_id = u32::from_be_bytes(payload[4..8].try_into().unwrap());
                Ok(ConnectionPacket::HandshakeEstablished { conv_id })
            },
        }
    } else {
        Ok(ConnectionPacket::SegmentData(direction, payload))
    }
}

#[instrument(skip_all, fields(len = data.len()))]
pub fn parse_udp(data: Vec<u8>) -> Result<(UdpHeader, Vec<u8>), ConnectionPacketError> {
    let packet: SlicedPacket = SlicedPacket::from_ethernet(&data)?;

    // sanity checking the pcap filters
    let Some(transport) = packet.transport else {
        debug!("transport was not present");
        return Err(ConnectionPacketError::TransportLayerNotPresent);
    };

    let TransportSlice::Udp(udp) = transport else {
        debug!("packet was not udp");
        return Err(ConnectionPacketError::TransportLayerNotUdp);
    };

    Ok((udp.to_header(), udp.payload().to_vec()))
}

fn validate_ports(
    port_filter: &[u16],
    udp: UdpHeader,
) -> Result<PacketDirection, ConnectionPacketError> {
    let (src, dest) = (udp.source_port, udp.destination_port);
    if port_filter.contains(&src) {
        Ok(PacketDirection::Received)
    } else if port_filter.contains(&dest) {
        Ok(PacketDirection::Sent)
    } else {
        return Err(ConnectionPacketError::IncorrectPorts);
    }
}

#[derive(Error, Debug)]
pub enum ConnectionPacketError {
    #[error("error while parsing network packet: {0}")]
    EtherparsePacketError(#[from] SliceError),
    #[error("transport layer is not present on packet")]
    TransportLayerNotPresent,
    #[error("transport layer is not udp protocol")]
    TransportLayerNotUdp,
    #[error("packet does not match the required ports")]
    IncorrectPorts,
    #[error("packet payload too short")]
    PayloadTooShort,
}
