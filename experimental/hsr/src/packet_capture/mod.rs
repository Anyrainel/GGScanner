//! In-process, read-only capture of Honkai: Star Rail inventory and achievements.
//!
//! The native boundary captures only HSR UDP traffic on the two established
//! game ports. [`HsrPacketDecoder`] can replay the same packet bytes
//! without a live device, while [`protocol`] is a pure decoder for sanitized
//! decrypted-command fixtures.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Mutex},
};

use crate::network::{AccountIdentity, GamePacket, GameSniffer, IdentityUpdate, PacketDirection};
use crate::{
    model::{
        CoverageLevel, EvidenceKind, InventoryCoverage, ObservationEvidence, ObservationSnapshot,
        OBSERVATION_SCHEMA_VERSION,
    },
    pipeline::write_json_create_new,
    reference::ReferenceCache,
};
use base64::prelude::*;
#[cfg(target_os = "windows")]
use futures::{stream::FusedStream, StreamExt};
use inventory::InventoryDecoder;
#[cfg(target_os = "windows")]
use pktmon::{
    filter::{PktMonFilter, TransportProtocol},
    Capture, Packet,
};
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinHandle},
};
use tokio_util::sync::CancellationToken;

use crate::{localization::LocalizedText, HsrError, HsrResult};

mod dump;
mod inventory;
mod inventory_sync;
pub mod proto;
pub mod protocol;

/// Decoder revision written into HSR exports.
pub const HSR_CAPTURE_REVISION: &str = "reliquary-sync-4.7-v1";

/// HSR's two UDP gateway ports. Kept public for build-time regression tests.
pub const HSR_PACKET_PORTS: [u16; 2] = [23301, 23302];
const CAPTURE_TIMEOUT_SECS: u64 = 300;

const CAPTURE_HINT: LocalizedText = LocalizedText::new(
    "无法读取 HSR 网络数据。请确认程序以管理员身份运行，然后重新抓包。",
    "HSR network data could not be read. Make sure the application is running as administrator, then capture again.",
);
const KEYS_HINT: LocalizedText = LocalizedText::new(
    "内置的 HSR 协议数据无法加载。请重新下载当前版本的 GGScanner。",
    "The bundled HSR protocol data could not be loaded. Download the current GGScanner build again.",
);
const WORKER_HINT: LocalizedText = LocalizedText::new(
    "HSR 抓包任务意外停止。请重新抓包；若问题再次出现，请复制完整错误。",
    "The HSR capture task stopped unexpectedly. Capture again; if it recurs, copy the full error.",
);
const UID_HINT: LocalizedText = LocalizedText::new(
    "无法验证星铁账号 UID。请更新 GGScanner，并在开始抓包后重新登录；若仍失败，请复制完整错误。",
    "The Star Rail account UID could not be verified. Update GGScanner and log in after starting capture; if this persists, copy the full error.",
);
#[cfg(not(target_os = "windows"))]
const PLATFORM_HINT: LocalizedText = LocalizedText::new(
    "抓包功能目前仅支持 Windows（pktmon）。Linux 后端尚未实现。",
    "Packet capture is currently Windows-only (pktmon). A Linux backend is not implemented yet.",
);

/// Categories selected before starting a capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureTargets {
    pub characters: bool,
    pub light_cones: bool,
    pub relics: bool,
    pub achievements: bool,
}

impl Default for CaptureTargets {
    fn default() -> Self {
        Self {
            characters: true,
            light_cones: true,
            relics: true,
            achievements: true,
        }
    }
}

impl CaptureTargets {
    pub fn any(self) -> bool {
        self.characters || self.light_cones || self.relics || self.achievements
    }
}

/// Stateful Ethernet/IP/UDP/KCP/session-key decoder used by both native
/// capture and deterministic packet replay.
pub struct HsrPacketDecoder {
    dispatch_keys: HashMap<u32, Vec<u8>>,
    sniffer: GameSniffer,
    inventory: InventoryDecoder,
    packet_dump: Option<dump::PacketDump>,
    targets: CaptureTargets,
    state: HsrCaptureState,
    conversation: Option<u32>,
    account: Option<AccountIdentity>,
    retired_conversations: HashSet<u32>,
}

impl HsrPacketDecoder {
    pub fn new(references: ReferenceCache, targets: CaptureTargets) -> HsrResult<Self> {
        if !targets.any() {
            return Err(HsrError::new(
                "HSR-CAPTURE-TARGETS",
                LocalizedText::new(
                    "请至少选择一项导出内容。",
                    "Select at least one category to export.",
                ),
                "capture requires at least one selected category",
            ));
        }
        let dispatch_keys = load_embedded_dispatch_keys()?;
        let sniffer = GameSniffer::new().set_initial_keys(dispatch_keys.clone());
        Ok(Self {
            dispatch_keys,
            sniffer,
            inventory: InventoryDecoder::new(references, targets)?,
            packet_dump: None,
            targets,
            state: HsrCaptureState::default(),
            conversation: None,
            account: None,
            retired_conversations: HashSet::new(),
        })
    }

    pub fn enable_continuous(&mut self) {
        self.inventory.continuous = true;
    }

    pub fn state(&self) -> &HsrCaptureState {
        &self.state
    }

    /// Save exact raw frames and decrypted messages during offline replay too.
    pub fn set_packet_dump(&mut self, root: std::path::PathBuf) {
        self.packet_dump = Some(dump::PacketDump::new(root));
    }

    /// Full packet replay and live capture use exactly the same transport.
    pub fn receive_packet(&mut self, packet: Vec<u8>) -> HsrResult<()> {
        // One-shot monitors stop at the first complete capture. Preserve that
        // historical result when offline replay includes later disconnects.
        if self.state.complete && !self.inventory.continuous {
            return Ok(());
        }
        if let Some(dump) = &mut self.packet_dump {
            dump.write_raw(&packet)?;
        }
        self.state.packet_count += 1;
        let packets = match self.sniffer.receive_packet(packet) {
            Ok(packets) => packets,
            Err(error) => {
                self.state.last_transport_error = Some(error.to_string());
                return Ok(());
            },
        };
        for packet in packets {
            match packet {
                GamePacket::Commands {
                    conv_id,
                    direction,
                    result: Ok(decoded),
                } => {
                    if direction != PacketDirection::Received
                        || self.retired_conversations.contains(&conv_id)
                    {
                        continue;
                    }
                    if let Some(update) = decoded.identity_update {
                        self.receive_identity(conv_id, update);
                    } else if decoded.login_started {
                        // The transport recognized a new login even if the UID
                        // layout is unsupported or its handshake was missed.
                        // Never leave the previous account's snapshot live.
                        self.begin_login_session(conv_id);
                    }
                    if self
                        .conversation
                        .is_some_and(|selected| selected != conv_id)
                    {
                        continue;
                    }
                    self.state.command_count += 1;
                    let deleted = inventory_sync::deletions(
                        decoded.dispatch_key_id,
                        decoded.command.command_id,
                        &decoded.command.proto_data,
                    )?;
                    self.receive_inventory_command(&decoded.command.proto_data, deleted)?;
                    if self.state.complete && self.account.is_none() {
                        self.state.complete = false;
                        return Err(HsrError::new(
                            "HSR-CAPTURE-UID",
                            UID_HINT,
                            format!(
                                "selected data decoded without a verified login UID; dispatchKey={:?}; conversation={conv_id}",
                                decoded.dispatch_key_id
                            ),
                        ));
                    }
                    if self.state.has_characters
                        || self.state.has_light_cones
                        || self.state.has_relics
                        || self.state.has_achievements
                    {
                        self.conversation = Some(conv_id);
                    }
                },
                GamePacket::Commands {
                    result: Err(error), ..
                } => {
                    if let crate::network::GameCommandError::DecryptionKeyMissing { version } =
                        &error
                    {
                        self.state.missing_dispatch_key = Some(*version);
                    }
                    self.state.last_transport_error = Some(error.to_string());
                },
                GamePacket::Connection(connection) => match connection {
                    crate::network::ConnectionPacket::Disconnected { conv_id }
                        if self.conversation == Some(conv_id) =>
                    {
                        self.retired_conversations.insert(conv_id);
                        self.clear_inventory_session();
                        self.state.last_transport_error = Some("game connection closed".to_owned());
                    },
                    crate::network::ConnectionPacket::HandshakeEstablished { conv_id }
                        if !self.retired_conversations.contains(&conv_id)
                            && self.conversation != Some(conv_id) =>
                    {
                        if let Some(previous) = self.conversation {
                            self.retired_conversations.insert(previous);
                        }
                        self.clear_inventory_session();
                        self.conversation = Some(conv_id);
                    },
                    // A request has no scoped conversation ID. Wait for the
                    // establishment/token instead of invalidating another account
                    // because of a delayed request or foreign disconnect.
                    _ => {},
                },
            }
        }
        let captured = self.sniffer.key_exchange_captured(self.conversation);
        if captured && !self.state.has_key_exchange {
            yas::log_info!("已捕获登录握手", "Login handshake captured");
        }
        self.state.has_key_exchange = captured;
        Ok(())
    }

    /// Diagnostic decrypted-command replay; it cannot establish account identity.
    /// Public capture exports still require a verified incoming login UID.
    pub fn receive_command(&mut self, bytes: &[u8]) -> HsrResult<()> {
        self.receive_inventory_command(bytes, None)
    }

    fn receive_inventory_command(
        &mut self,
        bytes: &[u8],
        deleted: Option<crate::scanner_export::CaptureInstanceIds>,
    ) -> HsrResult<()> {
        if let Some(dump) = &mut self.packet_dump {
            dump.write(bytes)?;
        }
        self.inventory.receive(bytes)?;
        if let Some(deleted) = deleted {
            self.inventory.remove_instances(deleted)?;
        }
        if self.targets.achievements && !self.state.has_achievements {
            for container in protocol::containers(bytes) {
                if let Some(snapshot) = protocol::decode_quest_command(container)? {
                    self.state.achievement_count = snapshot.len();
                    self.state.quests = snapshot;
                    self.state.has_achievements = true;
                    break;
                }
            }
        }
        self.state.character_count = self.inventory.characters.as_ref().map_or(0, Vec::len);
        self.state.light_cone_count = self.inventory.light_cones.as_ref().map_or(0, Vec::len);
        self.state.relic_count = self.inventory.relics.as_ref().map_or(0, Vec::len);
        self.state.has_characters = self.inventory.characters.is_some();
        self.state.has_light_cones = self.inventory.light_cones.is_some();
        self.state.has_relics = self.inventory.relics.is_some();
        self.state.has_items =
            self.inventory.light_cones.is_some() && self.inventory.relics.is_some();
        self.state.complete = self.inventory.complete()
            && (!self.targets.achievements || self.state.has_achievements);
        if self.state.complete {
            self.state.inventory = self.inventory.snapshot();
            self.state.export_details = self.inventory.export_details.clone();
            if let Some(account) = &self.account {
                self.state.export_details.uid = Some(u64::from(account.uid));
                self.state.export_details.nickname = account.nickname.clone();
            }
        }
        Ok(())
    }

    fn clear_inventory_session(&mut self) {
        let session_generation = self.state.session_generation + 1;
        self.inventory.reset();
        self.conversation = None;
        self.account = None;
        self.state = HsrCaptureState {
            session_generation,
            ..HsrCaptureState::default()
        };
    }

    fn begin_login_session(&mut self, conv_id: u32) {
        if let Some(previous) = self.conversation.filter(|previous| *previous != conv_id) {
            self.retired_conversations.insert(previous);
        }
        self.clear_inventory_session();
        self.conversation = Some(conv_id);
    }

    fn receive_identity(&mut self, conv_id: u32, update: IdentityUpdate) {
        if update.new_login {
            self.begin_login_session(conv_id);
            self.account = Some(update.account);
        } else if self.conversation == Some(conv_id)
            && self
                .account
                .as_ref()
                .is_some_and(|account| account.uid == update.account.uid)
        {
            self.account = Some(update.account);
            // Metadata changes must be observable even without an item mutation.
            self.inventory.revision += 1;
        }
        if let Some(account) = &self.account {
            self.state.export_details.uid = Some(u64::from(account.uid));
            self.state.export_details.nickname = account.nickname.clone();
        }
    }

    fn reset(&mut self) {
        self.sniffer = GameSniffer::new().set_initial_keys(self.dispatch_keys.clone());
        self.clear_inventory_session();
        self.retired_conversations.clear();
    }
}

/// Commands sent by the application to the background capture monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HsrCaptureCommand {
    StartCapture,
    /// Terminal for this monitor instance. A later queued StartCapture is
    /// ignored so an early UI stop cannot race native-device initialization.
    StopCapture,
}

/// Privacy-safe state shared with the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HsrCaptureState {
    /// Changes when login/account/reconnect invalidates the previous baseline.
    pub session_generation: u64,
    pub listener_ready: bool,
    pub elapsed_secs: u64,
    pub capturing: bool,
    /// All selected categories decoded. Full packet capture also requires UID;
    /// diagnostic command-only replay cannot authorize a public export.
    pub complete: bool,
    /// Actual token/session-seed capture, independent of decoded command counts.
    pub has_key_exchange: bool,
    pub packet_count: usize,
    pub command_count: usize,
    pub last_transport_error: Option<String>,
    /// Public dispatch-key identifier, retained when later unrelated frames fail.
    pub missing_dispatch_key: Option<u32>,
    pub has_characters: bool,
    pub has_items: bool,
    pub has_light_cones: bool,
    pub has_relics: bool,
    pub has_achievements: bool,
    pub character_count: usize,
    pub light_cone_count: usize,
    pub relic_count: usize,
    pub inventory: Option<ObservationSnapshot>,
    pub export_details: crate::scanner_export::CaptureExportDetails,
    pub achievement_count: usize,
    pub quests: Vec<crate::model::ObservedQuest>,
    pub error: Option<HsrError>,
}

impl HsrCaptureState {
    pub fn diagnostics(&self) -> String {
        format!("listenerReady={}; elapsed={}s; packets={}; decryptedCommands={}; keyExchange={}; uidVerified={}; characters={}; lightCones={}; relics={}; achievements={}; missingDispatchKey={:?}; lastTransportError={}", self.listener_ready, self.elapsed_secs, self.packet_count, self.command_count, self.has_key_exchange, self.export_details.uid.is_some(), self.character_count, self.light_cone_count, self.relic_count, self.achievement_count, self.missing_dispatch_key, self.last_transport_error.as_deref().unwrap_or("none"))
    }
    /// Single HSR-Scanner v4 JSON, including optional achievement and Trailblazer extensions.
    pub fn build_scanner_export(
        &self,
        references: &ReferenceCache,
    ) -> HsrResult<serde_json::Value> {
        if !self.complete {
            return Err(HsrError::new(
                "HSR-CAPTURE-EXPORT-INCOMPLETE",
                LocalizedText::new(
                    "抓包尚未完成，无法导出。",
                    "Capture is not complete, so nothing can be exported.",
                ),
                "build_scanner_export requires a complete capture",
            ));
        }
        if self.export_details.uid.is_none_or(|uid| uid < 100_000_000) {
            return Err(HsrError::new(
                "HSR-CAPTURE-UID",
                UID_HINT,
                "a public packet capture export requires a verified incoming login UID",
            ));
        }
        let owned;
        let inventory = match self.inventory.as_ref() {
            Some(snapshot) => snapshot,
            None => {
                owned = empty_capture_snapshot();
                &owned
            },
        };
        let mut export = crate::scanner_export::build_scanner_export(
            inventory,
            references,
            &self.export_details,
            None,
        )?;
        if self.has_achievements {
            export.as_object_mut().unwrap().insert(
                "quests".into(),
                serde_json::json!({
                    "schemaVersion": 1, "coverage": "complete", "records": self.quests,
                }),
            );
        }
        Ok(export)
    }

    /// Write one HSR-Scanner v4 JSON. Returns that path.
    pub fn write_interop_exports(
        &self,
        dir: &std::path::Path,
        references: &ReferenceCache,
        suffix: &str,
    ) -> HsrResult<std::path::PathBuf> {
        std::fs::create_dir_all(dir).map_err(|error| {
            HsrError::write_failed(
                "HSR-CAPTURE-EXPORT-DIR",
                format!("dir={}; cause={error}", dir.display()),
            )
        })?;
        let path = dir.join(format!("star_rail_export{suffix}.json"));
        write_json_create_new(&path, &self.build_scanner_export(references)?)?;
        Ok(path)
    }
}

fn empty_capture_snapshot() -> ObservationSnapshot {
    ObservationSnapshot {
        schema_version: OBSERVATION_SCHEMA_VERSION,
        evidence: ObservationEvidence {
            kind: EvidenceKind::PacketCapture,
            revision: HSR_CAPTURE_REVISION.to_owned(),
            coverage: InventoryCoverage {
                characters: CoverageLevel::Unknown,
                light_cones: CoverageLevel::Unknown,
                relics: CoverageLevel::Unknown,
            },
        },
        characters: vec![],
        light_cones: vec![],
        gear: vec![],
    }
}

/// Cancelable capture monitor. Constructing it does not access a native
/// capture device; the device opens only after [`StartCapture`](HsrCaptureCommand::StartCapture).
pub struct HsrCaptureMonitor {
    decoder: HsrPacketDecoder,
    state: Arc<Mutex<HsrCaptureState>>,
    capture_cancel_token: Option<CancellationToken>,
    capture_task: Option<JoinHandle<HsrResult<()>>>,
    terminal_stop_requested: bool,
    started: Option<tokio::time::Instant>,
    continuous: Option<Box<dyn Fn(&HsrCaptureState) + Send>>,
    packet_tx: mpsc::UnboundedSender<Vec<u8>>,
    packet_rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl HsrCaptureMonitor {
    pub fn new(
        state: Arc<Mutex<HsrCaptureState>>,
        references: ReferenceCache,
        targets: CaptureTargets,
    ) -> HsrResult<Self> {
        let decoder = HsrPacketDecoder::new(references, targets)?;
        let (packet_tx, packet_rx) = mpsc::unbounded_channel();
        Ok(Self {
            decoder,
            state,
            capture_cancel_token: None,
            capture_task: None,
            terminal_stop_requested: false,
            started: None,
            continuous: None,
            packet_tx,
            packet_rx,
        })
    }

    /// Enabling diagnostics performs no I/O until a decrypted command arrives.
    pub fn with_packet_dump(mut self, root: std::path::PathBuf) -> Self {
        self.decoder.set_packet_dump(root);
        self
    }

    pub fn with_continuous(mut self, observer: impl Fn(&HsrCaptureState) + Send + 'static) -> Self {
        self.decoder.enable_continuous();
        self.continuous = Some(Box::new(observer));
        self
    }

    pub async fn run(mut self, mut command_rx: mpsc::UnboundedReceiver<HsrCaptureCommand>) {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = timer.tick() => self.check_timeout(),
                Some(packet) = self.packet_rx.recv() => self.handle_packet(packet),
                command = command_rx.recv() => match command {
                    Some(command) => self.handle_command(command),
                    None => break,
                },
                result = wait_for_capture_task(&mut self.capture_task) => {
                    self.handle_capture_task_result(result);
                },
                else => break,
            }
        }

        self.stop_capture();
        if let Some(task) = self.capture_task.take() {
            let _ = task.await;
        }
    }

    fn handle_command(&mut self, command: HsrCaptureCommand) {
        match command {
            HsrCaptureCommand::StartCapture if !self.terminal_stop_requested => {
                self.start_capture();
            },
            HsrCaptureCommand::StartCapture => {},
            HsrCaptureCommand::StopCapture => {
                self.terminal_stop_requested = true;
                self.stop_capture();
            },
        }
    }

    fn start_capture(&mut self) {
        if self.capture_task.is_some() {
            return;
        }

        self.decoder.reset();
        if let Some(dump) = &mut self.decoder.packet_dump {
            if let Err(error) = dump.ensure_session() {
                self.fail(error);
                return;
            }
        }
        self.started = Some(tokio::time::Instant::now());
        if let Ok(mut state) = self.state.lock() {
            *state = HsrCaptureState {
                capturing: true,
                ..HsrCaptureState::default()
            };
        }

        let cancel_token = CancellationToken::new();
        self.capture_task = Some(tokio::spawn(capture_task(
            cancel_token.clone(),
            self.packet_tx.clone(),
            self.state.clone(),
        )));
        self.capture_cancel_token = Some(cancel_token);
    }

    fn stop_capture(&mut self) {
        self.started = None;
        if let Some(token) = &self.capture_cancel_token {
            token.cancel();
        }
        if let Ok(mut state) = self.state.lock() {
            state.capturing = false;
        }
    }

    fn handle_packet(&mut self, packet: Vec<u8>) {
        if self.started.is_none() {
            return;
        }
        let previous_revision = self.decoder.inventory.revision;
        let previous_generation = self.decoder.state.session_generation;
        match catch_unwind(AssertUnwindSafe(|| self.decoder.receive_packet(packet))) {
            Ok(Ok(())) => {
                let mut snapshot = self.decoder.state().clone();
                snapshot.capturing = self.continuous.is_some() || !snapshot.complete;
                if snapshot.complete && self.continuous.is_none() {
                    self.stop_capture();
                }
                if let Ok(mut state) = self.state.lock() {
                    snapshot.listener_ready = state.listener_ready;
                    snapshot.elapsed_secs = state.elapsed_secs;
                    if self.decoder.inventory.revision != previous_revision
                        || snapshot.session_generation != previous_generation
                    {
                        if let Some(observer) = &self.continuous {
                            observer(&snapshot);
                        }
                    }
                    *state = snapshot;
                }
            },
            Ok(Err(error)) => self.fail(error),
            Err(_) => self.fail(HsrError::new(
                "HSR-CAPTURE-KCP",
                WORKER_HINT,
                "kcp decoder panicked while reading a captured packet",
            )),
        }
    }

    fn check_timeout(&mut self) {
        if let Some(start) = self.started {
            let mut state = self.state.lock().expect("capture state poisoned");
            state.elapsed_secs = start.elapsed().as_secs();
            if state.elapsed_secs % 15 == 0 {
                yas::log_info!(
                    "星铁抓包诊断：{}",
                    "HSR capture diagnostics: {}",
                    state.diagnostics()
                );
            }
        }
        if self.started.is_some_and(|start| {
            self.continuous.is_none() && start.elapsed().as_secs() >= CAPTURE_TIMEOUT_SECS
        }) {
            let shared = self.state.lock().expect("capture state poisoned").clone();
            let mut snapshot = self.decoder.state().clone();
            snapshot.listener_ready = shared.listener_ready;
            snapshot.elapsed_secs = shared.elapsed_secs;
            let state = &snapshot;
            let detail = format!("capture timed out after {CAPTURE_TIMEOUT_SECS} seconds; packets={}; decryptedCommands={}; keyExchangeCaptured={}; characters={}; lightCones={}; relics={}; achievements={}; missingDispatchKey={:?}; lastTransportError={}",
                state.packet_count, state.command_count, state.has_key_exchange, state.character_count, state.light_cone_count,
                state.relic_count, state.achievement_count, state.missing_dispatch_key, state.last_transport_error.as_deref().unwrap_or("none"));
            let hint = if !state.listener_ready {
                LocalizedText::new(
                    "Windows 监听器未能完成启动，请复制完整错误。",
                    "The Windows listener did not finish starting. Copy the full error.",
                )
            } else if state.packet_count == 0 {
                LocalizedText::new("未收到星穹铁道流量。请确认游戏正在本机运行，重新抓包后再登录。",
                    "No Star Rail traffic was received. Check that the game is running on this PC, then start capture before logging in.")
            } else if state.missing_dispatch_key.is_some() && !state.has_key_exchange {
                LocalizedText::new("当前游戏使用的登录密钥尚未收录，无法解密数据。请更新 GGScanner；若最新版仍失败，请复制完整错误。",
                    "The current game's login key is missing, so its data cannot be decrypted. Update GGScanner; if the latest build still fails, copy the full error.")
            } else if state.command_count == 0 {
                LocalizedText::new("已收到流量，但未成功解码游戏消息。请重新登录并更新抓包器；若仍失败，请复制完整错误。", "Traffic arrived but no game messages decoded. Capture a new login and update the scanner; if this persists, copy the full error.")
            } else if !state.has_key_exchange {
                LocalizedText::new("已收到网络流量，但未捕获登录握手。请在启动游戏前开始抓包，再重新登录；若仍失败，请更新 GGScanner。",
                    "Traffic was received, but the login handshake was not captured. Start capture before launching the game, then log in again. If it still fails, update GGScanner.")
            } else if state.export_details.uid.is_none() {
                UID_HINT
            } else {
                LocalizedText::new("已读取游戏流量，但数据尚不完整。请重新登录抓包；若仍失败，请复制完整错误。",
                    "Game traffic was decoded, but some data is missing. Capture a new login; if it fails again, copy the full error.")
            };
            self.fail(HsrError::new("HSR-CAPTURE-TIMEOUT", hint, detail));
        }
    }

    fn handle_capture_task_result(&mut self, result: Result<HsrResult<()>, JoinError>) {
        let expected_stop = self
            .capture_cancel_token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled);
        self.capture_task = None;
        self.capture_cancel_token = None;

        match result {
            Ok(Ok(())) if expected_stop => {},
            Ok(Ok(())) => self.fail(HsrError::new(
                "HSR-CAPTURE-CLOSED",
                WORKER_HINT,
                "pktmon capture stream ended before all requested data was decoded",
            )),
            Ok(Err(_)) if expected_stop => {},
            Ok(Err(error)) => self.fail(error),
            Err(error) if expected_stop && error.is_cancelled() => {},
            Err(error) => self.fail(HsrError::new(
                "HSR-CAPTURE-WORKER",
                WORKER_HINT,
                format!("capture task join failed; cause={error}"),
            )),
        }
    }

    fn fail(&mut self, error: HsrError) {
        yas::log_error!("星铁抓包失败：{}", "HSR capture failed: {}", error);
        self.stop_capture();
        if let Ok(mut state) = self.state.lock() {
            // One datagram can release a login reset and several KCP commands
            // before decoding fails. Publish that reset rather than retaining
            // the previous account's UID/inventory in the shared status.
            let mut snapshot = self.decoder.state().clone();
            snapshot.listener_ready = state.listener_ready;
            snapshot.elapsed_secs = state.elapsed_secs;
            snapshot.complete = false;
            snapshot.error = Some(error);
            *state = snapshot;
            if let Some(observer) = &self.continuous {
                observer(&state);
            }
        }
    }
}

impl Drop for HsrCaptureMonitor {
    fn drop(&mut self) {
        if let Some(token) = &self.capture_cancel_token {
            token.cancel();
        }
    }
}

async fn wait_for_capture_task(
    task: &mut Option<JoinHandle<HsrResult<()>>>,
) -> Result<HsrResult<()>, JoinError> {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

async fn capture_task(
    cancel_token: CancellationToken,
    packet_tx: mpsc::UnboundedSender<Vec<u8>>,
    state: Arc<Mutex<HsrCaptureState>>,
) -> HsrResult<()> {
    #[cfg(target_os = "windows")]
    {
        let mut source = NativePacketSource::new()?;
        state.lock().expect("capture state poisoned").listener_ready = true;
        yas::log_info!(
            "星铁监听已启动：UDP 23301、23302",
            "HSR listener started: UDP 23301, 23302"
        );
        pump_packets(&mut source, &cancel_token, &packet_tx).await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (cancel_token, packet_tx);
        Err(HsrError::new(
            "HSR-CAPTURE-PLATFORM",
            PLATFORM_HINT,
            "pktmon capture is Windows-only; native capture task cannot start on this platform",
        ))
    }
}

trait PacketSource: Send {
    fn next_packet(&mut self) -> Pin<Box<dyn Future<Output = HsrResult<Vec<u8>>> + Send + '_>>;
}

async fn pump_packets<S: PacketSource>(
    source: &mut S,
    cancel_token: &CancellationToken,
    packet_tx: &mpsc::UnboundedSender<Vec<u8>>,
) -> HsrResult<()> {
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => return Ok(()),
            packet = source.next_packet() => {
                let packet = packet?;
                if packet_tx.send(packet).is_err() {
                    return Ok(());
                }
            },
        }
    }
}

#[cfg(target_os = "windows")]
struct NativePacketSource {
    stream: Box<dyn FusedStream<Item = Packet> + Unpin + Send>,
}

#[cfg(target_os = "windows")]
impl NativePacketSource {
    fn new() -> HsrResult<Self> {
        #[cfg(target_os = "windows")]
        yas::utils::ensure_admin().map_err(|error| {
            HsrError::new(
                "HSR-CAPTURE-ADMIN",
                CAPTURE_HINT,
                format!("administrator check failed; cause={error:#}"),
            )
        })?;

        let mut capture = Capture::new().map_err(|error| {
            let hint = yas::utils::packet_capture::initialization_hint(&error)
                .map(|(zh, en)| LocalizedText::new(zh, en))
                .unwrap_or(CAPTURE_HINT);
            HsrError::new(
                "HSR-CAPTURE-OPEN",
                hint,
                format!(
                    "pktmon capture could not be created; cause={:#}",
                    yas::utils::packet_capture::initialization_error(error)
                ),
            )
        })?;
        for (index, port) in HSR_PACKET_PORTS.into_iter().enumerate() {
            capture
                .add_filter(PktMonFilter {
                    name: format!("HSR UDP {index}"),
                    transport_protocol: Some(TransportProtocol::UDP),
                    port: port.into(),
                    ..PktMonFilter::default()
                })
                .map_err(|error| {
                    HsrError::new(
                        "HSR-CAPTURE-FILTER",
                        CAPTURE_HINT,
                        format!("pktmon UDP port {port} filter failed; cause={error}"),
                    )
                })?;
        }

        let stream = capture.stream().map_err(|error| {
            HsrError::new(
                "HSR-CAPTURE-STREAM",
                CAPTURE_HINT,
                format!("pktmon packet stream could not be started; cause={error}"),
            )
        })?;
        Ok(Self {
            stream: Box::new(stream.boxed().fuse()),
        })
    }
}

#[cfg(target_os = "windows")]
impl PacketSource for NativePacketSource {
    fn next_packet(&mut self) -> Pin<Box<dyn Future<Output = HsrResult<Vec<u8>>> + Send + '_>> {
        Box::pin(async move {
            match self.stream.next().await {
                Some(packet) => Ok(packet.payload.to_vec().clone()),
                None => Err(HsrError::new(
                    "HSR-CAPTURE-CLOSED",
                    WORKER_HINT,
                    "pktmon packet stream closed",
                )),
            }
        })
    }
}

fn load_embedded_dispatch_keys() -> HsrResult<HashMap<u32, Vec<u8>>> {
    let encoded: HashMap<u32, String> =
        serde_json::from_slice(include_bytes!("../../keys/hsr.json")).map_err(|error| {
            HsrError::new(
                "HSR-KEYS-JSON",
                KEYS_HINT,
                format!("embedded keys/hsr.json is invalid; cause={error}"),
            )
        })?;
    if encoded.is_empty() {
        return Err(HsrError::new(
            "HSR-KEYS-EMPTY",
            KEYS_HINT,
            "embedded keys/hsr.json contains no dispatch keys",
        ));
    }

    encoded
        .into_iter()
        .map(|(version, encoded)| {
            BASE64_STANDARD
                .decode(encoded)
                .map(|key| (version, key))
                .map_err(|error| {
                    HsrError::new(
                        "HSR-KEYS-BASE64",
                        KEYS_HINT,
                        format!("dispatch key {version} is invalid base64; cause={error}"),
                    )
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PendingPacketSource;

    #[test]
    fn inventory_capture_does_not_require_achievement_references() {
        let references = crate::load_embedded_gilore_reference()
            .unwrap()
            .without_achievements();
        let targets = CaptureTargets {
            achievements: false,
            ..CaptureTargets::default()
        };
        assert!(HsrPacketDecoder::new(references.clone(), targets).is_ok());
        assert!(HsrPacketDecoder::new(references, CaptureTargets::default()).is_ok());
    }

    impl PacketSource for PendingPacketSource {
        fn next_packet(&mut self) -> Pin<Box<dyn Future<Output = HsrResult<Vec<u8>>> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn packet_pump_honors_cancellation_while_source_is_pending() {
        let mut source = PendingPacketSource;
        let token = CancellationToken::new();
        token.cancel();
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

        pump_packets(&mut source, &token, &packet_tx)
            .await
            .expect("cancellation should be a clean stop");
        assert!(packet_rx.try_recv().is_err());
    }

    #[test]
    fn monitor_stop_cancels_the_owned_token_and_is_not_complete() {
        let state = Arc::new(Mutex::new(HsrCaptureState {
            capturing: true,
            ..HsrCaptureState::default()
        }));
        let mut monitor = HsrCaptureMonitor::new(
            state.clone(),
            crate::load_embedded_gilore_reference().unwrap(),
            CaptureTargets::default(),
        )
        .unwrap();
        let token = CancellationToken::new();
        monitor.capture_cancel_token = Some(token.clone());

        monitor.stop_capture();

        assert!(token.is_cancelled());
        let state = state.lock().unwrap();
        assert!(!state.capturing);
        assert!(!state.complete);
    }

    #[test]
    fn stop_before_start_is_terminal_and_never_spawns_native_capture() {
        let state = Arc::new(Mutex::new(HsrCaptureState::default()));
        let mut monitor = HsrCaptureMonitor::new(
            state.clone(),
            crate::load_embedded_gilore_reference().unwrap(),
            CaptureTargets::default(),
        )
        .unwrap();

        monitor.handle_command(HsrCaptureCommand::StopCapture);
        monitor.handle_command(HsrCaptureCommand::StartCapture);

        assert!(monitor.capture_task.is_none());
        assert!(monitor.capture_cancel_token.is_none());
        let state = state.lock().unwrap();
        assert!(!state.capturing);
        assert!(!state.complete);
    }

    #[test]
    fn embedded_dispatch_keys_are_pinned_and_decodable() {
        let keys = load_embedded_dispatch_keys().unwrap();
        assert_eq!(keys.len(), 32);
        assert!(keys.contains_key(&2766937619));
        assert!(keys.values().all(|key| key.len() >= 4_096));
        assert_eq!(HSR_PACKET_PORTS, [23301, 23302]);
    }

    #[test]
    fn current_inventory_wire_tags_decode_without_using_generated_writers() {
        use protobuf::Message;
        // HSR 4.7 wire records, independent of the generated serializers.
        let cone = proto::Equipment::Equipment::parse_from_bytes(&[
            8, 6, 16, 0xa1, 0x9c, 1, 24, 80, 32, 5, 48, 1, 64, 1, 96, 0xe9, 7,
        ])
        .unwrap();
        assert_eq!(
            (
                cone.tid,
                cone.unique_id,
                cone.level,
                cone.promotion,
                cone.rank,
                cone.equip_avatar_id
            ),
            (20001, 1, 80, 6, 5, 1001)
        );
        assert!(cone.is_protected);
        let relic = proto::Relic::Relic::parse_from_bytes(&[
            8, 1, 48, 15, 56, 0xe9, 7, 64, 1, 96, 6, 104, 0xd3, 0xdc, 3, 120, 1,
        ])
        .unwrap();
        assert_eq!(
            (
                relic.tid,
                relic.unique_id,
                relic.level,
                relic.main_affix_id,
                relic.equip_avatar_id
            ),
            (61011, 1, 15, 6, 1001)
        );
        assert!(relic.is_protected && relic.is_discarded);
    }

    #[test]
    fn missing_dispatch_key_survives_later_transport_noise_and_resets() {
        let mut decoder = HsrPacketDecoder::new(
            crate::load_embedded_gilore_reference().unwrap(),
            CaptureTargets::default(),
        )
        .unwrap();
        decoder.sniffer = GameSniffer::new();
        decoder
            .receive_packet(crate::network::tests::frame(77, 0, &[8, 42], &[0; 4096]))
            .unwrap();
        assert_eq!(decoder.state.missing_dispatch_key, Some(0));
        assert!(decoder
            .state
            .last_transport_error
            .as_deref()
            .unwrap()
            .contains("decryption key 0"));
        decoder.receive_packet(vec![0; 14]).unwrap();
        assert_eq!(decoder.state.missing_dispatch_key, Some(0));
        assert!(!decoder.state.has_key_exchange);
        decoder.reset();
        assert_eq!(decoder.state.missing_dispatch_key, None);
    }

    #[test]
    fn timeout_reports_transport_progress_and_stops_without_claiming_success() {
        let state = Arc::new(Mutex::new(HsrCaptureState::default()));
        let mut monitor = HsrCaptureMonitor::new(
            state.clone(),
            crate::load_embedded_gilore_reference().unwrap(),
            CaptureTargets::default(),
        )
        .unwrap();
        monitor.decoder.state.packet_count = 12;
        monitor.decoder.state.missing_dispatch_key = Some(2920436228);
        monitor.decoder.state.last_transport_error =
            Some("decryption key is missing for command".to_owned());
        monitor.started = Some(
            tokio::time::Instant::now() - std::time::Duration::from_secs(CAPTURE_TIMEOUT_SECS + 1),
        );
        monitor.check_timeout();
        let state = state.lock().unwrap();
        assert!(!state.capturing);
        assert!(!state.complete);
        let error = state.error.as_ref().unwrap();
        assert_eq!(error.code(), "HSR-CAPTURE-TIMEOUT");
        let message = error.localized_message(crate::Language::En);
        assert!(message.contains("packets=12"));
        assert!(message.contains("decryptedCommands=0"));
        assert!(message.contains("missingDispatchKey=Some(2920436228)"));
    }

    #[test]
    fn timeout_with_a_handshake_but_no_verified_uid_explains_how_to_retry() {
        let shared = Arc::new(Mutex::new(HsrCaptureState::default()));
        shared.lock().unwrap().listener_ready = true;
        let mut monitor = HsrCaptureMonitor::new(
            shared.clone(),
            crate::load_embedded_gilore_reference().unwrap(),
            CaptureTargets::default(),
        )
        .unwrap();
        monitor.decoder.state.packet_count = 12;
        monitor.decoder.state.command_count = 4;
        monitor.decoder.state.has_key_exchange = true;
        monitor.started = Some(
            tokio::time::Instant::now() - std::time::Duration::from_secs(CAPTURE_TIMEOUT_SECS + 1),
        );
        monitor.check_timeout();
        let state = shared.lock().unwrap();
        let error = state.error.as_ref().unwrap();
        assert!(error.hint().en.contains("UID could not be verified"));
        assert!(error.hint().en.contains("log in after starting capture"));
        assert!(!state.complete);
    }

    #[test]
    fn key_exchange_state_tracks_transport_and_resets_without_inventory() {
        use crate::network::tests::{frame, token};
        let keys = HashMap::from([(0, vec![0; 4096])]);
        let mut decoder = HsrPacketDecoder::new(
            crate::load_embedded_gilore_reference().unwrap(),
            CaptureTargets::default(),
        )
        .unwrap();
        decoder.dispatch_keys = keys.clone();
        decoder.sniffer = GameSniffer::new().set_initial_keys(keys);
        decoder
            .receive_packet(frame(77, 0, &[8, 42], &[0; 4096]))
            .unwrap();
        assert!(decoder.state.command_count > 0);
        assert!(!decoder.state.has_key_exchange);
        decoder
            .receive_packet(frame(77, 1, &token(0x1234567890, 993), &[0; 4096]))
            .unwrap();
        assert!(decoder.state.has_key_exchange);
        assert!(!decoder.state.has_characters);
        assert!(!decoder.state.has_items);
        assert!(!decoder.state.complete);
        decoder.reset();
        assert!(!decoder.state.has_key_exchange);
    }
}

#[cfg(test)]
mod identity_tests;
