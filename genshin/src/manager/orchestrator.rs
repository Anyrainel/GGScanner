use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use yas::{log_debug, log_info, log_warn};

use yas::cancel::CancelToken;

use crate::scanner::common::game_controller::GenshinGameController;
use crate::scanner::common::mappings::MappingManager;
use crate::scanner::common::models::GoodArtifact;
use crate::scanner::common::ocr_pool::SharedOcrPools;
pub use crate::scanner::common::progress::ProgressFn;

use super::equip_manager::{EquipManager, EquipTarget};
use super::lock_manager::LockManager;
use super::models::*;

/// A single lock/unlock target: the artifact to match + desired lock state.
pub struct LockTarget {
    /// Result ID for this target (e.g., "lock:0" or "unlock:2").
    pub result_id: String,
    /// The artifact identity from the client (used for matching).
    pub artifact: GoodArtifact,
    /// Desired lock state: true = lock, false = unlock.
    pub desired_lock: bool,
}

pub struct ArtifactManager {
    mappings: Arc<MappingManager>,
    pools: Arc<SharedOcrPools>,
    capture_delay: u64,
    delay_scroll: u64,
    panel_timeout: u64,
    initial_wait: u64,
    stop_on_all_matched: bool,
    filter_involved_sets: bool,
    dump_images: bool,
}

impl ArtifactManager {
    pub fn new(
        mappings: Arc<MappingManager>,
        pools: Arc<SharedOcrPools>,
        capture_delay: u64,
        delay_scroll: u64,
        panel_timeout: u64,
        initial_wait: u64,
        stop_on_all_matched: bool,
        filter_involved_sets: bool,
        dump_images: bool,
    ) -> Self {
        Self {
            mappings,
            pools,
            capture_delay,
            delay_scroll,
            panel_timeout,
            initial_wait,
            stop_on_all_matched,
            filter_involved_sets,
            dump_images,
        }
    }

    pub fn mappings(&self) -> &Arc<MappingManager> {
        &self.mappings
    }
    pub fn replace_mappings(&mut self, mappings: Arc<MappingManager>) {
        self.mappings = mappings;
    }
    pub fn pools(&self) -> &Arc<SharedOcrPools> {
        &self.pools
    }
    pub fn dump_images(&self) -> bool {
        self.dump_images
    }

    pub fn execute(
        &self,
        ctrl: &mut GenshinGameController,
        request: LockManageRequest,
        progress_fn: Option<&ProgressFn<'_>>,
        cancel_token: CancelToken,
    ) -> (ManageResult, Option<Vec<GoodArtifact>>) {
        // Build targets — validation is done at the server layer (400 on any invalid entry).
        let mut targets: Vec<LockTarget> = Vec::new();
        for (idx, artifact) in request.lock.iter().enumerate() {
            targets.push(LockTarget {
                result_id: format!("lock:{}", idx),
                artifact: artifact.clone(),
                desired_lock: true,
            });
        }
        for (idx, artifact) in request.unlock.iter().enumerate() {
            targets.push(LockTarget {
                result_id: format!("unlock:{}", idx),
                artifact: artifact.clone(),
                desired_lock: false,
            });
        }

        let mut all_results: Vec<InstructionResult> = Vec::new();
        let total = targets.len();

        ctrl.focus_game_window();
        ctrl.set_cancel_token(cancel_token.clone());

        let report = |completed: usize, phase: &str| {
            if let Some(f) = progress_fn {
                f(completed, total, "", phase);
            }
        };

        report(all_results.len(), "锁定变更 / Lock changes");

        log_info!(
            "[manager] 执行 {} 个锁定目标（{} 锁定, {} 解锁）",
            "[manager] Executing {} lock targets ({} lock, {} unlock)",
            targets.len(),
            targets.iter().filter(|t| t.desired_lock).count(),
            targets.iter().filter(|t| !t.desired_lock).count(),
        );

        let stop_on_all_matched = self.stop_on_all_matched || self.filter_involved_sets;

        let lock_mgr = LockManager::new(self.mappings.clone(), self.pools.clone());
        let (lock_results, scanned_artifacts, matched_indices, scan_complete, ocr_failures) =
            lock_mgr.execute(
                ctrl,
                &targets,
                self.capture_delay,
                self.delay_scroll,
                self.panel_timeout,
                self.initial_wait,
                stop_on_all_matched,
                self.filter_involved_sets,
                self.dump_images,
                progress_fn,
            );

        for r in &lock_results {
            all_results.push(r.clone());
            report(all_results.len(), "锁定变更 / Lock changes");
        }

        // Mark unprocessed targets as aborted/skipped
        let processed_ids: HashSet<String> = all_results.iter().map(|r| r.id.clone()).collect();
        let was_cancelled = cancel_token.is_cancelled();
        for target in &targets {
            if !processed_ids.contains(&target.result_id) {
                all_results.push(if was_cancelled {
                    InstructionResult::failure(
                        target.result_id.clone(),
                        InstructionStatus::Aborted,
                        "用户已停止此锁定操作。",
                        "This lock operation was stopped by the user.",
                        None,
                    )
                } else {
                    InstructionResult::failure(
                        target.result_id.clone(),
                        InstructionStatus::Skipped,
                        "未执行此锁定操作，因为之前的扫描步骤未能完成。",
                        "This lock operation was not performed because an earlier scan step did not finish.",
                        None,
                    )
                });
            }
        }

        let summary = ManageSummary::from_results(&all_results);
        log_info!(
            "[manager] 完成：{} 成功, {} 已正确, {} 未找到, {} 错误, {} 中断",
            "[manager] Done: {} success, {} already correct, {} not found, {} errors, {} aborted",
            summary.success,
            summary.already_correct,
            summary.not_found,
            summary.errors,
            summary.aborted,
        );

        // Only produce a snapshot if the scan was complete AND had no data quality issues.
        // Solver failures (total_rolls == None on leveled artifacts) indicate bad substat data.
        // OCR failures mean artifacts were lost entirely.
        let solver_failures = scanned_artifacts
            .iter()
            .filter(|(_, a)| a.total_rolls.is_none())
            .count();
        let has_data_errors = ocr_failures > 0 || solver_failures > 0;
        if has_data_errors && scan_complete {
            log_warn!(
                "[manager] 扫描数据存在质量问题（OCR失败: {}, 求解失败: {}），不生成快照",
                "[manager] Scan has data quality issues (OCR failures: {}, solver failures: {}), skipping snapshot",
                ocr_failures, solver_failures,
            );
        }

        let artifact_snapshot =
            if scan_complete && !scanned_artifacts.is_empty() && !has_data_errors {
                Some(build_artifact_snapshot(
                    &scanned_artifacts,
                    &targets,
                    &matched_indices,
                    &all_results,
                ))
            } else {
                None
            };

        (
            ManageResult {
                results: all_results,
                summary,
            },
            artifact_snapshot,
        )
    }

    pub fn execute_equip(
        &self,
        ctrl: &mut GenshinGameController,
        request: EquipRequest,
        progress_fn: Option<&ProgressFn<'_>>,
        cancel_token: CancelToken,
    ) -> ManageResult {
        let mut targets: Vec<EquipTarget> = Vec::new();
        for (idx, instr) in request.equip.iter().enumerate() {
            targets.push(EquipTarget {
                result_id: format!("equip:{}", idx),
                artifact: instr.artifact.clone(),
                target_location: instr.location.clone(),
            });
        }

        let total = targets.len();

        ctrl.focus_game_window();
        ctrl.set_cancel_token(cancel_token.clone());

        let report = |completed: usize, phase: &str| {
            if let Some(f) = progress_fn {
                f(completed, total, "", phase);
            }
        };

        report(0, "装备变更 / Equip changes");

        log_debug!(
            "[manager] 执行 {} 个装备目标",
            "[manager] executing {} equip targets",
            targets.len()
        );

        let equip_mgr =
            EquipManager::new(self.mappings.clone(), self.pools.clone(), self.dump_images);
        let results = equip_mgr.execute(ctrl, &targets, progress_fn);

        report(results.len(), "装备变更 / Equip changes");

        let summary = ManageSummary::from_results(&results);
        log_info!(
            "[manager] 装备完成：{} 成功, {} 已正确, {} 未找到, {} 错误, {} 中断",
            "[manager] equip done: {} ok, {} already correct, {} not found, {} errors, {} aborted",
            summary.success,
            summary.already_correct,
            summary.not_found,
            summary.errors,
            summary.aborted,
        );

        ManageResult { results, summary }
    }
}

fn build_artifact_snapshot(
    scanned_artifacts: &[(usize, GoodArtifact)],
    targets: &[LockTarget],
    matched_indices: &HashMap<usize, usize>,
    results: &[InstructionResult],
) -> Vec<GoodArtifact> {
    let result_success: HashSet<String> = results
        .iter()
        .filter(|r| r.status == InstructionStatus::Success)
        .map(|r| r.id.clone())
        .collect();

    // Map scanned artifact index -> desired_lock for successful toggles
    let mut toggled_to: HashMap<usize, bool> = HashMap::new();
    for (target_vec_idx, target) in targets.iter().enumerate() {
        if result_success.contains(&target.result_id) {
            if let Some(&scanned_idx) = matched_indices.get(&target_vec_idx) {
                toggled_to.insert(scanned_idx, target.desired_lock);
            }
        }
    }

    scanned_artifacts
        .iter()
        .map(|(idx, artifact)| {
            if let Some(&desired_lock) = toggled_to.get(idx) {
                let mut updated = artifact.clone();
                updated.lock = desired_lock;
                // Unlocking removes astral mark (game engine forces this)
                if !desired_lock {
                    updated.astral_mark = false;
                }
                updated
            } else {
                artifact.clone()
            }
        })
        .collect()
}
