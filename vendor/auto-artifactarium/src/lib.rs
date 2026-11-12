//! Parse network packets transmitted between the game and the server
//!
//! Packets are built up in following layers depending on the purpose of the packet:
//!
//! - Packets for connection management ([`GamePacket::Connection`])
//!     - **Ethernet/IP/UDP**, handled using [`etherparse`]
//!     - **[`ConnectionPacket`]**, containing events for connection establishment/disconnection
//! - Packets for game commands ([`GamePacket::Commands`])
//!     - **Ethernet/IP/UDP**, handled using [`etherparse`]
//!     - **KCP**, handled using [`kcp`]
//!         - The KCP header contains an extra field that needs to be removed
//!           to be compatible with the regular KCP protocol
//!     - **[`GameCommand`]**, encrypted using XOR
//!     - **Protobuf**, payload, needs to be parsed into using the types generated in [`gen::proto`]
//!
//! [`GameCommand`]s are encrypted using an XOR-key.
//! One of the first packets sent is a request for a new key from a seed.
//! That key is used for the rest of the packets.
//! This means the recording for packets needs to start before the game starts (train hyperdrive).
//!
//! ## Example
//! ```
//! use auto_artifactarium::{GamePacket, GameSniffer, ConnectionPacket};
//!
//! let packets: Vec<Vec<u8>> = vec![/**/];
//!
//! let mut sniffer = GameSniffer::new();
//! for packet in packets {
//!     match sniffer.receive_packet(packet) {
//!         Some(GamePacket::Connection(ConnectionPacket::Disconnected { .. })) => {
//!             println!("Disconnected!");
//!             break;
//!         }
//!         Some(GamePacket::Commands(commands)) => {
//!             for command in commands {
//!                 println!("{:?}", command);
//!             }
//!         }
//!         _ => {}
//!     }
//! }
//! ```
//!

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::Write;

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use rsa::{RsaPrivateKey, pkcs1::DecodeRsaPrivateKey};
use tracing::{error, info, info_span, instrument, trace, warn};

use crate::connection::parse_connection_packet;
use crate::crypto::{bruteforce, decrypt_command, lookup_initial_key};
// use crate::gen::protos::GetPlayerTokenRsp;
use crate::Key::Dispatch;
use crate::r#gen::protos::PacketHead;
use crate::kcp::KcpSniffer;
pub use crate::unk_util::Achievement;
pub use crate::unk_util::{
    matches_achievement_all_data_notify, matches_avatars_all_data_notify,
    matches_get_player_token_rsp, matches_items_all_data_notify,
};

fn bytes_as_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, b| {
        let _ = write!(output, "{b:02x}");
        output
    })
}

// pub mod command_id;
pub mod r#gen;

mod connection;
mod crypto;
mod cs_rand;
mod identity;
mod kcp;
mod unk_util;

const PORTS: [u16; 2] = [22101, 22102];

/// Top-level packet sent by the game
pub enum GamePacket {
    Connection(ConnectionPacket),
    Commands(Vec<GameCommand>),
}

/// Packet for connection management
pub enum ConnectionPacket {
    HandshakeRequested,
    Disconnected { conv_id: Option<u32> },
    HandshakeEstablished { conv_id: Option<u32> },
    SegmentData(PacketDirection, Vec<u8>),
}

#[repr(u16)]
enum CommandId {
    AvatarDataNotify = 6586,
    PlayerStoreNotify = 8132,
}

/// A framed game command with separate protobuf header and body.
///
/// `header_data` contains PacketHead; `proto_data` contains only the command body.
/// They must be decoded separately because protobuf field numbers overlap.
///
/// ## Bit Layout
/// | Bit indices     |  Type |  Name |
/// | - | - | - |
/// |   0..2      |  `u16`  |  Header (magic constant) |
/// |   2..4      |  `u16`  |  command_id |
/// |   4..6      |  `u16`  |  header_len |
/// |   6..10     |  `u32`  |  data_len |
/// |  10..10+header_len |  variable  |  header_data |
/// |  10+header_len..10+header_len+data_len |  variable  |  proto_data |
/// | final 2 bytes  |  `u16`  |  Tail (magic constant) |
#[derive(Clone)]
pub struct GameCommand {
    pub command_id: u16,
    pub direction: PacketDirection,
    /// An incoming dispatch response whose encrypted session seed was verified.
    /// Never inferred from a command ID or from unrecognized protobuf fields.
    pub is_token_response: bool,
    /// Public game UID validated against this command's supported login layout.
    pub account_uid: Option<u32>,
    #[allow(unused)]
    pub header_len: u16,
    #[allow(unused)]
    pub data_len: u32,
    pub header_data: Vec<u8>,
    pub proto_data: Vec<u8>,
}

impl GameCommand {
    const HEADER_LEN: usize = 10;
    const TAIL_LEN: usize = 2;

    #[instrument(skip(bytes), fields(len = bytes.len()))]
    pub fn try_new(bytes: Vec<u8>, direction: PacketDirection) -> Option<Self> {
        let header_overhead = Self::HEADER_LEN + Self::TAIL_LEN;
        if bytes.len() < header_overhead {
            warn!(len = bytes.len(), "game command header incomplete");
            return None;
        }

        if bytes[0] != 0x45
            || bytes[1] != 0x67
            || bytes[bytes.len() - 2] != 0x89
            || bytes[bytes.len() - 1] != 0xAB
        {
            error!("Didn't get magic in try_new!");
            return None;
        }

        // skip header magic const
        let command_id = u16::from_be_bytes(bytes[2..4].try_into().unwrap());
        let header_len = u16::from_be_bytes(bytes[4..6].try_into().unwrap());
        let data_len = u32::from_be_bytes(bytes[6..10].try_into().unwrap());

        let body_start = Self::HEADER_LEN.checked_add(header_len as usize)?;
        let end = body_start.checked_add(data_len as usize)?;
        if end != bytes.len() - Self::TAIL_LEN {
            return None;
        }
        Some(GameCommand {
            command_id,
            direction,
            is_token_response: false,
            account_uid: None,
            header_len,
            data_len,
            header_data: bytes[Self::HEADER_LEN..body_start].to_vec(),
            proto_data: bytes[body_start..end].to_vec(),
        })
    }

    pub fn parse_header<T: protobuf::Message>(&self) -> protobuf::Result<T> {
        T::parse_from_bytes(&self.header_data)
    }

    /// Reconstruct the complete decrypted frame, retaining the header boundary.
    pub fn to_bytes(&self) -> Vec<u8> {
        let header_len = u16::try_from(self.header_data.len())
            .expect("parsed command header fits its frame length");
        let body_len = u32::try_from(self.proto_data.len())
            .expect("parsed command body fits its frame length");
        let mut bytes = Vec::with_capacity(
            Self::HEADER_LEN + self.header_data.len() + self.proto_data.len() + Self::TAIL_LEN,
        );
        bytes.extend_from_slice(&0x4567u16.to_be_bytes());
        bytes.extend_from_slice(&self.command_id.to_be_bytes());
        bytes.extend_from_slice(&header_len.to_be_bytes());
        bytes.extend_from_slice(&body_len.to_be_bytes());
        bytes.extend_from_slice(&self.header_data);
        bytes.extend_from_slice(&self.proto_data);
        bytes.extend_from_slice(&0x89ABu16.to_be_bytes());
        bytes
    }

    pub fn parse_proto<T: protobuf::Message>(&self) -> protobuf::Result<T> {
        T::parse_from_bytes(&self.proto_data)
    }

    pub fn is_avatar_data_notify(&self) -> bool {
        self.command_id == CommandId::AvatarDataNotify as u16
    }

    pub fn is_player_store_notify(&self) -> bool {
        self.command_id == CommandId::PlayerStoreNotify as u16
    }
}

impl fmt::Debug for GameCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GameCommand")
            .field("command_id", &self.command_id)
            .field("direction", &self.direction)
            .field("is_token_response", &self.is_token_response)
            .field("header_len", &self.header_len)
            .field("data_len", &self.data_len)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum PacketDirection {
    Sent,
    Received,
}

pub enum Key {
    Dispatch(Vec<u8>),
    Session(Vec<u8>),
}

#[derive(Default)]
pub struct GameSniffer {
    last_error: Option<String>,
    rejected_commands: usize,
    sent_kcp: Option<KcpSniffer>,
    recv_kcp: Option<KcpSniffer>,
    client_seed: Option<u64>,
    key: Option<Key>,
    initial_keys: HashMap<u16, Vec<u8>>,
    rsa_keys: Vec<RsaPrivateKey>,
    sent_time: Option<u64>,
    possible_seeds: Vec<u64>,
    active_conv: Option<u32>,
    retired_convs: HashSet<u32>,
    // Initial late-start capture remains allowed. After an explicit disconnect,
    // only a new handshake request can authorize another KCP stream.
    awaiting_handshake: bool,
}

impl GameSniffer {
    /// Diagnostic metadata only; never includes keys, seeds or packet contents.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
    pub fn rejected_commands(&self) -> usize {
        self.rejected_commands
    }
    pub fn new() -> Self {
        let pem_data_4 = include_str!("../keys/private_key_4.pem");
        let pem_data_5 = include_str!("../keys/private_key_5.pem");

        let rsa_4 = RsaPrivateKey::from_pkcs1_pem(pem_data_4);
        let rsa_5 = RsaPrivateKey::from_pkcs1_pem(pem_data_5);

        GameSniffer {
            rsa_keys: vec![rsa_4, rsa_5]
                .iter()
                .filter_map(|rsa_key| rsa_key.clone().ok())
                .collect(),
            ..Default::default()
        }
    }

    pub fn set_initial_keys(mut self, initial_keys: HashMap<u16, Vec<u8>>) -> Self {
        self.initial_keys = initial_keys
            .into_iter()
            .filter(|(_, key)| key.len() == 4096)
            .collect();
        self
    }

    /// A token response provided the seed candidates needed for session decryption.
    /// This does not imply that inventory data has arrived or been decrypted.
    pub fn key_exchange_captured(&self) -> bool {
        !self.possible_seeds.is_empty()
    }

    fn reset_connection_state(&mut self, awaiting_handshake: bool) {
        if let Some(conv_id) = self.active_conv.take() {
            self.retired_convs.insert(conv_id);
        }
        self.recv_kcp = None;
        self.sent_kcp = None;
        self.client_seed = None;
        self.key = None;
        self.possible_seeds.clear();
        self.sent_time = None;
        self.last_error = None;
        self.awaiting_handshake = awaiting_handshake;
    }

    #[instrument(skip_all, fields(len = bytes.len()))]
    pub fn receive_packet(&mut self, bytes: Vec<u8>) -> Option<GamePacket> {
        let packet = match parse_connection_packet(&PORTS, bytes) {
            Some(packet) => packet,
            None => {
                self.last_error = Some("frame is not a supported game UDP packet".into());
                return None;
            },
        };
        match packet {
            ConnectionPacket::HandshakeRequested => {
                info!("handshake requested, resetting state");
                self.reset_connection_state(false);
                Some(GamePacket::Connection(packet))
            },
            ConnectionPacket::Disconnected { conv_id } => {
                if conv_id.is_some_and(|conv_id| {
                    self.active_conv.is_some_and(|current| current != conv_id)
                        || self.retired_convs.contains(&conv_id)
                }) {
                    return None;
                }
                self.reset_connection_state(true);
                self.last_error = Some("game connection closed; capture a new login".into());
                Some(GamePacket::Connection(packet))
            },
            ConnectionPacket::HandshakeEstablished { conv_id } => {
                if let Some(conv_id) = conv_id {
                    if self.awaiting_handshake
                        || self.active_conv.is_some_and(|current| current != conv_id)
                        || self.retired_convs.contains(&conv_id)
                    {
                        return None;
                    }
                    self.active_conv = Some(conv_id);
                }
                Some(GamePacket::Connection(packet))
            },

            ConnectionPacket::SegmentData(direction, kcp_seg) => {
                if self.awaiting_handshake {
                    self.last_error =
                        Some("game connection closed; waiting for a new login handshake".into());
                    return None;
                }
                let Some(conv_id) = kcp::validate_kcp_segment(&kcp_seg) else {
                    self.last_error = Some("incomplete KCP segment header".into());
                    return None;
                };
                if self.retired_convs.contains(&conv_id)
                    || self.active_conv.is_some_and(|current| current != conv_id)
                {
                    self.last_error =
                        Some("KCP segment belongs to a retired or different conversation".into());
                    return None;
                }
                // Both directions belong to the same login. Initial late-start
                // capture can select its first conversation without a handshake.
                self.active_conv = Some(conv_id);
                let commands = self.receive_kcp_segment(direction, &kcp_seg);
                match commands {
                    Some(commands) => Some(GamePacket::Commands(commands)),
                    None => Some(GamePacket::Connection(ConnectionPacket::SegmentData(
                        direction, kcp_seg,
                    ))),
                }
            },
        }
    }

    fn receive_kcp_segment(
        &mut self,
        direction: PacketDirection,
        kcp_seg: &[u8],
    ) -> Option<Vec<GameCommand>> {
        let kcp = match direction {
            PacketDirection::Sent => &mut self.sent_kcp,
            PacketDirection::Received => &mut self.recv_kcp,
        };

        if kcp.is_none() {
            let Some(new_kcp) = KcpSniffer::try_new(kcp_seg) else {
                self.last_error = Some("incomplete KCP segment header".into());
                return None;
            };
            *kcp = Some(new_kcp);
        }

        if let Some(kcp) = kcp {
            let data = match kcp.receive_segments(kcp_seg) {
                Ok(data) => data,
                Err(error) => {
                    self.last_error = Some(error.into());
                    return None;
                },
            };
            let commands = data
                .into_iter()
                .filter_map(|data| self.receive_command(data, direction))
                .collect();

            return Some(commands);
        }

        None
    }

    #[instrument(skip_all, fields(len = data.len()))]
    fn receive_command(
        &mut self,
        mut data: Vec<u8>,
        direction: PacketDirection,
    ) -> Option<GameCommand> {
        if data.len() < 12 {
            self.reject_command("incomplete game command".into());
            return None;
        }
        let key_r = match &self.key {
            None => {
                let key = lookup_initial_key(&self.initial_keys, &data);
                match key {
                    Some(key) => {
                        self.key = Some(Dispatch(key));
                        self.key.as_ref().unwrap()
                    },
                    None => {
                        let version = u16::from_be_bytes([data[0], data[1]]) ^ 0x4567;
                        self.reject_command(format!(
                            "dispatch key not available or invalid; version={version}"
                        ));
                        error!("No dispatch key found");
                        return None;
                    },
                }
            },
            Some(Dispatch(k)) => {
                let mut test = data.clone();
                decrypt_command(k, &mut test);

                if test[0] == 0x45
                    && test[1] == 0x67
                    && test[test.len() - 2] == 0x89
                    && test[test.len() - 1] == 0xAB
                {
                    self.key.as_ref().unwrap()
                } else {
                    let mut discovered_key: Option<&Key> = None;
                    for &seed in &self.possible_seeds {
                        // First try with a retained client seed.
                        if let Some(client_seed) = self.client_seed
                            && let Some((client_seed, key)) =
                                bruteforce(client_seed, seed, data.clone())
                        {
                            self.client_seed = Some(client_seed);
                            self.key = Some(Key::Session(key));
                            discovered_key = self.key.as_ref();
                            break;
                        }

                        // If that fails, try with a client seed generated from the packet's
                        // `sent_time`
                        if let Some((client_seed, key)) = self
                            .sent_time
                            .and_then(|sent_time| bruteforce(sent_time, seed, data.clone()))
                        {
                            self.client_seed = Some(client_seed);
                            self.key = Some(Key::Session(key));
                            discovered_key = self.key.as_ref();
                            break;
                        }
                    }

                    match discovered_key {
                        Some(key) => key,
                        None => {
                            self.reject_command("session decryption failed; login handshake may be missing or protocol unsupported".into());
                            error!("Couldn't bruteforce from deduced keys");
                            return None;
                        },
                    }
                }
            },
            Some(Key::Session(k)) => {
                let mut test = data.clone();
                decrypt_command(k, &mut test);

                if test[0] == 0x45 && test[1] == 0x67 {
                    //|| test[test.len() - 2] == 0x89 && test[test.len() - 1] == 0xAB
                    self.key.as_ref().unwrap()
                } else {
                    warn!("Invalidated session key");
                    self.key = None;
                    self.reject_command("session key invalidated; capture a new login".into());
                    error!("Session key dead, relaunch game");
                    return None;
                }
            },
        };

        let key = match key_r {
            Dispatch(k) | Key::Session(k) => k,
        };

        decrypt_command(key, &mut data);

        let mut command = match GameCommand::try_new(data, direction) {
            Some(command) => command,
            None => {
                self.reject_command("decrypted command has an invalid header or length".into());
                return None;
            },
        };

        let span = info_span!("command", ?command);
        let _enter = span.enter();

        info!("received");
        trace!(data = BASE64_STANDARD.encode(&command.proto_data), "data");

        // if !matches!(
        //     command.command_id,
        //     command_id::GET_PLAYER_TOKEN_RSP | command_id::ACHIEVEMENT_ALL_DATA_NOTIFY
        // ) {
        //     return None;
        // }

        if direction == PacketDirection::Received && matches!(self.key, Some(Dispatch(_))) {
            if let Some(possible_seeds) =
                matches_get_player_token_rsp(command.proto_data.clone(), self.rsa_keys.clone())
            {
                command.is_token_response = true;
                command.account_uid = identity::token_uid(&command.proto_data, &self.rsa_keys);
                self.possible_seeds = possible_seeds;
                info!(?self.possible_seeds, "setting new possible session seeds");
                self.sent_time = command
                    .parse_header::<PacketHead>()
                    .ok()
                    .map(|head| head.sent_ms);
                info!(?self.sent_time, "setting new send time");
            }
        }

        Some(command)
    }

    fn reject_command(&mut self, error: String) {
        self.rejected_commands += 1;
        self.last_error = Some(error);
    }
}

pub fn matches_achievement_packet(game_command: &GameCommand) -> Option<Vec<Achievement>> {
    return matches_achievement_all_data_notify(game_command.proto_data.clone());
}

#[cfg(test)]
mod capture_state_tests {
    use super::*;

    #[test]
    fn malformed_command_lengths_and_missing_keys_are_diagnostic() {
        let mut sniffer = GameSniffer::new();
        assert!(
            sniffer
                .receive_command(vec![0; 2], PacketDirection::Received)
                .is_none()
        );
        assert_eq!(sniffer.rejected_commands(), 1);
        assert_eq!(sniffer.last_error(), Some("incomplete game command"));
        assert!(
            sniffer
                .receive_command(vec![0; 12], PacketDirection::Received)
                .is_none()
        );
        assert!(sniffer.last_error().unwrap().contains("version=17767"));
        let mut bytes = dispatch_command(&[8, 42]);
        bytes[6..10].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(GameCommand::try_new(bytes, PacketDirection::Received).is_none());
    }
    use protobuf::{CodedOutputStream, Message};
    use rsa::{Pkcs1v15Encrypt, RsaPublicKey, rand_core::OsRng};

    fn dispatch_command(body: &[u8]) -> Vec<u8> {
        command_frame(&[], body)
    }

    fn command_frame(header: &[u8], body: &[u8]) -> Vec<u8> {
        let mut bytes = 0x4567u16.to_be_bytes().to_vec();
        bytes.extend_from_slice(&65500u16.to_be_bytes()); // unrelated, rotating command ID
        bytes.extend_from_slice(&(header.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(&0x89ABu16.to_be_bytes());
        bytes
    }

    #[test]
    fn command_header_and_body_keep_overlapping_fields_separate() {
        let mut header = PacketHead::new();
        header.user_id = 800000001;
        header.sent_ms = 1700000000000;
        let mut body = PacketHead::new();
        body.user_id = 900000002;
        body.sent_ms = 17;
        let header_bytes = header.write_to_bytes().unwrap();
        let body_bytes = body.write_to_bytes().unwrap();
        for direction in [PacketDirection::Sent, PacketDirection::Received] {
            let command =
                GameCommand::try_new(command_frame(&header_bytes, &body_bytes), direction).unwrap();
            assert_eq!(command.direction, direction);
            assert!(!command.is_token_response);
            assert_eq!(command.account_uid, None);
            assert_eq!(command.header_data, header_bytes);
            assert_eq!(command.proto_data, body_bytes);
            assert_eq!(
                command.to_bytes(),
                command_frame(&header_bytes, &body_bytes)
            );
            let decoded_header = command.parse_header::<PacketHead>().unwrap();
            assert_eq!(decoded_header.user_id, 800000001);
            assert_eq!(decoded_header.sent_ms, 1700000000000);
            let decoded_body = command.parse_proto::<PacketHead>().unwrap();
            assert_eq!(decoded_body.user_id, 900000002);
            assert_eq!(decoded_body.sent_ms, 17);
        }
    }

    #[test]
    fn framed_commands_reject_inconsistent_lengths_and_allow_empty_parts() {
        for (header, body) in [
            (&[][..], &[][..]),
            (&[8, 1][..], &[][..]),
            (&[][..], &[8, 2][..]),
        ] {
            let command =
                GameCommand::try_new(command_frame(header, body), PacketDirection::Received)
                    .unwrap();
            assert_eq!(command.header_data, header);
            assert_eq!(command.proto_data, body);
        }
        let valid = command_frame(&[8, 1], &[16, 2]);
        for (offset, replacement) in [
            (4, &u16::MAX.to_be_bytes()[..]),
            (6, &u32::MAX.to_be_bytes()[..]),
            (4, &1u16.to_be_bytes()[..]),
            (6, &1u32.to_be_bytes()[..]),
        ] {
            let mut malformed = valid.clone();
            malformed[offset..offset + replacement.len()].copy_from_slice(replacement);
            assert!(GameCommand::try_new(malformed, PacketDirection::Received).is_none());
        }
        let mut truncated = valid.clone();
        truncated.remove(10);
        assert!(GameCommand::try_new(truncated, PacketDirection::Received).is_none());
        let mut trailing = valid;
        trailing.splice(trailing.len() - 2..trailing.len() - 2, [0]);
        assert!(GameCommand::try_new(trailing, PacketDirection::Received).is_none());
    }

    fn sniffer_with_token() -> GameSniffer {
        let mut sniffer = GameSniffer::new().set_initial_keys(HashMap::from([(0, vec![0; 4096])]));
        assert!(!sniffer.key_exchange_captured());
        let ordinary = sniffer
            .receive_command(dispatch_command(&[8, 42]), PacketDirection::Received)
            .unwrap();
        assert!(!ordinary.is_token_response);
        assert!(
            !sniffer.key_exchange_captured(),
            "ordinary dispatch data is not a token response"
        );

        let token = sniffer
            .receive_command(token_response_command(&sniffer), PacketDirection::Received)
            .unwrap();
        assert!(token.is_token_response);
        assert_eq!(
            token.account_uid, None,
            "unknown token layouts stay unidentified"
        );
        sniffer
    }

    fn token_response_command(sniffer: &GameSniffer) -> Vec<u8> {
        // Encrypt a synthetic seed with the same public key as real token responses.
        let public = RsaPublicKey::from(&sniffer.rsa_keys[0]);
        let encrypted = public
            .encrypt(&mut OsRng, Pkcs1v15Encrypt, &123456789u64.to_be_bytes())
            .unwrap();
        let mut body = Vec::new();
        let mut out = CodedOutputStream::vec(&mut body);
        // An overlapping body field must not replace the PacketHead timestamp.
        out.write_uint64(6, 17).unwrap();
        out.write_bytes(883, BASE64_STANDARD.encode(encrypted).as_bytes())
            .unwrap();
        out.flush().unwrap();
        drop(out);
        let mut header = PacketHead::new();
        header.sent_ms = 1700000000000;
        command_frame(&header.write_to_bytes().unwrap(), &body)
    }

    #[test]
    fn token_capture_is_observable_before_session_decryption_or_inventory() {
        let sniffer = sniffer_with_token();
        assert!(sniffer.key_exchange_captured());
        assert_eq!(sniffer.sent_time, Some(1700000000000));
        assert!(
            matches!(sniffer.key, Some(Key::Dispatch(_))),
            "session key discovery has not happened yet"
        );
    }

    #[test]
    fn outgoing_rsa_seed_does_not_mark_a_response_or_change_key_exchange_state() {
        let mut sniffer = GameSniffer::new().set_initial_keys(HashMap::from([(0, vec![0; 4096])]));
        let bytes = token_response_command(&sniffer);
        let sent = sniffer
            .receive_command(bytes.clone(), PacketDirection::Sent)
            .unwrap();
        assert_eq!(sent.direction, PacketDirection::Sent);
        assert!(!sent.is_token_response);
        assert_eq!(sent.account_uid, None);
        assert!(!sniffer.key_exchange_captured());
        assert!(sniffer.sent_time.is_none());

        let incoming = sniffer
            .receive_command(bytes, PacketDirection::Received)
            .unwrap();
        assert!(incoming.is_token_response);
        assert!(sniffer.key_exchange_captured());
        assert_eq!(sniffer.sent_time, Some(1700000000000));
        let ordinary = sniffer
            .receive_command(dispatch_command(&[8, 42]), PacketDirection::Received)
            .unwrap();
        assert!(
            !ordinary.is_token_response,
            "a previous token cannot identify another command"
        );
        assert!(sniffer.key_exchange_captured());
    }

    fn udp_frame(payload: &[u8], incoming: bool) -> Vec<u8> {
        let (src, dst) = if incoming {
            (22101, 50000)
        } else {
            (50000, 22101)
        };
        let builder = etherparse::PacketBuilder::ethernet2([0; 6], [0; 6])
            .ipv4([127, 0, 0, 1], [127, 0, 0, 1], 64)
            .udp(src, dst);
        let mut packet = Vec::new();
        builder.write(&mut packet, payload).unwrap();
        packet
    }

    fn encrypted_frame(
        conv: u32,
        sequence: u32,
        body: &[u8],
        key: &[u8],
        incoming: bool,
    ) -> Vec<u8> {
        let mut command = dispatch_command(body);
        decrypt_command(key, &mut command);
        let mut segment = conv.to_le_bytes().to_vec();
        segment.extend_from_slice(&[0; 4]); // Game-specific KCP token.
        segment.extend_from_slice(&[81, 0, 0, 4]); // PUSH, no fragments, window 1024.
        segment.extend_from_slice(&0u32.to_le_bytes()); // Timestamp.
        segment.extend_from_slice(&sequence.to_le_bytes());
        segment.extend_from_slice(&0u32.to_le_bytes()); // Unacknowledged sequence.
        segment.extend_from_slice(&(command.len() as u32).to_le_bytes());
        segment.extend_from_slice(&[0; 4]); // Game-specific KCP header extension.
        segment.extend_from_slice(&command);
        udp_frame(&segment, incoming)
    }

    fn control_frame(code: u32, conv_id: Option<u32>, incoming: bool) -> Vec<u8> {
        let mut payload = code.to_be_bytes().to_vec();
        if let Some(conv_id) = conv_id {
            payload.extend_from_slice(&conv_id.to_be_bytes());
            payload.extend_from_slice(&[0; 12]);
        }
        udp_frame(&payload, incoming)
    }

    #[test]
    fn both_directions_share_one_conversation_even_during_late_start_capture() {
        let key = vec![0xA5; 4096];
        let mut sniffer =
            GameSniffer::new().set_initial_keys(HashMap::from([(0xA5A5, key.clone())]));
        assert!(matches!(
            sniffer.receive_packet(encrypted_frame(11, 0, &[8, 1], &key, true)),
            Some(GamePacket::Commands(_))
        ));
        assert!(
            sniffer
                .receive_packet(encrypted_frame(22, 0, &[8, 2], &key, false))
                .is_none()
        );
        assert!(sniffer.sent_kcp.is_none());
        let Some(GamePacket::Commands(commands)) =
            sniffer.receive_packet(encrypted_frame(11, 0, &[8, 3], &key, false))
        else {
            panic!("outgoing direction must retain the selected login");
        };
        assert_eq!(commands[0].proto_data, [8, 3]);
    }

    #[test]
    fn new_login_rejects_retired_data_and_controls_without_losing_current_stream() {
        let key = vec![0xA5; 4096];
        let mut sniffer =
            GameSniffer::new().set_initial_keys(HashMap::from([(0xA5A5, key.clone())]));
        assert!(matches!(
            sniffer.receive_packet(encrypted_frame(11, 0, &[8, 1], &key, true)),
            Some(GamePacket::Commands(_))
        ));
        assert!(matches!(
            sniffer.receive_packet(control_frame(0xFF, None, false)),
            Some(GamePacket::Connection(ConnectionPacket::HandshakeRequested))
        ));
        // Delayed packets may arrive before either direction of the new login.
        for incoming in [true, false] {
            assert!(
                sniffer
                    .receive_packet(encrypted_frame(11, 1, &[8, 2], &key, incoming))
                    .is_none()
            );
        }
        assert!(sniffer.recv_kcp.is_none());
        assert!(sniffer.sent_kcp.is_none());
        assert!(matches!(
            sniffer.receive_packet(control_frame(0x145, Some(22), true)),
            Some(GamePacket::Connection(
                ConnectionPacket::HandshakeEstablished { conv_id: Some(22) }
            ))
        ));
        for incoming in [true, false] {
            let Some(GamePacket::Commands(commands)) =
                sniffer.receive_packet(encrypted_frame(22, 0, &[8, 3], &key, incoming))
            else {
                panic!("both new-login directions should decode");
            };
            assert_eq!(commands[0].proto_data, [8, 3]);
            assert!(
                sniffer
                    .receive_packet(encrypted_frame(11, 2, &[8, 4], &key, incoming))
                    .is_none()
            );
        }
        for code in [404, 0x145] {
            assert!(
                sniffer
                    .receive_packet(control_frame(code, Some(11), true))
                    .is_none()
            );
        }
        assert!(
            sniffer
                .receive_packet(control_frame(0x146, Some(22), true))
                .is_none()
        );
        assert_eq!(sniffer.active_conv, Some(22));
        for incoming in [true, false] {
            let Some(GamePacket::Commands(commands)) =
                sniffer.receive_packet(encrypted_frame(22, 1, &[8, 5], &key, incoming))
            else {
                panic!("retired controls must not clear current transport state");
            };
            assert_eq!(commands[0].proto_data, [8, 5]);
        }
    }

    #[test]
    fn retired_conversation_cannot_be_reopened_by_delayed_establishment() {
        let key = vec![0xA5; 4096];
        let mut sniffer =
            GameSniffer::new().set_initial_keys(HashMap::from([(0xA5A5, key.clone())]));
        assert!(matches!(
            sniffer.receive_packet(encrypted_frame(11, 0, &[8, 1], &key, true)),
            Some(GamePacket::Commands(_))
        ));
        assert!(matches!(
            sniffer.receive_packet(control_frame(404, Some(11), true)),
            Some(GamePacket::Connection(ConnectionPacket::Disconnected {
                conv_id: Some(11)
            }))
        ));
        assert!(
            sniffer
                .receive_packet(control_frame(0x145, Some(11), true))
                .is_none()
        );
        assert!(matches!(
            sniffer.receive_packet(control_frame(0xFF, None, false)),
            Some(GamePacket::Connection(ConnectionPacket::HandshakeRequested))
        ));
        assert!(
            sniffer
                .receive_packet(encrypted_frame(11, 0, &[8, 2], &key, true))
                .is_none()
        );
        // An outgoing or unknown control packet cannot authorize reuse.
        assert!(
            sniffer
                .receive_packet(control_frame(0x145, Some(11), false))
                .is_none()
        );
        assert!(
            sniffer
                .receive_packet(control_frame(0x146, Some(11), true))
                .is_none()
        );
        assert!(
            sniffer
                .receive_packet(encrypted_frame(11, 0, &[8, 3], &key, true))
                .is_none()
        );
        assert!(
            sniffer
                .receive_packet(control_frame(0x145, Some(11), true))
                .is_none()
        );
        assert!(
            sniffer
                .receive_packet(encrypted_frame(11, 0, &[8, 4], &key, true))
                .is_none()
        );
        // Without a correlated request nonce, even an explicit old ACK cannot
        // authorize conversation reuse. A fresh conversation remains usable.
        assert!(matches!(
            sniffer.receive_packet(control_frame(0x145, Some(22), true)),
            Some(GamePacket::Connection(
                ConnectionPacket::HandshakeEstablished { conv_id: Some(22) }
            ))
        ));
        let Some(GamePacket::Commands(commands)) =
            sniffer.receive_packet(encrypted_frame(22, 0, &[8, 5], &key, true))
        else {
            panic!("a fresh conversation should survive a delayed old ACK");
        };
        assert_eq!(commands[0].proto_data, [8, 5]);
    }

    #[test]
    fn disconnect_drops_old_encrypted_stream_until_a_new_handshake_request() {
        let key = vec![0xA5; 4096];
        let mut sniffer =
            GameSniffer::new().set_initial_keys(HashMap::from([(0xA5A5, key.clone())]));
        // Initial late-start capture remains supported without a control frame.
        for incoming in [true, false] {
            let Some(GamePacket::Commands(commands)) =
                sniffer.receive_packet(encrypted_frame(11, 0, &[8, 42], &key, incoming))
            else {
                panic!("initial synthetic stream should decode");
            };
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].proto_data, [8, 42]);
            assert_eq!(commands[0].header_data, []);
            assert_eq!(
                commands[0].direction,
                if incoming {
                    PacketDirection::Received
                } else {
                    PacketDirection::Sent
                }
            );
        }
        sniffer.possible_seeds = vec![123456789];
        sniffer.sent_time = Some(1700000000000);
        sniffer.client_seed = Some(987654321);
        assert!(matches!(
            sniffer.receive_packet(udp_frame(&404u32.to_be_bytes(), true)),
            Some(GamePacket::Connection(ConnectionPacket::Disconnected {
                conv_id: None
            }))
        ));
        assert!(sniffer.recv_kcp.is_none());
        assert!(sniffer.sent_kcp.is_none());
        assert!(sniffer.key.is_none());
        assert!(sniffer.client_seed.is_none());
        assert!(sniffer.sent_time.is_none());
        assert!(!sniffer.key_exchange_captured());

        for incoming in [true, false] {
            assert!(
                sniffer
                    .receive_packet(encrypted_frame(11, 1, &[8, 43], &key, incoming),)
                    .is_none()
            );
        }
        assert!(sniffer.recv_kcp.is_none());
        assert!(sniffer.sent_kcp.is_none());
        assert!(sniffer.key.is_none());
        // An establishment without a fresh request cannot reopen a closed stream.
        assert!(matches!(
            sniffer.receive_packet(udp_frame(&0x145u32.to_be_bytes(), true)),
            Some(GamePacket::Connection(
                ConnectionPacket::HandshakeEstablished { conv_id: None }
            ))
        ));
        assert!(
            sniffer
                .receive_packet(encrypted_frame(11, 0, &[8, 43], &key, true),)
                .is_none()
        );

        assert!(matches!(
            sniffer.receive_packet(udp_frame(&0xFFu32.to_be_bytes(), false)),
            Some(GamePacket::Connection(ConnectionPacket::HandshakeRequested))
        ));
        assert!(sniffer.last_error().is_none());
        let Some(GamePacket::Commands(commands)) =
            sniffer.receive_packet(encrypted_frame(22, 0, &[8, 44], &key, true))
        else {
            panic!("new login stream should decode after handshake");
        };
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].proto_data, [8, 44]);
    }

    #[test]
    fn new_handshake_clears_old_token_candidates() {
        let mut sniffer = sniffer_with_token();
        sniffer.client_seed = Some(987654321);
        let builder = etherparse::PacketBuilder::ethernet2([0; 6], [0; 6])
            .ipv4([127, 0, 0, 1], [127, 0, 0, 1], 64)
            .udp(50000, 22101);
        let mut packet = Vec::new();
        builder.write(&mut packet, &0xFFu32.to_be_bytes()).unwrap();
        assert!(matches!(
            sniffer.receive_packet(packet),
            Some(GamePacket::Connection(ConnectionPacket::HandshakeRequested))
        ));
        assert!(!sniffer.key_exchange_captured());
        assert!(sniffer.sent_time.is_none());
        assert!(sniffer.client_seed.is_none());
    }
}

pub fn matches_item_packet(game_command: &GameCommand) -> Option<Vec<r#gen::protos::Item>> {
    if !game_command.is_player_store_notify() {
        return None;
    }

    return matches_items_all_data_notify(&game_command.proto_data);
}

pub fn matches_avatar_packet(game_command: &GameCommand) -> Option<Vec<r#gen::protos::AvatarInfo>> {
    if !game_command.is_avatar_data_notify() {
        return None;
    }

    return matches_avatars_all_data_notify(&game_command.proto_data);
}
