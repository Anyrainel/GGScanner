/// Capture monitor: orchestrates packet capture, decryption, and data accumulation.
///
/// Ported from irminsul's `monitor.rs`, simplified for yas integration.
/// The monitor runs on a tokio runtime and communicates via channels.
///
/// ## Version resilience
///
/// Both command IDs and protobuf field numbers change across game versions.
/// The monitor is resilient to both:
///
/// - **Command IDs**: every decrypted command is tested heuristically,
///   regardless of its `command_id`.
/// - **Outer field numbers and wrappers**: instead of relying on a fixed proto
///   schema for the outer container, we first try every repeated
///   length-delimited field. Item packets also get a wire-level fallback that
///   finds structurally valid `Item` records inside opaque/prefixed wrappers,
///   without assuming a wrapper field number, item tag, or byte offset.
///
/// Dispatch keys are loaded from an external `keys/gi.json` file first
/// (next to the exe), falling back to the compile-time embedded copy.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use auto_artifactarium::r#gen::protos::{AvatarInfo, Item, Unk};
use auto_artifactarium::{GameCommand, GamePacket, GameSniffer, PacketDirection};
use base64::prelude::*;
use protobuf::Message;
use protobuf::UnknownValueRef;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use yas::{log_debug, log_error, log_info, log_warn};

use super::packet_capture::{CaptureError, PacketCapture};
use super::packet_dump::PacketDump;
use super::player_data::{CaptureExportSettings, PlayerData};

// --- Heuristic thresholds for field-number-agnostic packet matching ---
// These control how many valid sub-messages a repeated field must contain
// before we accept it as an item/avatar packet. Tuned to avoid false positives
// while accepting real game data (accounts typically have 100+ items, 20+ avatars).
const MIN_ITEM_ENTRIES: usize = 10;
const MIN_GEAR_COUNT: usize = 5; // items with actual weapon/reliquary equip data
const MIN_AVATAR_ENTRIES: usize = 4;
const MIN_AVATARS_WITH_PROPS: usize = 2; // avatars with non-empty prop_map
const MIN_ACHIEVEMENT_ENTRIES: usize = 5;
const MIN_ACHIEVEMENT_ID: u64 = 80_000;
const MAX_ACHIEVEMENT_ID_EXCLUSIVE: u64 = 100_000;
const MIN_FINISH_TIMESTAMP: u64 = 1_500_000_000;
const MAX_FINISH_TIMESTAMP: u64 = 4_102_444_800; // 2100-01-01 UTC
use crate::scanner::common::models::GoodExport;

/// Commands the UI can send to the monitor.
pub enum CaptureCommand {
    StopCapture,
}

/// State shared between the monitor and UI.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CaptureState {
    pub listener_ready: bool,
    pub elapsed_secs: u64,
    pub packet_count: usize,
    pub command_count: usize,
    pub rejected_commands: usize,
    pub unmatched_commands: usize,
    pub last_transport_error: Option<String>,
    pub capturing: bool,
    /// All requested packet categories have been received; capture auto-stopped.
    pub complete: bool,
    /// Actual token response recognized by the transport; independent of inventory.
    pub has_key_exchange: bool,
    /// Verified game UID for this login, available before the initial inventory.
    pub uid: Option<u32>,
    pub has_characters: bool,
    pub has_items: bool,
    pub has_achievements: bool,
    pub character_count: usize,
    pub weapon_count: usize,
    pub artifact_count: usize,
    pub achievement_count: usize,
    pub error: Option<String>,
}

impl Default for CaptureState {
    fn default() -> Self {
        Self {
            listener_ready: false,
            elapsed_secs: 0,
            packet_count: 0,
            command_count: 0,
            rejected_commands: 0,
            unmatched_commands: 0,
            last_transport_error: None,
            capturing: false,
            complete: false,
            has_key_exchange: false,
            uid: None,
            has_characters: false,
            has_items: false,
            has_achievements: false,
            character_count: 0,
            weapon_count: 0,
            artifact_count: 0,
            achievement_count: 0,
            error: None,
        }
    }
}

impl CaptureState {
    fn selected_complete(&self, targets: &CaptureExportSettings) -> bool {
        self.capturing
            && self.uid.is_some()
            && (!targets.include_characters || self.has_characters)
            && (!targets.include_weapons || self.has_items || self.weapon_count > 0)
            && (!targets.include_artifacts || self.has_items || self.artifact_count > 0)
            && (!targets.include_achievements || self.has_achievements)
    }

    pub fn diagnostics(&self) -> String {
        format!("listenerReady={}; elapsed={}s; packets={}; decryptedCommands={}; rejectedCommands={}; unmatchedCommands={}; keyExchange={}; uidVerified={}; characters={}; weapons={}; artifacts={}; achievements={}; lastTransportError={}", self.listener_ready, self.elapsed_secs, self.packet_count, self.command_count, self.rejected_commands, self.unmatched_commands, self.has_key_exchange, self.uid.is_some(), self.character_count, self.weapon_count, self.artifact_count, self.achievement_count, self.last_transport_error.as_deref().unwrap_or("none"))
    }

    pub fn timeout_hint(&self) -> (&'static str, &'static str) {
        if !self.listener_ready {
            (
                "Windows 监听器未能完成启动，请复制完整错误。",
                "The Windows listener did not finish starting. Copy the full error.",
            )
        } else if self.packet_count == 0 {
            ("监听已启动，但未收到游戏流量。请确认游戏在本机运行，并在启动游戏前开始抓包。", "The listener started but received no game traffic. Run the game on this PC and start capture before launching it.")
        } else if self.command_count == 0 {
            ("已收到流量，但未成功解码游戏消息。请重新登录并更新抓包器，复制完整错误以检查协议或密钥兼容性。", "Traffic arrived but no game messages decoded. Capture a new login and update the scanner. Copy the full error to check protocol or key compatibility.")
        } else if !self.has_key_exchange {
            ("已解码部分消息，但未捕获登录握手。请关闭游戏，在开始抓包后重新登录。", "Some messages decoded but the login handshake was not captured. Close the game and log in after starting capture.")
        } else if self.uid.is_none() {
            ("尚未验证游戏 UID。请更新 GGScanner，并在开始抓包后重新登录；若仍失败，请复制完整错误。", "The game UID has not been verified. Update GGScanner and log in after starting capture; if this persists, copy the full error.")
        } else {
            ("已解码游戏消息，但尚未收到全部所选数据。需要成就时请打开成就界面；若仍失败，请复制完整错误。", "Game messages decoded but selected data is incomplete. Open Achievements if selected; if this persists, copy the full error.")
        }
    }
}

/// The capture monitor. Runs on a tokio runtime.
pub struct CaptureMonitor {
    player_data: PlayerData,
    sniffer: GameSniffer,
    state: Arc<Mutex<CaptureState>>,
    capture_cancel_token: Option<CancellationToken>,
    capture_task: Option<JoinHandle<Result<()>>>,
    packet_tx: mpsc::UnboundedSender<Vec<u8>>,
    packet_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    packet_dump: Option<PacketDump>,
    targets: CaptureExportSettings,
    continuous: Option<crate::continuous::ContinuousCapture>,
}

impl CaptureMonitor {
    /// Initialize the monitor: load data cache, set up sniffer.
    #[cfg(target_os = "windows")]
    pub fn new(state: Arc<Mutex<CaptureState>>, dump_packets: bool) -> Result<Self> {
        yas::utils::ensure_admin()?;

        log_info!(
            "正在加载抓包所需的游戏数据",
            "Loading game data for capture"
        );
        let data_cache = crate::game_data::load()
            .context("capture game data could not be loaded")?
            .capture;
        let player_data = PlayerData::new(data_cache);
        log_info!("正在加载登录密钥", "Loading dispatch keys");
        let keys = load_keys().context("capture dispatch keys could not be loaded")?;
        let sniffer = GameSniffer::new().set_initial_keys(keys);
        let (packet_tx, packet_rx) = mpsc::unbounded_channel();

        let packet_dump = if dump_packets {
            let dump = PacketDump::new(&crate::cli::exe_dir().join("debug_capture"))
                .context("packet dump session could not be created")?;
            log_info!(
                "数据包转储已开启 → {}",
                "Packet dump enabled → {}",
                dump.directory().display(),
            );
            Some(dump)
        } else {
            None
        };

        Ok(Self {
            player_data,
            sniffer,
            state,
            capture_cancel_token: None,
            capture_task: None,
            packet_tx,
            packet_rx,
            packet_dump,
            targets: CaptureExportSettings::default(),
            continuous: None,
        })
    }

    /// Non-Windows stub: the packet source (pktmon) is Windows-only, so
    /// capture fails fast with a clear message instead of downloading the
    /// data cache first.
    #[cfg(not(target_os = "windows"))]
    pub fn new(state: Arc<Mutex<CaptureState>>, dump_packets: bool) -> Result<Self> {
        let _ = (&state, dump_packets);
        anyhow::bail!(
            "Packet capture is Windows-only (pktmon); use OCR scanning on this platform."
        );
    }

    /// Run one capture, returning its export only after the packet task stops.
    /// Completion must not depend on the UI requesting export or closing a sender.
    pub async fn run(
        mut self,
        mut cmd_rx: mpsc::UnboundedReceiver<CaptureCommand>,
        settings: CaptureExportSettings,
    ) -> Result<Option<GoodExport>> {
        // A Stop/close during initialization must not start packet capture later.
        if cmd_rx.is_closed() || cmd_rx.try_recv().is_ok() {
            return Ok(None);
        }
        self.targets = settings.clone();
        self.start_capture();
        self.run_capture(cmd_rx, &settings).await
    }

    pub async fn run_continuous(
        mut self,
        cmd_rx: mpsc::UnboundedReceiver<CaptureCommand>,
        latest: crate::continuous::ContinuousCapture,
    ) -> Result<Option<GoodExport>> {
        self.continuous = Some(latest);
        self.run(cmd_rx, CaptureExportSettings::default()).await
    }

    async fn run_capture(
        mut self,
        mut cmd_rx: mpsc::UnboundedReceiver<CaptureCommand>,
        settings: &CaptureExportSettings,
    ) -> Result<Option<GoodExport>> {
        let started = tokio::time::Instant::now();
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            if self
                .state
                .lock()
                .is_ok_and(|s| s.complete || s.error.is_some())
            {
                break;
            }
            tokio::select! {
                _ = timer.tick() => {
                    let mut state = self.state.lock().expect("capture state poisoned");
                    state.elapsed_secs = started.elapsed().as_secs();
                    if state.elapsed_secs % 15 == 0 {
                        log_info!("抓包诊断：{}", "Capture diagnostics: {}", state.diagnostics());
                    }
                    if self.continuous.is_none() && state.elapsed_secs >= 300 {
                        state.error = Some(format!("{}; {}", state.timeout_hint().1, state.diagnostics()));
                        log_warn!("抓包等待超时：{}", "Capture timed out: {}", yas::lang::localize(state.error.as_deref().unwrap()));
                    }
                }
                Some(packet) = self.packet_rx.recv() => {
                    self.handle_packet(packet);
                }
                _ = cmd_rx.recv() => break,
                result = wait_for_capture_task(&mut self.capture_task) => {
                    self.handle_capture_task_result(result);
                }
                else => break,
            }
        }

        // Keep native task ownership until pktmon's stream and worker have
        // actually been dropped, on completion, cancellation, and failure.
        self.stop_capture();
        if let Some(task) = self.capture_task.take() {
            self.handle_capture_task_result(task.await);
        }
        let state = self.state.lock().expect("capture state poisoned").clone();
        if let Some(dump) = &mut self.packet_dump {
            let result = dump.write_state("end", &state);
            self.check_dump_write(result);
        }
        log_info!("抓包结束：{}", "Capture ended: {}", state.diagnostics());
        if let Some(error) = state.error {
            return Err(anyhow!(error));
        }
        if state.complete {
            anyhow::ensure!(
                self.player_data.uid().is_some(),
                "A completed Genshin packet capture must have a verified game UID."
            );
            return self.player_data.export(settings).map(Some);
        }
        Ok(None)
    }

    fn start_capture(&mut self) {
        self.player_data.begin_capture();
        let cancel_token = CancellationToken::new();
        if let Ok(mut state) = self.state.lock() {
            *state = CaptureState {
                capturing: true,
                ..CaptureState::default()
            };
        }
        self.capture_task = Some(tokio::spawn(capture_task(
            cancel_token.clone(),
            self.packet_tx.clone(),
            self.state.clone(),
        )));
        self.capture_cancel_token = Some(cancel_token);
    }

    fn stop_capture(&mut self) {
        if let Some(token) = &self.capture_cancel_token {
            token.cancel();
        }
        if let Ok(mut state) = self.state.lock() {
            state.capturing = false;
        }
    }

    fn handle_capture_task_result(&mut self, result: std::result::Result<Result<()>, JoinError>) {
        let expected_stop = self
            .capture_cancel_token
            .as_ref()
            .map_or(false, CancellationToken::is_cancelled);
        self.capture_task = None;
        self.capture_cancel_token = None;
        update_state_from_capture_task(&self.state, result, expected_stop);
    }

    fn handle_packet(&mut self, packet: Vec<u8>) {
        if let Some(dump) = &mut self.packet_dump {
            let result = dump.write_packet(&packet);
            self.check_dump_write(result);
        }
        self.process_packet(packet);
        if let Some(dump) = &mut self.packet_dump {
            let state = self.state.lock().expect("capture state poisoned").clone();
            let result = dump.write_state("packet", &state);
            self.check_dump_write(result);
        }
    }

    fn check_dump_write(&mut self, result: std::io::Result<()>) {
        if let Err(error) = result {
            log_warn!(
                "抓包调试数据无法继续写入，已停止本次转储。完整错误详情: {}",
                "Capture debug data could not be written; dumping has stopped for this session. Full error details: {}",
                error,
            );
            // A partially written PCAP record must not be followed by more records.
            self.packet_dump = None;
        }
    }

    fn accepts_incremental_updates(&self) -> bool {
        self.continuous.is_some()
            && self
                .state
                .lock()
                .is_ok_and(|state| state.has_characters && state.has_items)
    }

    fn reset_login(&mut self) {
        self.player_data.begin_capture();
        if let Some(latest) = &self.continuous {
            latest.reset();
        }
        let mut state = self.state.lock().expect("capture state poisoned");
        let listener_ready = state.listener_ready;
        *state = CaptureState {
            capturing: true,
            listener_ready,
            has_key_exchange: self.sniffer.key_exchange_captured(),
            ..CaptureState::default()
        };
    }

    fn process_packet(&mut self, packet: Vec<u8>) {
        let decoded = self.sniffer.receive_packet(packet);
        if let Ok(mut state) = self.state.lock() {
            state.packet_count += 1;
            state.rejected_commands = self.sniffer.rejected_commands();
            state.last_transport_error = self.sniffer.last_error().map(str::to_owned);
            let captured = self.sniffer.key_exchange_captured();
            if captured && !state.has_key_exchange {
                log_info!("已捕获登录握手", "Login handshake captured");
            }
            state.has_key_exchange = captured;
        }
        if matches!(
            decoded,
            Some(GamePacket::Connection(
                auto_artifactarium::ConnectionPacket::HandshakeRequested
                    | auto_artifactarium::ConnectionPacket::Disconnected { .. }
            ))
        ) {
            self.reset_login();
        }
        let Some(GamePacket::Commands(commands)) = decoded else {
            return;
        };
        self.process_commands(commands);
    }

    fn process_commands(&mut self, commands: Vec<GameCommand>) {
        // Heuristic matching: try parsing every command as item/avatar packets
        // regardless of command_id.  This survives command ID rotation across
        // game versions.
        let mut data_changed = false;
        for command in commands {
            self.state
                .lock()
                .expect("capture state poisoned")
                .command_count += 1;
            // Dump raw decrypted commands when enabled
            if let Some(dump) = &mut self.packet_dump {
                let result = dump.write_command(command.command_id, &command.to_bytes());
                self.check_dump_write(result);
            }

            if command.direction != PacketDirection::Received {
                continue;
            }
            if command.is_token_response {
                // A token response is a login boundary even if its handshake
                // control frame was missed, and even when reconnecting to the
                // same UID. Never attach old inventory to a new identity.
                self.reset_login();
                self.player_data.set_uid(command.account_uid);
                self.state.lock().expect("capture state poisoned").uid = command.account_uid;
                if command.account_uid.is_none() {
                    log_warn!(
                        "登录响应中的游戏 UID 无法验证，请更新扫描器后重新登录。",
                        "The login response's game UID could not be verified. Update the scanner and log in again."
                    );
                    let error = "The Genshin login UID could not be verified. Update GGScanner and log in again.";
                    self.state.lock().expect("capture state poisoned").error = Some(error.into());
                    if let Some(latest) = &self.continuous {
                        latest.fail(error.into());
                    }
                }
                continue;
            }

            if let Some(guids) = self
                .player_data
                .uid()
                .filter(|_| self.accepts_incremental_updates())
                .and_then(|uid| super::inventory_updates::deleted_items(&command.proto_data, uid))
            {
                let removed = self.player_data.remove_items(&guids);
                data_changed |= removed > 0;
                let mut state = self.state.lock().expect("capture state poisoned");
                state.weapon_count = self.player_data.weapon_count();
                state.artifact_count = self.player_data.artifact_count();
                log_debug!(
                    "捕获到物品删除数据包 (cmd={})，移除 {} 个物品",
                    "Captured item deletion packet (cmd={}), removed {} items",
                    command.command_id,
                    removed,
                );
            } else if let Some(achievement_ids) = try_match_achievements(&command.proto_data) {
                log_info!(
                    "捕获到成就数据包 (cmd={})，共 {} 个已完成成就",
                    "Captured achievement packet (cmd={}), {} completed achievements",
                    command.command_id,
                    achievement_ids.len(),
                );
                data_changed = true;
                self.player_data.process_achievements(&achievement_ids);
                if let Ok(mut state) = self.state.lock() {
                    state.has_achievements = true;
                    state.achievement_count = self.player_data.achievement_count();
                }
            } else if let Some(items) = try_match_items(&command.proto_data).or_else(|| {
                if !self.accepts_incremental_updates() {
                    return None;
                }
                if try_match_avatars(&command.proto_data).is_some() {
                    return None;
                }
                let items = find_nested_items(&command.proto_data);
                (!items.is_empty()).then_some(items)
            }) {
                log_info!(
                    "捕获到物品数据包 (cmd={})，共 {} 个物品",
                    "Captured item packet (cmd={}), {} items",
                    command.command_id,
                    items.len(),
                );
                data_changed = true;
                self.player_data.process_items(&items);
                if let Ok(mut state) = self.state.lock() {
                    state.weapon_count = self.player_data.weapon_count();
                    state.artifact_count = self.player_data.artifact_count();
                    // Recent game versions may split store categories across
                    // multiple notifications. Do not stop after a weapon-only
                    // batch before the artifact batch has arrived.
                    // Once the baseline has arrived, consuming the last item of
                    // a family must not make subsequent deltas unavailable.
                    state.has_items |= state.weapon_count > 0 && state.artifact_count > 0;
                }
            } else if let Some(avatars) = try_match_avatars(&command.proto_data).or_else(|| {
                if !self.accepts_incremental_updates() {
                    return None;
                }
                find_best_field::<AvatarInfo>(&command.proto_data, 1, |avatar| {
                    avatar.avatar_type == 1
                        && avatar.avatar_id >= 10_000_000
                        && !avatar.prop_map.is_empty()
                })
                .map(|(_, avatars)| avatars)
            }) {
                log_info!(
                    "捕获到角色数据包 (cmd={})，共 {} 个角色",
                    "Captured avatar packet (cmd={}), {} avatars",
                    command.command_id,
                    avatars.len(),
                );
                data_changed = true;
                if self.continuous.is_some() {
                    self.player_data.merge_characters(&avatars);
                } else {
                    self.player_data.process_characters(&avatars);
                }
                if let Ok(mut state) = self.state.lock() {
                    state.has_characters = true;
                    state.character_count = self.player_data.character_count();
                }
            } else {
                let mut state = self.state.lock().expect("capture state poisoned");
                state.unmatched_commands += 1;
                if state.unmatched_commands <= 5 {
                    log_debug!(
                        "未识别的数据包：cmd={}，长度={}",
                        "Unmatched command: cmd={}, length={}",
                        command.command_id,
                        command.proto_data.len()
                    );
                }
            }
        }

        // Auto-stop once the normal GOOD data and every requested extension
        // have arrived. Achievement capture is optional for backwards-compatible
        // workflows, but defaults on in GGScanner.
        let should_stop = self
            .state
            .lock()
            .is_ok_and(|state| state.selected_complete(&self.targets));
        if let Some(latest) = self.continuous.as_ref().filter(|_| data_changed) {
            let state = self.state.lock().expect("capture state poisoned");
            if state.has_characters && state.has_items && self.player_data.uid().is_some() {
                let settings = CaptureExportSettings {
                    include_achievements: state.has_achievements,
                    ..self.targets.clone()
                };
                match self
                    .player_data
                    .export(&settings)
                    .and_then(|data| Ok(serde_json::to_value(data)?))
                {
                    Ok(data) => latest.publish(data),
                    Err(error) => latest.fail(error.to_string()),
                }
            }
        }
        if should_stop && self.continuous.is_none() {
            log_info!(
                "已收集到所有数据，自动停止抓包",
                "All data collected, stopping capture automatically"
            );
            self.stop_capture();
            if let Ok(mut state) = self.state.lock() {
                state.complete = true;
            }
        }
    }
}

async fn wait_for_capture_task(
    task: &mut Option<JoinHandle<Result<()>>>,
) -> std::result::Result<Result<()>, JoinError> {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

fn capture_task_failure(
    result: std::result::Result<Result<()>, JoinError>,
    expected_stop: bool,
) -> Option<String> {
    match result {
        Ok(Ok(())) if expected_stop => None,
        Ok(Ok(())) => Some("capture task exited unexpectedly without reporting an error".into()),
        Ok(Err(error)) => Some(format!("{error:#}")),
        Err(join_error) if join_error.is_panic() => {
            let fallback = join_error.to_string();
            let payload = join_error.into_panic();
            let message = payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or(fallback);
            Some(format!("capture task panicked: {message}"))
        },
        Err(join_error) => Some(format!(
            "capture task was cancelled unexpectedly: {join_error}"
        )),
    }
}

fn update_state_from_capture_task(
    state: &Arc<Mutex<CaptureState>>,
    result: std::result::Result<Result<()>, JoinError>,
    expected_stop: bool,
) {
    let error = capture_task_failure(result, expected_stop);
    if let Some(error) = &error {
        log_error!(
            "抓包任务失败: {}",
            "Capture task failed: {}",
            yas::lang::localize(error)
        );
    }
    if let Ok(mut state) = state.lock() {
        state.capturing = false;
        if let Some(error) = error {
            state.error = Some(error);
        }
    }
}

/// Extract the repeated field with the most entries that parse as `T` and pass
/// the `filter`.  Returns `(best_field_number, parsed_entries)`.
///
/// This is the core of our field-number-agnostic packet matching: parse the
/// outer message as `Unk` (generic protobuf), group all length-delimited
/// values by field number, try parsing each group as `T`, and pick the field
/// with the most valid results.
fn find_best_field<T: Message>(
    proto_data: &[u8],
    min_entries: usize,
    filter: impl Fn(&T) -> bool,
) -> Option<(u32, Vec<T>)> {
    let unk = Unk::parse_from_bytes(proto_data).ok()?;

    let mut field_map: HashMap<u32, Vec<&[u8]>> = HashMap::new();
    for (field_num, value) in unk.unknown_fields().iter() {
        if let UnknownValueRef::LengthDelimited(bytes) = value {
            field_map.entry(field_num).or_default().push(bytes);
        }
    }

    let mut best: Option<(u32, Vec<T>)> = None;
    for (field_num, blobs) in &field_map {
        if blobs.len() < min_entries {
            continue;
        }
        let parsed: Vec<T> = blobs
            .iter()
            .filter_map(|b| T::parse_from_bytes(b).ok())
            .filter(|v| filter(v))
            .collect();
        if parsed.len() >= min_entries
            && best.as_ref().map_or(true, |(_, b)| parsed.len() > b.len())
        {
            best = Some((*field_num, parsed));
        }
    }
    best
}

/// Field-number-agnostic achievement packet detection.
///
/// Achievement entries are generic protobuf messages whose field numbers are
/// obfuscated per game version. We identify the repeated entry field by its
/// structure, then infer the ID and finish-time fields from value domains. This
/// deliberately avoids command IDs, protobuf tags, byte offsets, and a single
/// sentinel achievement ID. Only records with a finish timestamp are exported:
/// those are the achievements the account has actually completed.
fn try_match_achievements(proto_data: &[u8]) -> Option<Vec<u32>> {
    let outer = Unk::parse_from_bytes(proto_data).ok()?;
    let mut field_map: HashMap<u32, Vec<&[u8]>> = HashMap::new();
    for (field_num, value) in outer.unknown_fields().iter() {
        if let UnknownValueRef::LengthDelimited(bytes) = value {
            field_map.entry(field_num).or_default().push(bytes);
        }
    }

    let mut best: Option<(usize, Vec<u32>)> = None;
    for blobs in field_map.values() {
        if blobs.len() < MIN_ACHIEVEMENT_ENTRIES {
            continue;
        }

        let records: Vec<HashMap<u32, u64>> = blobs
            .iter()
            .filter_map(|bytes| parse_varint_record(bytes))
            .collect();
        if records.len() < MIN_ACHIEVEMENT_ENTRIES {
            continue;
        }

        let Some(id_tag) = infer_achievement_id_tag(&records) else {
            continue;
        };
        let Some(timestamp_tag) = infer_finish_timestamp_tag(&records) else {
            continue;
        };

        let mut all_ids = Vec::new();
        let mut completed_ids = Vec::new();
        for record in &records {
            let Some(&id) = record.get(&id_tag) else {
                continue;
            };
            all_ids.push(id as u32);
            if record.get(&timestamp_tag).is_some_and(|timestamp| {
                (MIN_FINISH_TIMESTAMP..=MAX_FINISH_TIMESTAMP).contains(timestamp)
            }) {
                completed_ids.push(id as u32);
            }
        }

        all_ids.sort_unstable();
        all_ids.dedup();
        // A real all-achievement notification has a unique ID on nearly every
        // repeated entry. This rejects unrelated repeated-varint messages.
        if all_ids.len() * 5 < records.len() * 4 {
            continue;
        }
        completed_ids.sort_unstable();
        completed_ids.dedup();

        if best
            .as_ref()
            .map_or(true, |(record_count, _)| records.len() > *record_count)
        {
            best = Some((records.len(), completed_ids));
        }
    }

    best.map(|(_, ids)| ids)
}

fn parse_varint_record(bytes: &[u8]) -> Option<HashMap<u32, u64>> {
    let record = Unk::parse_from_bytes(bytes).ok()?;
    let mut values = HashMap::new();
    for (field_num, value) in record.unknown_fields().iter() {
        match value {
            UnknownValueRef::Varint(value) => {
                values.insert(field_num, value);
            },
            // AchievementInfo consists solely of scalar varints. Rejecting
            // other wire types is a useful false-positive guard.
            _ => return None,
        }
    }
    (!values.is_empty()).then_some(values)
}

fn infer_achievement_id_tag(records: &[HashMap<u32, u64>]) -> Option<u32> {
    let mut counts = HashMap::<u32, usize>::new();
    for record in records {
        for (&tag, &value) in record {
            if (MIN_ACHIEVEMENT_ID..MAX_ACHIEVEMENT_ID_EXCLUSIVE).contains(&value) {
                *counts.entry(tag).or_default() += 1;
            }
        }
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count >= MIN_ACHIEVEMENT_ENTRIES)
        .max_by_key(|(_, count)| *count)
        .map(|(tag, _)| tag)
}

fn infer_finish_timestamp_tag(records: &[HashMap<u32, u64>]) -> Option<u32> {
    let mut counts = HashMap::<u32, usize>::new();
    for record in records {
        for (&tag, &value) in record {
            if (MIN_FINISH_TIMESTAMP..=MAX_FINISH_TIMESTAMP).contains(&value) {
                *counts.entry(tag).or_default() += 1;
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(tag, _)| tag)
}

/// Field-number-agnostic item packet detection.
///
/// Survives both command ID rotation AND outer field number changes.
fn try_match_items(proto_data: &[u8]) -> Option<Vec<Item>> {
    let direct = find_best_field::<Item>(proto_data, MIN_ITEM_ENTRIES, |item| {
        item.item_id != 0 && item.guid != 0
    });

    let (source, items) = match direct {
        Some((field, items)) => (format!("field={field}"), items),
        None => {
            // Avatar notifications can contain their equipped Item records.
            // They are real gear, but only a partial inventory snapshot, so
            // prefer the stronger avatar packet classification when it fits.
            if try_match_avatars(proto_data).is_some() {
                return None;
            }
            (
                "nested wire scan".to_string(),
                find_nested_items(proto_data),
            )
        },
    };

    let gear_count = items
        .iter()
        .filter(|i| i.has_equip() && (i.equip().has_weapon() || i.equip().has_reliquary()))
        .count();
    if gear_count < MIN_GEAR_COUNT {
        log_debug!(
            "物品数据包候选被拒（来源={}，{} 个物品，{} 个武器/圣遗物）",
            "Item packet candidate rejected ({}, {} items, {} weapons/artifacts)",
            source,
            items.len(),
            gear_count,
        );
        return None;
    }

    log_debug!(
        "物品数据包匹配成功（来源={}，{} 个物品）",
        "Item packet matched ({}, {} items)",
        source,
        items.len(),
    );
    Some(items)
}

/// Find equipment `Item` records anywhere in a command payload.
///
/// Some versions wrap repeated item records in an opaque blob with a non-proto
/// prefix. Walking possible length-delimited wire records byte-by-byte lets us
/// enter such blobs naturally and does not bake in the wrapper's field number,
/// the repeated-item tag, or the prefix length. Structural validation plus GUID
/// deduplication keeps unrelated protobuf messages from becoming item packets.
fn find_nested_items(data: &[u8]) -> Vec<Item> {
    let mut items_by_guid = HashMap::new();
    let mut offset = 0usize;

    while offset < data.len() {
        let Some((key, key_len)) = read_varint(&data[offset..]) else {
            offset += 1;
            continue;
        };
        if key >> 3 == 0 || key & 0x07 != 2 {
            offset += 1;
            continue;
        }

        let len_offset = offset + key_len;
        let Some((value_len, value_len_size)) = data.get(len_offset..).and_then(read_varint) else {
            offset += 1;
            continue;
        };
        let Ok(value_len) = usize::try_from(value_len) else {
            offset += 1;
            continue;
        };
        let value_start = len_offset + value_len_size;
        let Some(value_end) = value_start.checked_add(value_len) else {
            offset += 1;
            continue;
        };
        if value_end > data.len() {
            offset += 1;
            continue;
        }

        if let Ok(item) = Item::parse_from_bytes(&data[value_start..value_end]) {
            if item.item_id != 0
                && item.guid != 0
                && item.has_equip()
                && (item.equip().has_weapon() || item.equip().has_reliquary())
            {
                items_by_guid.insert(item.guid, item);
                offset = value_end;
                continue;
            }
        }

        // The value may itself contain a prefixed protobuf stream, so advance
        // by one byte rather than skipping the whole length-delimited value.
        offset += 1;
    }

    items_by_guid.into_values().collect()
}

fn read_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, byte) in data.iter().copied().take(10).enumerate() {
        let shift = u32::try_from(index).ok()?.checked_mul(7)?;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

/// Field-number-agnostic avatar packet detection.
///
/// Requires ≥4 avatars and ≥2 with non-empty `prop_map`
/// (every real avatar has property entries like level/ascension).
fn try_match_avatars(proto_data: &[u8]) -> Option<Vec<AvatarInfo>> {
    let (field, avatars) = find_best_field::<AvatarInfo>(proto_data, MIN_AVATAR_ENTRIES, |a| {
        a.avatar_id != 0 && a.guid != 0
    })?;

    let has_props = avatars.iter().filter(|a| !a.prop_map.is_empty()).count();
    if has_props < MIN_AVATARS_WITH_PROPS {
        log_debug!(
            "角色数据包候选被拒（field={}, {} 个角色，仅 {} 个有属性）",
            "Avatar packet candidate rejected (field={}, {} avatars, only {} with props)",
            field,
            avatars.len(),
            has_props,
        );
        return None;
    }

    log_debug!(
        "角色数据包匹配成功（field={}, {} 个角色）",
        "Avatar packet matched (field={}, {} avatars)",
        field,
        avatars.len(),
    );
    Some(avatars)
}

async fn capture_task(
    cancel_token: CancellationToken,
    packet_tx: mpsc::UnboundedSender<Vec<u8>>,
    state: Arc<Mutex<CaptureState>>,
) -> Result<()> {
    let mut capture = PacketCapture::new().map_err(|error| {
        capture_error_with_context(error, "创建抓包失败 / Error creating packet capture")
    })?;
    state.lock().expect("capture state poisoned").listener_ready = true;
    log_info!(
        "监听已启动：UDP 22101、22102",
        "Listener started: UDP 22101, 22102"
    );
    loop {
        let packet = tokio::select!(
            packet = capture.next_packet() => packet,
            _ = cancel_token.cancelled() => break,
        );
        let packet = match packet {
            Ok(packet) => packet,
            Err(error) => {
                handle_capture_read_error(error)?;
                continue;
            },
        };
        if let Err(e) = packet_tx.send(packet) {
            return Err(anyhow!("capture packet receiver closed: {e}"));
        }
    }
    log_info!("抓包已停止", "Packet capture stopped");
    Ok(())
}

/// Add user-facing operation context without flattening the nested pktmon error.
///
/// `CaptureError` predates `std::error::Error` support, but its variants retain
/// the original `anyhow::Error`. Destructuring it here keeps that source chain
/// available to the GUI's alternate (`{:#}`) formatter.
fn capture_error_with_context(error: CaptureError, operation: &'static str) -> anyhow::Error {
    let error = match error {
        CaptureError::Filter(source) => source.context("pktmon packet-filter setup failed"),
        CaptureError::Capture {
            has_captured,
            error: source,
        } => source.context(format!(
            "pktmon capture failed (has_captured = {has_captured})"
        )),
        CaptureError::CaptureClosed => anyhow!("packet capture stream closed"),
    };
    error.context(operation)
}

/// Return permanent stream closure to the retained task so the monitor can
/// publish it to `CaptureState::error`. Other receive failures remain retryable.
fn handle_capture_read_error(error: CaptureError) -> Result<()> {
    match error {
        error @ CaptureError::CaptureClosed => Err(capture_error_with_context(
            error,
            "抓包数据流意外关闭 / Packet capture stream closed unexpectedly",
        )),
        error => {
            let error = capture_error_with_context(
                error,
                "接收抓包数据失败 / Failed to receive captured packet",
            );
            log_error!(
                "接收数据包时出现问题，将继续重试。完整错误详情: {:#}",
                "A problem occurred while receiving a packet; capture will retry. Full error details: {:#}",
                error
            );
            Ok(())
        },
    }
}

/// Load dispatch keys from external file first, then merge with embedded keys.
///
/// External keys (in `keys/gi.json` next to the exe) override embedded ones for
/// the same version, allowing key updates without recompiling.
fn load_keys() -> Result<HashMap<u16, Vec<u8>>> {
    let mut all_keys = HashMap::new();

    // 1. Embedded keys (compile-time fallback)
    let embedded: HashMap<u16, String> =
        serde_json::from_slice(include_bytes!("../../keys/gi.json"))?;
    for (version, b64) in &embedded {
        let key = BASE64_STANDARD
            .decode(b64)
            .context("embedded dispatch key is not valid base64")?;
        anyhow::ensure!(
            key.len() == 4096,
            "invalid embedded dispatch key length; version={version}; length={}",
            key.len()
        );
        all_keys.insert(*version, key);
    }

    // 2. External key file next to the exe (overrides embedded for same version)
    let external_path = crate::cli::exe_dir().join("keys").join("gi.json");
    match std::fs::read_to_string(&external_path) {
        Ok(content) => match serde_json::from_str::<HashMap<u16, String>>(&content) {
            Ok(external) => {
                let mut added = 0usize;
                for (version, b64) in &external {
                    match BASE64_STANDARD.decode(b64) {
                        Ok(decoded) if decoded.len() == 4096 => {
                            if !all_keys.contains_key(version) {
                                added += 1;
                            }
                            all_keys.insert(*version, decoded);
                        },
                        Ok(decoded) => log_warn!("忽略长度错误的外部密钥：版本 {}，长度 {}", "Ignoring invalid external key: version {}, length {}", version, decoded.len()),
                        Err(error) => log_warn!(
                            "外部密钥文件中的版本 {} 无法解码；该版本将使用内置密钥（如有）。完整错误详情: {:#}",
                            "Version {} in the external key file could not be decoded; the embedded key will be used for that version when available. Full error details: {:#}",
                            version,
                            error,
                        ),
                    }
                }
                log_info!(
                    "已加载外部密钥文件（{} 个密钥，{} 个新增）",
                    "Loaded external key file ({} keys, {} new)",
                    external.len(),
                    added,
                );
            },
            Err(error) => log_warn!(
                "外部密钥文件格式错误；将改用内置密钥。完整错误详情: {:#}",
                "The external key file has an invalid format; embedded keys will be used instead. Full error details: {:#}",
                error,
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => log_warn!(
            "无法读取外部密钥文件；将改用内置密钥。完整错误详情: {:#}",
            "The external key file could not be read; embedded keys will be used instead. Full error details: {:#}",
            error,
        ),
    }

    anyhow::ensure!(!all_keys.is_empty(), "no dispatch keys available");
    log_info!(
        "已加载 {} 个登录密钥",
        "Loaded {} dispatch keys",
        all_keys.len()
    );
    Ok(all_keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_distinguish_listener_traffic_decryption_and_missing_data() {
        let mut state = CaptureState::default();
        assert!(state.timeout_hint().1.contains("listener"));
        state.listener_ready = true;
        assert!(state.timeout_hint().1.contains("no game traffic"));
        state.packet_count = 8;
        assert!(state.timeout_hint().1.contains("no game messages decoded"));
        state.command_count = 3;
        assert!(state.timeout_hint().1.contains("login handshake"));
        state.has_key_exchange = true;
        assert!(state.timeout_hint().1.contains("UID has not been verified"));
        state.uid = Some(800_000_001);
        assert!(state
            .timeout_hint()
            .1
            .contains("selected data is incomplete"));
        assert!(state
            .diagnostics()
            .contains("packets=8; decryptedCommands=3"));
    }

    #[test]
    fn unrelated_frames_are_counted_without_claiming_decryption() {
        let mut monitor = test_monitor(CaptureState::default());
        monitor.handle_packet(vec![0; 14]);
        let state = monitor.state.lock().unwrap();
        assert_eq!(state.packet_count, 1);
        assert_eq!(state.command_count, 0);
        assert!(state.last_transport_error.is_some());
    }

    #[test]
    fn selected_targets_determine_completion() {
        let state = CaptureState {
            capturing: true,
            uid: Some(800_000_001),
            has_achievements: true,
            ..CaptureState::default()
        };
        let mut targets = CaptureExportSettings {
            include_characters: false,
            include_weapons: false,
            include_artifacts: false,
            ..CaptureExportSettings::default()
        };
        assert!(state.selected_complete(&targets));
        targets.include_artifacts = true;
        assert!(!state.selected_complete(&targets));
        let weapons_only = CaptureState {
            capturing: true,
            uid: Some(800_000_001),
            weapon_count: 5,
            ..CaptureState::default()
        };
        targets.include_artifacts = false;
        targets.include_achievements = false;
        targets.include_weapons = true;
        assert!(weapons_only.selected_complete(&targets));
        assert!(!CaptureState {
            uid: None,
            ..weapons_only
        }
        .selected_complete(&targets));
    }

    const ITEMS_BIN: &[u8] = include_bytes!("testdata/items.bin");
    const AVATARS_BIN: &[u8] = include_bytes!("testdata/avatars.bin");
    const NOISE_BIN: &[u8] = include_bytes!("testdata/noise.bin");

    fn test_monitor(state: CaptureState) -> CaptureMonitor {
        let data_cache = serde_json::from_str(r#"{"version":1,"git_hash":"test"}"#).unwrap();
        let (packet_tx, packet_rx) = mpsc::unbounded_channel();
        CaptureMonitor {
            player_data: PlayerData::new(data_cache),
            sniffer: GameSniffer::new(),
            state: Arc::new(Mutex::new(state)),
            capture_cancel_token: None,
            capture_task: None,
            packet_tx,
            packet_rx,
            packet_dump: None,
            targets: CaptureExportSettings::default(),
            continuous: None,
        }
    }

    #[test]
    fn failed_decode_still_dumps_raw_frame_and_error() {
        let root = std::env::temp_dir().join(format!("gi-failed-decode-{}", uuid::Uuid::new_v4()));
        let mut monitor = test_monitor(CaptureState::default());
        let dump = PacketDump::new(&root).unwrap();
        let session = dump.directory().to_owned();
        monitor.packet_dump = Some(dump);
        let frame = vec![0; 14];
        monitor.handle_packet(frame.clone());

        let pcap = std::fs::read(session.join("packets.pcap")).unwrap();
        assert_eq!(&pcap[40..], frame);
        let record: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(session.join("diagnostics.jsonl")).unwrap(),
        )
        .unwrap();
        assert_eq!(record["rawPacket"], 1);
        assert_eq!(record["commands"], 0);
        assert_eq!(record["state"]["packet_count"], 1);
        assert_eq!(record["state"]["command_count"], 0);
        assert!(record["state"]["last_transport_error"]
            .as_str()
            .unwrap()
            .contains("UDP"));
        assert!(!session.join("000000_cmd0.bin").exists());
        drop(monitor);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn completed_capture_exports_and_exits_with_command_sender_still_open() {
        let mut monitor = test_monitor(CaptureState {
            capturing: true,
            complete: true,
            uid: Some(800_000_001),
            has_characters: true,
            has_items: true,
            has_achievements: true,
            ..CaptureState::default()
        });
        let root = std::env::temp_dir().join(format!("gi-completed-dump-{}", uuid::Uuid::new_v4()));
        let dump = PacketDump::new(&root).unwrap();
        let session = dump.directory().to_owned();
        monitor.packet_dump = Some(dump);
        monitor.player_data.set_uid(Some(800_000_001));
        monitor.player_data.process_achievements(&[80001]);
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let cleanup_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cleanup_done = cleanup_done.clone();
        monitor.capture_cancel_token = Some(cancel);
        monitor.capture_task = Some(tokio::spawn(async move {
            worker_cancel.cancelled().await;
            tokio::task::yield_now().await;
            worker_cleanup_done.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        }));
        let (sender, receiver) = mpsc::unbounded_channel();
        let export = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            monitor.run_capture(receiver, &CaptureExportSettings::default()),
        )
        .await
        .expect("completion must not wait for UI commands")
        .unwrap()
        .expect("completed capture must return its export");
        assert_eq!(export.achievements, Some(vec![80001]));
        assert_eq!(export.metadata.unwrap().uid.as_deref(), Some("800000001"));
        let record: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(session.join("diagnostics.jsonl")).unwrap(),
        )
        .unwrap();
        assert_eq!(record["event"], "end");
        assert_eq!(record["state"]["complete"], true);
        assert_eq!(record["state"]["capturing"], false);
        std::fs::remove_dir_all(root).unwrap();
        assert!(cleanup_done.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            sender.is_closed(),
            "monitor must release the command receiver"
        );
    }

    #[tokio::test]
    async fn stop_and_channel_close_join_the_packet_worker_without_exporting() {
        for stop_command in [true, false] {
            let mut monitor = test_monitor(CaptureState {
                capturing: true,
                ..CaptureState::default()
            });
            let shared = monitor.state.clone();
            let cancel = CancellationToken::new();
            let worker_cancel = cancel.clone();
            let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
            monitor.capture_cancel_token = Some(cancel);
            monitor.capture_task = Some(tokio::spawn(async move {
                worker_cancel.cancelled().await;
                let _ = cleanup_tx.send(());
                Ok(())
            }));
            let (sender, receiver) = mpsc::unbounded_channel();
            if stop_command {
                sender.send(CaptureCommand::StopCapture).unwrap();
            } else {
                drop(sender);
            }
            let export = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                monitor.run_capture(receiver, &CaptureExportSettings::default()),
            )
            .await
            .expect("stop must finish without another UI action")
            .unwrap();
            assert!(export.is_none());
            cleanup_rx.await.expect("packet worker must finish cleanup");
            let state = shared.lock().unwrap();
            assert!(!state.capturing);
            assert!(state.error.is_none());
        }
    }

    #[tokio::test]
    async fn stop_during_initialization_does_not_start_packet_capture() {
        for stop_command in [true, false] {
            let monitor = test_monitor(CaptureState::default());
            let shared = monitor.state.clone();
            let (sender, receiver) = mpsc::unbounded_channel();
            if stop_command {
                sender.send(CaptureCommand::StopCapture).unwrap();
            } else {
                drop(sender);
            }
            assert!(monitor
                .run(receiver, CaptureExportSettings::default())
                .await
                .unwrap()
                .is_none());
            assert!(!shared.lock().unwrap().capturing);
        }
    }

    #[tokio::test]
    async fn packet_failure_exits_without_waiting_for_ui_and_preserves_details() {
        let mut monitor = test_monitor(CaptureState {
            capturing: true,
            ..CaptureState::default()
        });
        monitor.capture_task = Some(tokio::spawn(async {
            Err(anyhow!("pktmon diagnostic").context("capture failed"))
        }));
        let (sender, receiver) = mpsc::unbounded_channel();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            monitor.run_capture(receiver, &CaptureExportSettings::default()),
        )
        .await
        .expect("capture failure must terminate the monitor")
        .unwrap_err();
        assert_eq!(error.to_string(), "capture failed: pktmon diagnostic");
        assert!(sender.is_closed());
    }

    #[test]
    fn capture_creation_error_preserves_nested_source_chain() {
        let source = anyhow!("windows error 5").context("pktmon session start failed");
        let error = capture_error_with_context(
            CaptureError::Capture {
                has_captured: false,
                error: source,
            },
            "创建抓包失败 / Error creating packet capture",
        );

        assert_eq!(
            format!("{error:#}"),
            "创建抓包失败 / Error creating packet capture: \
             pktmon capture failed (has_captured = false): \
             pktmon session start failed: windows error 5"
        );
    }

    #[test]
    fn missing_pktmon_module_surfaces_recovery_hint_and_original_error() {
        let state = Arc::new(Mutex::new(CaptureState {
            capturing: true,
            ..CaptureState::default()
        }));
        let source = yas::utils::packet_capture::initialization_error(
            std::io::Error::from_raw_os_error(-2147024770),
        );
        let error = capture_error_with_context(
            CaptureError::Capture {
                has_captured: false,
                error: source,
            },
            "创建抓包失败 / Error creating packet capture",
        );
        update_state_from_capture_task(&state, Ok(Err(error)), false);

        let state = state.lock().unwrap();
        assert!(!state.capturing);
        let message = state.error.as_deref().unwrap();
        assert!(message.contains("PktMonApi.dll"));
        assert!(message.contains("DISM.exe /Online /Cleanup-Image /RestoreHealth"));
        assert!(message.contains("os error -2147024770"));
    }

    #[test]
    fn closed_capture_stream_is_terminal_and_searchable() {
        let error = handle_capture_read_error(CaptureError::CaptureClosed)
            .expect_err("a closed capture stream must terminate the capture task");

        assert_eq!(
            format!("{error:#}"),
            "抓包数据流意外关闭 / Packet capture stream closed unexpectedly: \
             packet capture stream closed"
        );
    }

    #[tokio::test]
    async fn capture_task_error_updates_state_with_full_chain() {
        let state = Arc::new(Mutex::new(CaptureState {
            capturing: true,
            ..CaptureState::default()
        }));
        let task = tokio::spawn(async {
            Err::<(), _>(anyhow!("pktmon diagnostic").context("packet capture creation failed"))
        });

        update_state_from_capture_task(&state, task.await, false);

        let state = state.lock().unwrap();
        assert!(!state.capturing);
        assert_eq!(
            state.error.as_deref(),
            Some("packet capture creation failed: pktmon diagnostic")
        );
    }

    #[tokio::test]
    async fn capture_task_panic_updates_state_with_payload() {
        let state = Arc::new(Mutex::new(CaptureState {
            capturing: true,
            ..CaptureState::default()
        }));
        let task = tokio::spawn(async {
            panic!("pktmon access violation 0xc0000005");
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        update_state_from_capture_task(&state, task.await, false);

        let state = state.lock().unwrap();
        assert!(!state.capturing);
        assert_eq!(
            state.error.as_deref(),
            Some("capture task panicked: pktmon access violation 0xc0000005")
        );
    }

    #[tokio::test]
    async fn expected_capture_task_stop_does_not_set_an_error() {
        let state = Arc::new(Mutex::new(CaptureState {
            capturing: true,
            ..CaptureState::default()
        }));
        let task = tokio::spawn(async { Ok::<(), anyhow::Error>(()) });

        update_state_from_capture_task(&state, task.await, true);

        let state = state.lock().unwrap();
        assert!(!state.capturing);
        assert!(state.error.is_none());
    }

    // --- try_match_items ---

    #[test]
    fn match_items_on_real_packet() {
        let items = try_match_items(ITEMS_BIN).expect("should match item packet");
        // Real account has thousands of items, hundreds of weapons/artifacts
        assert!(
            items.len() > 1000,
            "expected >1000 items, got {}",
            items.len()
        );
        let weapons = items
            .iter()
            .filter(|i| i.has_equip() && i.equip().has_weapon())
            .count();
        let artifacts = items
            .iter()
            .filter(|i| i.has_equip() && i.equip().has_reliquary())
            .count();
        assert!(weapons > 50, "expected >50 weapons, got {}", weapons);
        assert!(artifacts > 50, "expected >50 artifacts, got {}", artifacts);
    }

    #[test]
    fn match_items_inside_prefixed_unknown_wrapper() {
        let (_, items) = find_best_field::<Item>(ITEMS_BIN, 10, |i| i.item_id != 0 && i.guid != 0)
            .expect("fixture should contain items");

        let mut opaque = vec![0xff, 0x00, 0x7f, 0x80, 0x00];
        for item in items.into_iter().filter(|i| i.has_equip()).take(12) {
            let bytes = item.write_to_bytes().expect("serialize item");
            // Deliberately use an arbitrary two-byte field key. The matcher
            // must not depend on the historical field-2 tag (0x12).
            push_varint((37u64 << 3) | 2, &mut opaque);
            push_varint(bytes.len() as u64, &mut opaque);
            opaque.extend_from_slice(&bytes);
        }

        let mut wrapped = vec![0x91, 0x92, 0x93];
        push_varint((91u64 << 3) | 2, &mut wrapped);
        push_varint(opaque.len() as u64, &mut wrapped);
        wrapped.extend_from_slice(&opaque);

        let matched = try_match_items(&wrapped).expect("should scan nested item records");
        assert!(matched.len() >= MIN_GEAR_COUNT);
        assert!(matched.iter().all(|item| item.has_equip()));
    }

    fn push_varint(mut value: u64, output: &mut Vec<u8>) {
        while value >= 0x80 {
            output.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        output.push(value as u8);
    }

    fn push_varint_field(field: u32, value: u64, output: &mut Vec<u8>) {
        push_varint(u64::from(field) << 3, output);
        push_varint(value, output);
    }

    fn push_message_field(field: u32, message: &[u8], output: &mut Vec<u8>) {
        push_varint((u64::from(field) << 3) | 2, output);
        push_varint(message.len() as u64, output);
        output.extend_from_slice(message);
    }

    fn achievement_packet(
        outer_tag: u32,
        id_tag: u32,
        timestamp_tag: u32,
        records: &[(u32, Option<u64>)],
    ) -> Vec<u8> {
        let mut packet = Vec::new();
        for (index, &(id, timestamp)) in records.iter().enumerate() {
            let mut record = Vec::new();
            // These arbitrary status/progress tags make the fixture resemble
            // AchievementInfo without teaching the matcher fixed positions.
            push_varint_field(2, if timestamp.is_some() { 2 } else { 1 }, &mut record);
            push_varint_field(id_tag, u64::from(id), &mut record);
            push_varint_field(19, index as u64, &mut record);
            if let Some(timestamp) = timestamp {
                push_varint_field(timestamp_tag, timestamp, &mut record);
            }
            push_message_field(outer_tag, &record, &mut packet);
        }
        packet
    }

    #[test]
    fn match_achievements_infers_obfuscated_fields() {
        let packet = achievement_packet(
            47,
            11,
            29,
            &[
                (87007, Some(1_725_000_000)),
                (82003, None),
                (80001, Some(1_600_000_000)),
                (84512, None),
                (86101, Some(1_700_000_000)),
                (83009, None),
            ],
        );

        assert_eq!(
            try_match_achievements(&packet),
            Some(vec![80001, 86101, 87007])
        );
    }

    #[test]
    fn match_achievements_survives_another_tag_layout_without_sentinel_id() {
        let packet = achievement_packet(
            5,
            73,
            4,
            &[
                (81001, Some(1_650_000_000)),
                (81002, Some(1_650_000_001)),
                (81003, None),
                (81004, None),
                (81005, Some(1_650_000_002)),
            ],
        );

        assert_eq!(
            try_match_achievements(&packet),
            Some(vec![81001, 81002, 81005])
        );
    }

    #[test]
    fn match_achievements_rejects_duplicate_id_records() {
        let packet = achievement_packet(
            5,
            7,
            9,
            &[
                (80001, Some(1_650_000_000)),
                (80001, Some(1_650_000_001)),
                (80001, Some(1_650_000_002)),
                (80001, Some(1_650_000_003)),
                (80001, Some(1_650_000_004)),
            ],
        );

        assert!(try_match_achievements(&packet).is_none());
        assert!(try_match_achievements(NOISE_BIN).is_none());
    }

    #[test]
    fn match_items_rejects_avatar_packet() {
        assert!(
            try_match_items(AVATARS_BIN).is_none(),
            "avatar packet should not match as items"
        );
    }

    #[test]
    fn match_items_rejects_noise() {
        assert!(try_match_items(NOISE_BIN).is_none());
    }

    #[test]
    fn match_items_rejects_empty() {
        assert!(try_match_items(&[]).is_none());
    }

    // --- try_match_avatars ---

    #[test]
    fn match_avatars_on_real_packet() {
        let avatars = try_match_avatars(AVATARS_BIN).expect("should match avatar packet");
        assert!(
            avatars.len() > 20,
            "expected >20 avatars, got {}",
            avatars.len()
        );
        // All real avatars should have prop_map with level
        let with_level = avatars
            .iter()
            .filter(|a| a.prop_map.contains_key(&4001))
            .count();
        assert!(
            with_level > 10,
            "expected >10 avatars with level prop, got {}",
            with_level
        );
    }

    #[test]
    fn match_avatars_rejects_item_packet() {
        assert!(
            try_match_avatars(ITEMS_BIN).is_none(),
            "item packet should not match as avatars"
        );
    }

    #[test]
    fn match_avatars_rejects_noise() {
        assert!(try_match_avatars(NOISE_BIN).is_none());
    }

    #[test]
    fn match_avatars_rejects_empty() {
        assert!(try_match_avatars(&[]).is_none());
    }

    // --- find_best_field ---

    #[test]
    fn find_best_field_returns_correct_field_number() {
        // Items are in field 3 for this game version
        let (field, items) =
            find_best_field::<Item>(ITEMS_BIN, 10, |i| i.item_id != 0 && i.guid != 0)
                .expect("should find items");
        assert_eq!(field, 3, "items should be in field 3");
        assert!(items.len() > 1000);

        // Avatars are in field 10 for this game version
        let (field, avatars) =
            find_best_field::<AvatarInfo>(AVATARS_BIN, 4, |a| a.avatar_id != 0 && a.guid != 0)
                .expect("should find avatars");
        assert_eq!(field, 10, "avatars should be in field 10");
        assert!(avatars.len() > 20);
    }

    #[test]
    fn find_best_field_respects_min_entries() {
        // With an impossibly high threshold, nothing should match
        assert!(find_best_field::<Item>(ITEMS_BIN, 100_000, |_| true).is_none());
    }

    // --- Cross-contamination: items shouldn't parse as avatars and vice versa ---

    #[test]
    fn item_data_does_not_produce_valid_avatars() {
        // Even if we lower thresholds, item blobs shouldn't parse as AvatarInfo
        // with meaningful data
        let result =
            find_best_field::<AvatarInfo>(ITEMS_BIN, 4, |a| a.avatar_id != 0 && a.guid != 0);
        // Either None, or if protobuf happens to parse garbage, the prop_map check
        // in try_match_avatars would reject it
        if let Some((_, avatars)) = result {
            let with_props = avatars.iter().filter(|a| !a.prop_map.is_empty()).count();
            assert!(
                with_props < MIN_AVATARS_WITH_PROPS,
                "item data shouldn't produce avatars with valid props"
            );
        }
    }

    fn identity_test_monitor() -> (CaptureMonitor, crate::continuous::ContinuousCapture) {
        use crate::game_data::types::{Artifact, ArtifactSlot, DataCache, Property, Weapon};

        let mut cache: DataCache =
            serde_json::from_str(r#"{"version":1,"git_hash":"synthetic-login"}"#).unwrap();
        cache.property_map.insert(1, Property::Hp);
        cache.artifact_map.insert(
            101,
            Artifact {
                set: "Gladiator's Finale".into(),
                slot: ArtifactSlot::Flower,
                rarity: 5,
            },
        );
        cache.weapon_map.insert(
            201,
            Weapon {
                name: "Test Sword".into(),
                rarity: 5,
            },
        );
        for index in 1..=4 {
            cache
                .character_map
                .insert(10_000_000 + index, format!("Test Character {index}"));
        }
        let mut monitor = test_monitor(CaptureState {
            capturing: true,
            ..CaptureState::default()
        });
        monitor.player_data = PlayerData::new(cache);
        monitor.sniffer = GameSniffer::new().set_initial_keys(HashMap::from([(0, vec![0; 4096])]));
        let latest = crate::continuous::ContinuousCapture::default();
        monitor.continuous = Some(latest.clone());
        (monitor, latest)
    }

    fn identity_udp_frame(payload: &[u8], incoming: bool) -> Vec<u8> {
        // Complete Ethernet/IPv4/UDP framing, consumed by the production parser.
        let mut frame = vec![0; 14 + 20 + 8];
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&((20 + 8 + payload.len()) as u16).to_be_bytes());
        frame[22] = 64;
        frame[23] = 17;
        frame[26..30].copy_from_slice(&[127, 0, 0, 1]);
        frame[30..34].copy_from_slice(&[127, 0, 0, 1]);
        let (source, destination): (u16, u16) = if incoming {
            (22101, 50000)
        } else {
            (50000, 22101)
        };
        frame[34..36].copy_from_slice(&source.to_be_bytes());
        frame[36..38].copy_from_slice(&destination.to_be_bytes());
        frame[38..40].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn identity_control_frame(code: u32, conv: Option<u32>, incoming: bool) -> Vec<u8> {
        let mut payload = code.to_be_bytes().to_vec();
        if let Some(conv) = conv {
            payload.extend_from_slice(&conv.to_be_bytes());
        }
        identity_udp_frame(&payload, incoming)
    }

    fn identity_command_frame(body: &[u8]) -> Vec<u8> {
        let mut command = 0x4567u16.to_be_bytes().to_vec();
        command.extend_from_slice(&65500u16.to_be_bytes()); // Rotating ID is irrelevant.
        command.extend_from_slice(&0u16.to_be_bytes());
        command.extend_from_slice(&(body.len() as u32).to_be_bytes());
        command.extend_from_slice(body);
        command.extend_from_slice(&0x89ABu16.to_be_bytes());
        command
    }

    fn identity_kcp_frame(conv: u32, sequence: u32, command: &[u8], incoming: bool) -> Vec<u8> {
        // A zero dispatch key leaves the fictional command bytes unchanged.
        let mut segment = conv.to_le_bytes().to_vec();
        segment.extend_from_slice(&[0; 4]); // Game KCP token.
        segment.extend_from_slice(&[81, 0, 0, 4]); // PUSH, no fragments, window 1024.
        segment.extend_from_slice(&0u32.to_le_bytes());
        segment.extend_from_slice(&sequence.to_le_bytes());
        segment.extend_from_slice(&0u32.to_le_bytes());
        segment.extend_from_slice(&(command.len() as u32).to_le_bytes());
        segment.extend_from_slice(&[0; 4]); // Game KCP header extension.
        segment.extend_from_slice(command);
        identity_udp_frame(&segment, incoming)
    }

    fn identity_token(uid: u32) -> Vec<u8> {
        let mut command = GameCommand::try_new(
            include_bytes!("testdata/token7_2.bin").to_vec(),
            PacketDirection::Received,
        )
        .unwrap();
        let mut body = Unk::parse_from_bytes(&command.proto_data).unwrap();
        body.mut_unknown_fields().remove(9);
        body.mut_unknown_fields().add_varint(9, u64::from(uid));
        command.proto_data = body.write_to_bytes().unwrap();
        command.to_bytes()
    }

    fn identity_inventory(guid_offset: u64) -> (Vec<u8>, Vec<u8>) {
        use auto_artifactarium::r#gen::protos::{Equip, PropValue, Reliquary, Weapon};

        let mut items = Unk::new();
        for index in 1..=6 {
            let mut weapon = Weapon::new();
            weapon.level = 20;
            let mut equip = Equip::new();
            equip.set_weapon(weapon);
            let mut item = Item::new();
            item.item_id = 201;
            item.guid = guid_offset + index;
            item.set_equip(equip);
            items
                .mut_unknown_fields()
                .add_length_delimited(3, item.write_to_bytes().unwrap());

            let mut artifact = Reliquary::new();
            artifact.level = 1;
            artifact.main_prop_id = 1;
            let mut equip = Equip::new();
            equip.set_reliquary(artifact);
            let mut item = Item::new();
            item.item_id = 101;
            item.guid = guid_offset + 10 + index;
            item.set_equip(equip);
            items
                .mut_unknown_fields()
                .add_length_delimited(3, item.write_to_bytes().unwrap());
        }
        let mut avatars = Unk::new();
        for index in 1..=4 {
            let mut avatar = AvatarInfo::new();
            avatar.avatar_id = 10_000_000 + index;
            avatar.guid = guid_offset + 100 + u64::from(index);
            avatar.avatar_type = 1;
            let mut level = PropValue::new();
            level.val = 20;
            avatar.prop_map.insert(4001, level);
            avatar.prop_map.insert(1002, PropValue::new());
            avatars
                .mut_unknown_fields()
                .add_length_delimited(10, avatar.write_to_bytes().unwrap());
        }
        (
            identity_command_frame(&items.write_to_bytes().unwrap()),
            identity_command_frame(&avatars.write_to_bytes().unwrap()),
        )
    }

    fn identity_begin_login(monitor: &mut CaptureMonitor, conv: u32, uid: u32) {
        monitor.process_packet(identity_control_frame(0xff, None, false));
        monitor.process_packet(identity_control_frame(0x145, Some(conv), true));
        monitor.process_packet(identity_kcp_frame(conv, 0, &identity_token(uid), true));
    }

    fn identity_publish_inventory(monitor: &mut CaptureMonitor, conv: u32, guid_offset: u64) {
        let (items, avatars) = identity_inventory(guid_offset);
        monitor.process_packet(identity_kcp_frame(conv, 1, &items, true));
        monitor.process_packet(identity_kcp_frame(conv, 2, &avatars, true));
    }

    fn inventory_change_packet(item: &Item) -> Vec<u8> {
        let mut packet = Unk::new();
        packet
            .mut_unknown_fields()
            .add_length_delimited(12, item.write_to_bytes().unwrap());
        // Enhancement deltas may also carry consumed material counts, including
        // virtual materials without an instance GUID. Gear still updates.
        let mut material = Item::new();
        material.item_id = 104_013;
        material.mut_material().count = 0;
        packet
            .mut_unknown_fields()
            .add_length_delimited(12, material.write_to_bytes().unwrap());
        packet.mut_unknown_fields().add_varint(1, 1);
        identity_command_frame(&packet.write_to_bytes().unwrap())
    }

    fn inventory_delete_packet(guids: &[u64]) -> Vec<u8> {
        let mut packed = Vec::new();
        for guid in guids {
            push_varint(*guid, &mut packed);
        }
        let mut body = Vec::new();
        push_message_field(4, &packed, &mut body);
        push_varint_field(15, 1, &mut body);
        identity_command_frame(&body)
    }

    #[test]
    fn continuous_upgrade_and_consumption_replace_then_remove_inventory_including_last_item() {
        let uid = 800_000_001;
        let base = u64::from(uid) << 32;
        let (mut monitor, latest) = identity_test_monitor();
        identity_begin_login(&mut monitor, 41, uid);
        let (items, _) = identity_inventory(base);
        let command = GameCommand::try_new(items, PacketDirection::Received).unwrap();
        let mut artifact = try_match_items(&command.proto_data)
            .unwrap()
            .into_iter()
            .find(|item| item.guid == base + 11)
            .unwrap();
        monitor.process_commands(vec![GameCommand::try_new(
            inventory_change_packet(&artifact),
            PacketDirection::Received,
        )
        .unwrap()]);
        assert!(!monitor.state.lock().unwrap().has_items);
        assert_eq!(monitor.player_data.artifact_count(), 0);
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).0,
            503
        );
        identity_publish_inventory(&mut monitor, 41, base);
        artifact.mut_equip().mut_reliquary().level = 5;
        artifact.mut_equip().is_locked = true;
        monitor.process_packet(identity_kcp_frame(
            41,
            3,
            &inventory_change_packet(&artifact),
            true,
        ));
        let upgraded = crate::continuous::ContinuousCapture::response(Some(&latest)).1;
        assert_eq!(upgraded["data"]["artifacts"].as_array().unwrap().len(), 6);
        assert_eq!(upgraded["data"]["artifacts"][0]["level"], 4);
        assert_eq!(upgraded["data"]["artifacts"][0]["lock"], true);

        monitor.process_packet(identity_kcp_frame(
            41,
            4,
            &inventory_delete_packet(&[artifact.guid]),
            true,
        ));
        let deleted = crate::continuous::ContinuousCapture::response(Some(&latest)).1;
        assert_eq!(deleted["data"]["artifacts"].as_array().unwrap().len(), 5);
        assert!(deleted["revision"].as_u64().unwrap() > upgraded["revision"].as_u64().unwrap());
        let remaining: Vec<_> = (1..=6)
            .chain(12..=16)
            .map(|counter| base + counter)
            .collect();
        monitor.process_packet(identity_kcp_frame(
            41,
            5,
            &inventory_delete_packet(&remaining),
            true,
        ));
        let (status, empty) = crate::continuous::ContinuousCapture::response(Some(&latest));
        assert_eq!(status, 200);
        assert!(empty["data"]["artifacts"].as_array().unwrap().is_empty());
        assert!(empty["data"]["weapons"].as_array().unwrap().is_empty());
        assert_eq!(monitor.state.lock().unwrap().artifact_count, 0);
        assert!(monitor.state.lock().unwrap().has_items);

        // Repeated/unknown deletions do not change content or its revision.
        monitor.process_packet(identity_kcp_frame(
            41,
            6,
            &inventory_delete_packet(&[artifact.guid, base + 999]),
            true,
        ));
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).1,
            empty
        );
        // A new artifact still arrives after consuming the final old item.
        artifact.guid = base + 1000;
        monitor.process_packet(identity_kcp_frame(
            41,
            7,
            &inventory_change_packet(&artifact),
            true,
        ));
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).1["data"]["artifacts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn continuous_deletes_ignore_outgoing_and_other_account_guids() {
        let uid = 800_000_001;
        let base = u64::from(uid) << 32;
        let (mut monitor, latest) = identity_test_monitor();
        identity_begin_login(&mut monitor, 41, uid);
        identity_publish_inventory(&mut monitor, 41, base);
        let original = crate::continuous::ContinuousCapture::response(Some(&latest)).1;
        monitor.process_packet(identity_kcp_frame(
            41,
            0,
            &inventory_delete_packet(&[base + 11]),
            false,
        ));
        monitor.process_packet(identity_kcp_frame(
            41,
            3,
            &inventory_delete_packet(&[((u64::from(uid) + 1) << 32) + 11]),
            true,
        ));
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).1,
            original
        );
    }

    #[test]
    fn continuous_uid_comes_from_incoming_login_before_inventory_is_published() {
        let (mut monitor, latest) = identity_test_monitor();
        let shared = monitor.state.clone();
        let (items, avatars) = identity_inventory(1000);
        monitor.process_packet(identity_kcp_frame(11, 0, &items, true));
        monitor.process_packet(identity_kcp_frame(11, 1, &avatars, true));
        assert_eq!(monitor.player_data.uid(), None);
        assert_eq!(shared.lock().unwrap().uid, None);
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).0,
            503
        );

        // Outgoing bytes with the exact token shape cannot establish account identity.
        monitor.process_packet(identity_kcp_frame(
            11,
            0,
            &identity_token(800_000_002),
            false,
        ));
        assert_eq!(monitor.player_data.uid(), None);
        assert!(!monitor.sniffer.key_exchange_captured());
        monitor.process_packet(identity_kcp_frame(
            11,
            2,
            &identity_token(800_000_001),
            true,
        ));
        assert_eq!(monitor.player_data.uid(), Some(800_000_001));
        assert_eq!(shared.lock().unwrap().uid, Some(800_000_001));
        assert!(monitor.sniffer.key_exchange_captured());
        assert_eq!(monitor.player_data.character_count(), 0);
        assert_eq!(monitor.player_data.weapon_count(), 0);
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).0,
            503
        );

        monitor.process_packet(identity_kcp_frame(11, 3, &items, true));
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).0,
            503
        );
        monitor.process_packet(identity_kcp_frame(11, 4, &avatars, true));
        let (status, snapshot) = crate::continuous::ContinuousCapture::response(Some(&latest));
        assert_eq!(status, 200);
        assert_eq!(snapshot["data"]["metadata"]["uid"], "800000001");
        assert_eq!(snapshot["data"]["characters"].as_array().unwrap().len(), 4);
        assert_eq!(snapshot["data"]["weapons"].as_array().unwrap().len(), 6);
        assert_eq!(snapshot["data"]["artifacts"].as_array().unwrap().len(), 6);
        let normal = monitor
            .player_data
            .export(&CaptureExportSettings::default())
            .unwrap();
        assert_eq!(normal.metadata.unwrap().uid.as_deref(), Some("800000001"));
    }

    #[tokio::test]
    async fn one_time_packet_capture_exports_the_verified_login_uid() {
        let (mut monitor, _) = identity_test_monitor();
        monitor.continuous = None;
        monitor.targets.include_achievements = false;
        identity_begin_login(&mut monitor, 11, 800_000_001);
        assert!(!monitor.state.lock().unwrap().complete);
        identity_publish_inventory(&mut monitor, 11, 1000);
        assert!(monitor.state.lock().unwrap().complete);
        let (_sender, receiver) = mpsc::unbounded_channel();
        let settings = monitor.targets.clone();
        let export = monitor
            .run_capture(receiver, &settings)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(export.metadata.unwrap().uid.as_deref(), Some("800000001"));
        assert_eq!(export.characters.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn one_time_packet_capture_rejects_an_unverified_login_uid() {
        let (mut monitor, _) = identity_test_monitor();
        monitor.continuous = None;
        monitor.targets.include_achievements = false;
        identity_begin_login(&mut monitor, 11, 0);
        identity_publish_inventory(&mut monitor, 11, 1000);
        assert!(!monitor.state.lock().unwrap().complete);
        let (_sender, receiver) = mpsc::unbounded_channel();
        let settings = monitor.targets.clone();
        let error = monitor.run_capture(receiver, &settings).await.unwrap_err();
        assert!(error.to_string().contains("UID could not be verified"));
        assert!(error.to_string().contains("log in again"));
    }

    #[tokio::test]
    async fn decoded_inventory_without_a_login_uid_cannot_complete_one_time_capture() {
        let (mut monitor, _) = identity_test_monitor();
        monitor.continuous = None;
        monitor.targets.include_achievements = false;
        monitor.process_packet(identity_control_frame(0xff, None, false));
        monitor.process_packet(identity_control_frame(0x145, Some(11), true));
        let (items, avatars) = identity_inventory(1000);
        monitor.process_packet(identity_kcp_frame(11, 0, &items, true));
        monitor.process_packet(identity_kcp_frame(11, 1, &avatars, true));
        {
            let state = monitor.state.lock().unwrap();
            assert!(state.has_items && state.has_characters);
            assert!(!state.complete);
            assert_eq!(state.uid, None);
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(CaptureCommand::StopCapture).unwrap();
        let settings = monitor.targets.clone();
        assert!(monitor
            .run_capture(receiver, &settings)
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn continuous_account_switches_and_same_uid_reconnects_reset_identity_and_inventory() {
        let (mut monitor, latest) = identity_test_monitor();
        let shared = monitor.state.clone();
        identity_begin_login(&mut monitor, 11, 800_000_001);
        assert_eq!(shared.lock().unwrap().uid, Some(800_000_001));
        identity_publish_inventory(&mut monitor, 11, 1000);
        let first = crate::continuous::ContinuousCapture::response(Some(&latest)).1;
        assert_eq!(first["data"]["metadata"]["uid"], "800000001");

        monitor.process_packet(identity_control_frame(0xff, None, false));
        assert_eq!(monitor.player_data.uid(), None);
        assert_eq!(shared.lock().unwrap().uid, None);
        assert_eq!(monitor.player_data.character_count(), 0);
        assert_eq!(monitor.player_data.artifact_count(), 0);
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).0,
            503
        );
        // Delayed traffic from the old conversation cannot identify the new login.
        monitor.process_packet(identity_kcp_frame(
            11,
            3,
            &identity_token(800_000_003),
            true,
        ));
        assert_eq!(monitor.player_data.uid(), None);
        monitor.process_packet(identity_control_frame(0x145, Some(22), true));
        monitor.process_packet(identity_kcp_frame(
            22,
            0,
            &identity_token(800_000_002),
            true,
        ));
        let (old_items, _) = identity_inventory(1000);
        monitor.process_packet(identity_kcp_frame(11, 4, &old_items, true));
        assert_eq!(monitor.player_data.uid(), Some(800_000_002));
        assert_eq!(shared.lock().unwrap().uid, Some(800_000_002));
        assert_eq!(monitor.player_data.weapon_count(), 0);
        identity_publish_inventory(&mut monitor, 22, 2000);
        let (status, second) = crate::continuous::ContinuousCapture::response(Some(&latest));
        assert_eq!(status, 200);
        assert_ne!(first["sessionId"], second["sessionId"]);
        assert_eq!(second["data"]["metadata"]["uid"], "800000002");
        assert_eq!(second["data"]["characters"].as_array().unwrap().len(), 4);
        assert_eq!(second["data"]["weapons"].as_array().unwrap().len(), 6);
        assert_eq!(second["data"]["artifacts"].as_array().unwrap().len(), 6);
        monitor.process_packet(identity_control_frame(404, Some(11), true));
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).1,
            second
        );

        identity_begin_login(&mut monitor, 33, 800_000_002);
        assert_eq!(monitor.player_data.uid(), Some(800_000_002));
        assert_eq!(monitor.player_data.weapon_count(), 0);
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).0,
            503
        );
        identity_publish_inventory(&mut monitor, 33, 3000);
        let reconnected = crate::continuous::ContinuousCapture::response(Some(&latest)).1;
        assert_ne!(second["sessionId"], reconnected["sessionId"]);
        assert_eq!(reconnected["revision"], 1);
        assert_eq!(reconnected["data"]["metadata"]["uid"], "800000002");
        assert_eq!(reconnected["data"]["weapons"].as_array().unwrap().len(), 6);

        monitor.process_packet(identity_control_frame(404, Some(33), true));
        assert_eq!(shared.lock().unwrap().uid, None);
        assert_eq!(monitor.player_data.uid(), None);
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).0,
            503
        );
    }

    #[test]
    fn unverified_login_cannot_publish_anonymous_inventory_or_reuse_previous_uid() {
        let (mut monitor, latest) = identity_test_monitor();
        identity_begin_login(&mut monitor, 11, 800_000_001);
        identity_publish_inventory(&mut monitor, 11, 1000);
        assert_eq!(
            crate::continuous::ContinuousCapture::response(Some(&latest)).0,
            200
        );

        identity_begin_login(&mut monitor, 22, 0);
        assert_eq!(monitor.player_data.uid(), None);
        assert_eq!(monitor.state.lock().unwrap().uid, None);
        identity_publish_inventory(&mut monitor, 22, 2000);
        let (status, body) = crate::continuous::ContinuousCapture::response(Some(&latest));
        assert_eq!(status, 503);
        assert_eq!(body["error"], "continuous_capture_failed");
        assert!(body.get("data").is_none());
        assert_eq!(
            monitor
                .player_data
                .export(&CaptureExportSettings::default())
                .unwrap()
                .metadata,
            None
        );
    }
}
