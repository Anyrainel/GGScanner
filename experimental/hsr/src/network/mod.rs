// Adapted from IceDynamix/reliquary d5cf3b7 (MIT); see THIRD_PARTY_NOTICES.md.
use std::collections::{HashMap, VecDeque};

use thiserror::Error;
use tracing::{info, info_span, instrument, warn};

use crate::network::command::GameCommand;

use crate::network::connection::parse_connection_packet;
use crate::network::crypto::{
    decrypt_command, get_game_version, lookup_initial_key, new_key_from_seed,
};
use crate::network::kcp::KcpSniffer;

pub use crate::network::command::GameCommandError;
pub use crate::network::connection::ConnectionPacketError;
pub use crate::network::kcp::KcpError;

pub mod command;

mod connection;
mod crypto;
mod identity;
mod kcp;

pub(crate) use identity::{AccountIdentity, IdentityUpdate};

const PORTS: [u16; 2] = [23301, 23302];

#[derive(Error, Debug)]
pub enum NetworkError {
    #[error(transparent)]
    ConnectionPacket(#[from] ConnectionPacketError),
    #[error(transparent)]
    Kcp(#[from] KcpError),
    #[error(transparent)]
    GameCommand(#[from] GameCommandError),
}

/// Top-level packet sent by the game
pub enum GamePacket {
    Connection(ConnectionPacket),
    Commands {
        conv_id: u32,
        direction: PacketDirection,
        result: Result<DecodedCommand, GameCommandError>,
    },
}

pub(crate) struct DecodedCommand {
    pub command: GameCommand,
    pub identity_update: Option<IdentityUpdate>,
    /// Retained dispatch revision identifies the reviewed destructive sync schema.
    pub dispatch_key_id: Option<u32>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn frame(conv: u32, sequence: u32, body: &[u8], key: &[u8]) -> Vec<u8> {
        command_frame(conv, sequence, 65500, body, key, PacketDirection::Received)
    }

    pub(crate) fn command_frame(
        conv: u32,
        sequence: u32,
        id: u16,
        body: &[u8],
        key: &[u8],
        direction: PacketDirection,
    ) -> Vec<u8> {
        let mut command = 0x9D74C714u32.to_be_bytes().to_vec();
        command.extend_from_slice(&id.to_be_bytes());
        command.extend_from_slice(&0u16.to_be_bytes());
        command.extend_from_slice(&(body.len() as u32).to_be_bytes());
        command.extend_from_slice(body);
        command.extend_from_slice(&[0; 4]);
        decrypt_command(key, &mut command);
        let mut payload = conv.to_le_bytes().to_vec();
        payload.extend_from_slice(&[0; 4]); // HSR KCP token
        payload.extend_from_slice(&[81, 0, 0, 1]);
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&sequence.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&(command.len() as u32).to_le_bytes());
        payload.extend(command);
        let mut packet = vec![0; 14];
        packet[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let length = 20 + 8 + payload.len();
        let mut ip = vec![0; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&(length as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        packet.extend(ip);
        let ports: [u16; 2] = if direction == PacketDirection::Received {
            [23301, 50000]
        } else {
            [50000, 23301]
        };
        packet.extend_from_slice(&ports[0].to_be_bytes());
        packet.extend_from_slice(&ports[1].to_be_bytes());
        packet.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        packet.extend_from_slice(&[0, 0]);
        packet.extend(payload);
        packet
    }

    pub(crate) fn session_key(seed: u64) -> Vec<u8> {
        new_key_from_seed(seed)
    }

    pub(crate) fn token(seed: u64, tag: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut out = protobuf::CodedOutputStream::vec(&mut bytes);
        out.write_uint64(tag, seed).unwrap();
        out.write_uint32(73, 100).unwrap();
        out.flush().unwrap();
        drop(out);
        bytes
    }

    fn decoded(sniffer: &mut GameSniffer, bytes: Vec<u8>) -> Vec<Vec<u8>> {
        sniffer
            .receive_packet(bytes)
            .unwrap()
            .into_iter()
            .filter_map(|packet| match packet {
                GamePacket::Commands {
                    result: Ok(command),
                    ..
                } => Some(command.command.proto_data),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn interleaved_conversations_keep_independent_inferred_session_keys() {
        let key = vec![0; 4096];
        let mut sniffer = GameSniffer::new().set_initial_keys(HashMap::from([(0, key.clone())]));
        let first = token(0x1234567890, 993);
        let second = token(0x2345678901, 21);
        assert!(!sniffer.key_exchange_captured(None));
        assert_eq!(
            decoded(&mut sniffer, frame(11, 0, &first, &key)),
            vec![first]
        );
        assert!(sniffer.key_exchange_captured(Some(11)));
        assert!(!sniffer.key_exchange_captured(Some(22)));
        assert_eq!(
            decoded(&mut sniffer, frame(22, 0, &second, &key)),
            vec![second]
        );
        assert!(sniffer.key_exchange_captured(Some(22)));
        let payload = vec![8, 42];
        assert_eq!(
            decoded(
                &mut sniffer,
                frame(11, 1, &payload, &new_key_from_seed(0x1234567890))
            ),
            vec![payload.clone()]
        );
        assert_eq!(
            decoded(
                &mut sniffer,
                frame(22, 1, &payload, &new_key_from_seed(0x2345678901))
            ),
            vec![payload]
        );
    }

    #[test]
    fn reordered_kcp_messages_are_reassembled_and_duplicates_are_ignored() {
        let key = vec![0; 4096];
        let mut sniffer = GameSniffer::new().set_initial_keys(HashMap::from([(0, key.clone())]));
        assert!(decoded(&mut sniffer, frame(33, 1, &[8, 2], &key)).is_empty());
        assert_eq!(
            decoded(&mut sniffer, frame(33, 0, &[8, 1], &key)),
            vec![vec![8, 1], vec![8, 2]]
        );
        assert!(!sniffer.key_exchange_captured(None));
        assert!(decoded(&mut sniffer, frame(33, 1, &[8, 2], &key)).is_empty());
    }

    #[test]
    fn truncated_frames_return_errors_without_panicking() {
        let mut sniffer = GameSniffer::new();
        let packet = frame(11, 0, &[8, 1], &[0; 4096]);
        for end in 0..packet.len() {
            let _ = sniffer.receive_packet(packet[..end].to_vec());
        }
    }
}

/// Packet for connection management
pub enum ConnectionPacket {
    HandshakeRequested,
    Disconnected { conv_id: u32 },
    HandshakeEstablished { conv_id: u32 },
    SegmentData(PacketDirection, Vec<u8>),
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum PacketDirection {
    Sent,
    Received,
}

/// Per-conversation state for KCP and command handling
struct ConversationSniffer {
    session_key: bool,
    conv_id: u32,
    sent_kcp: KcpSniffer,
    recv_kcp: KcpSniffer,
    key: Option<Vec<u8>>,
    dispatch_key_id: Option<u32>,
    account: Option<AccountIdentity>,
    // Commands sent by client that couldn't be decrypted due to a version mismatch.
    // We buffer the raw command bytes and retry after we learn the new session key
    // from PlayerGetTokenScRsp.
    pending_sent: VecDeque<Vec<u8>>,
    // Commands that were successfully retried after setting the new key. These are
    // emitted with the next receive_packet() response.
    deferred_commands: Vec<DecodedCommand>,
}

impl ConversationSniffer {
    fn new(conv_id: u32) -> Self {
        Self {
            session_key: false,
            conv_id,
            sent_kcp: KcpSniffer::new(conv_id),
            recv_kcp: KcpSniffer::new(conv_id),
            key: None,
            dispatch_key_id: None,
            account: None,
            pending_sent: VecDeque::new(),
            deferred_commands: Vec::new(),
        }
    }

    fn receive_kcp_segment(
        &mut self,
        direction: PacketDirection,
        kcp_seg: &[u8],
        initial_keys: &HashMap<u32, Vec<u8>>,
    ) -> Result<Vec<Result<DecodedCommand, GameCommandError>>, KcpError> {
        let kcp = match direction {
            PacketDirection::Sent => &mut self.sent_kcp,
            PacketDirection::Received => &mut self.recv_kcp,
        };

        kcp.receive_segments(kcp_seg).map(|segments| {
            segments
                .into_iter()
                .map(|data| self.receive_command(direction, data, initial_keys))
                .collect()
        })
    }

    #[instrument(skip_all, fields(conv_id = self.conv_id, len = data.len()))]
    fn receive_command(
        &mut self,
        direction: PacketDirection,
        mut data: Vec<u8>,
        initial_keys: &HashMap<u32, Vec<u8>>,
    ) -> Result<DecodedCommand, GameCommandError> {
        // Keep original bytes in case we need to buffer on version mismatch (client-sent only)
        let original_data = if matches!(direction, PacketDirection::Sent) {
            Some(data.clone())
        } else {
            None
        };

        if data.len() < 16 {
            return Err(GameCommandError::HeaderTooShort {
                expected: 16,
                actual: data.len(),
            });
        }
        let key = match self.key.as_ref() {
            Some(key) => key,
            None => match lookup_initial_key(initial_keys, get_game_version(&data)) {
                Some(key) => {
                    self.dispatch_key_id = Some(get_game_version(&data));
                    self.key = Some(key);
                    self.key.as_ref().unwrap()
                },
                None => {
                    return Err(GameCommandError::DecryptionKeyMissing {
                        version: get_game_version(&data),
                    })
                },
            },
        };

        decrypt_command(key, &mut data);

        let decrypted_version = get_game_version(&data);
        if decrypted_version != 0 {
            warn!(
                decrypted = decrypted_version,
                "decrypted version does not match expected version"
            );

            // If the client sent this command, we likely used an outdated key.
            // Buffer the original bytes and retry after we get PlayerGetTokenScRsp.
            if let Some(orig) = original_data {
                if self.pending_sent.len() < 128 {
                    self.pending_sent.push_back(orig);
                }
            }
            return Err(GameCommandError::VersionMismatch);
        }

        let command = GameCommand::try_new(data)?;

        let span = info_span!("command", ?command);
        let _enter = span.enter();

        info!("received");

        let mut identity_update = None;
        if direction == PacketDirection::Received {
            if !self.session_key {
                if let Some(uid) = identity::token_uid(
                    self.dispatch_key_id,
                    command.command_id,
                    &command.proto_data,
                ) {
                    let account = AccountIdentity {
                        uid,
                        nickname: None,
                    };
                    self.account = Some(account.clone());
                    identity_update = Some(IdentityUpdate {
                        account,
                        new_login: true,
                    });
                }
            } else if let Some(account) = &mut self.account {
                if let Some(nickname) = identity::login_nickname(
                    self.dispatch_key_id,
                    command.command_id,
                    &command.proto_data,
                ) {
                    account.nickname = Some(nickname);
                    identity_update = Some(IdentityUpdate {
                        account: account.clone(),
                        new_login: false,
                    });
                }
            }
        }

        if let Some(seed) = self
            .key
            .as_ref()
            .filter(|_| !self.session_key && matches!(direction, PacketDirection::Received))
            .and_then(|_| crate::packet_capture::protocol::infer_session_seed(&command.proto_data))
        {
            self.session_key = true;
            info!("session decryption ready");
            self.key = Some(new_key_from_seed(seed));

            // Now that we have the new session key, retry any buffered client-sent
            // commands that previously failed with VersionMismatch.
            while let Some(bytes) = self.pending_sent.pop_front() {
                match self.receive_command(PacketDirection::Sent, bytes.clone(), initial_keys) {
                    Ok(cmd) => {
                        // Defer emission to receive_packet so we can include
                        // these in the same output batch.
                        self.deferred_commands.push(cmd);
                    },
                    Err(GameCommandError::VersionMismatch) => {
                        // Still mismatched; push back and stop to avoid a busy loop.
                        // We'll try again when another key update occurs.
                        // (Shouldn't normally happen.)
                        // Re-queue at the front to preserve order for next retry.
                        self.pending_sent.push_front(bytes);
                        break;
                    },
                    Err(e) => {
                        warn!(?e, "failed to retry buffered sent command");
                    },
                }
            }
        }

        Ok(DecodedCommand {
            command,
            identity_update,
            dispatch_key_id: self.dispatch_key_id,
        })
    }

    fn take_deferred_commands(&mut self) -> Vec<DecodedCommand> {
        std::mem::take(&mut self.deferred_commands)
    }
}

#[derive(Default)]
pub struct GameSniffer {
    conversations: HashMap<u32, ConversationSniffer>,
    initial_keys: HashMap<u32, Vec<u8>>,
}

impl GameSniffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_initial_keys(mut self, initial_keys: HashMap<u32, Vec<u8>>) -> Self {
        self.initial_keys = initial_keys;
        self
    }

    /// A token response supplied a session seed for the selected conversation.
    /// Before inventory selects a conversation, report any captured exchange.
    pub fn key_exchange_captured(&self, selected: Option<u32>) -> bool {
        match selected {
            Some(id) => self.conversations.get(&id).is_some_and(|c| c.session_key),
            None => self.conversations.values().any(|c| c.session_key),
        }
    }

    #[instrument(skip_all, fields(len = bytes.len()))]
    pub fn receive_packet(&mut self, bytes: Vec<u8>) -> Result<Vec<GamePacket>, NetworkError> {
        let packet = parse_connection_packet(&PORTS, bytes)?;

        match packet {
            ConnectionPacket::HandshakeRequested => {
                info!("handshake requested");
                Ok(vec![GamePacket::Connection(packet)])
            },

            ConnectionPacket::HandshakeEstablished { conv_id } => {
                info!(conv_id, "handshake established, creating new conversation");
                // pktmon can deliver the same handshake from several capture points.
                // A duplicate must not discard a key/identity already learned.
                self.conversations
                    .entry(conv_id)
                    .or_insert_with(|| ConversationSniffer::new(conv_id));
                Ok(vec![GamePacket::Connection(packet)])
            },

            ConnectionPacket::Disconnected { conv_id } => {
                self.conversations.remove(&conv_id);
                Ok(vec![GamePacket::Connection(packet)])
            },

            ConnectionPacket::SegmentData(direction, kcp_seg) => {
                let conv_id = kcp::validate_kcp_segment(&kcp_seg)?;

                // Get or create conversation
                let conversation = self
                    .conversations
                    .entry(conv_id)
                    .or_insert_with(|| ConversationSniffer::new(conv_id));

                let mut commands: Vec<GamePacket> = conversation
                    .receive_kcp_segment(direction, &kcp_seg, &self.initial_keys)?
                    .into_iter()
                    .map(|result| GamePacket::Commands {
                        conv_id,
                        direction,
                        result,
                    })
                    .collect();

                // Emit any deferred commands from key update
                for cmd in conversation.take_deferred_commands() {
                    commands.push(GamePacket::Commands {
                        conv_id,
                        direction: PacketDirection::Sent,
                        result: Ok(cmd),
                    });
                }

                Ok(commands)
            },
        }
    }
}
