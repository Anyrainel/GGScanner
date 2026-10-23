use super::{
    state::{Lang, TaskStatus},
    task_progress::{self, Step, StepState},
    theme,
};
use eframe::egui;
use genshin_scanner::manager::models::{JobKind, JobPhase, JobState, PhaseState};
use std::sync::{Arc, Mutex};

pub fn show(
    ui: &mut egui::Ui,
    lang: Lang,
    shared: &Arc<Mutex<JobState>>,
    status: Option<&TaskStatus>,
) {
    let Ok(job) = shared.try_lock() else {
        theme::task_status(
            ui,
            lang,
            status,
            lang.t("等待网页请求", "Waiting for a web request"),
        );
        return;
    };
    if job.state == JobPhase::Idle || !matches!(status, Some(TaskStatus::Running(_))) {
        theme::task_status(ui, lang, status, lang.t("服务器未启动", "Server is off"));
        return;
    }
    let kind = job.ui.kind.as_ref();
    let count = job
        .progress
        .as_ref()
        .or(job.ui.progress.as_ref())
        .map(|p| p.total)
        .unwrap_or(0);
    let mut text = match kind {
        Some(JobKind::Manage { lock, unlock }) => match lang {
            Lang::Zh => format!("{} · 锁定 {lock} / 解锁 {unlock}", "加解锁请求"),
            Lang::En => format!("{} · {lock} lock / {unlock} unlock", "Lock / unlock"),
        },
        Some(JobKind::Equip) => format!("{} · {count}", lang.t("装备请求", "Equip request")),
        Some(JobKind::Scan) => lang.t("扫描请求", "Scan request").to_owned(),
        None => lang
            .t("等待网页请求", "Waiting for a web request")
            .to_owned(),
    };
    let unfinished = job.result.as_ref().map_or(0, |r| {
        r.summary.errors + r.summary.aborted + r.summary.not_found
    });
    let feedback = if job.state == JobPhase::Completed {
        if unfinished > 0 {
            text = format!(
                "{} · {unfinished}",
                lang.t("请求未完成", "Request incomplete")
            );
            TaskStatus::AwaitingInput(super::state::UiText::new(&text, &text))
        } else {
            let text = format!("{} · {}", lang.t("已完成", "Completed"), text);
            TaskStatus::Completed(super::state::UiText::new(&text, &text))
        }
    } else {
        TaskStatus::Running(super::state::UiText::new(&text, &text))
    };
    theme::task_status(ui, lang, Some(&feedback), "");
    if let Some(scan) = job.scan_progress.as_ref().or(job.ui.scan_progress.as_ref()) {
        for (key, zh, en, phase) in [
            ("characters", "角色", "Characters", &scan.characters),
            ("weapons", "武器", "Weapons", &scan.weapons),
            ("artifacts", "圣遗物", "Artifacts", &scan.artifacts),
            ("achievements", "成就", "Achievements", &scan.achievements),
        ] {
            if let Some(phase) = phase {
                let mut step = Step::new(key, zh, en);
                step.completed = phase.completed;
                step.total = if key != "characters" && phase.total > 0 {
                    Some(phase.total)
                } else {
                    None
                };
                step.state = match phase.state {
                    PhaseState::Pending => StepState::Pending,
                    PhaseState::Running => StepState::Running,
                    PhaseState::Complete => StepState::Complete,
                    PhaseState::Aborted => StepState::Interrupted,
                };
                task_progress::row(ui, lang, &step);
            }
        }
    } else if let Some(progress) = job.progress.as_ref().or(job.ui.progress.as_ref()) {
        let mut step = Step::new("operations", "执行操作", "Apply operations");
        step.completed = progress.completed;
        step.total = Some(progress.total);
        step.state = StepState::Running;
        if let Some(result) = &job.result {
            step.completed = result.summary.total;
            step.state =
                if result.summary.errors + result.summary.aborted + result.summary.not_found == 0 {
                    StepState::Complete
                } else {
                    StepState::Interrupted
                };
        }
        task_progress::row(ui, lang, &step);
    }
    if let Some(result) = &job.result {
        let summary = &result.summary;
        ui.label(format!(
            "{} {} · {} {} · {} {}",
            lang.t("成功", "Succeeded"),
            summary.success,
            lang.t("已符合", "Already correct"),
            summary.already_correct,
            lang.t("未完成", "Unfinished"),
            summary.errors + summary.aborted + summary.not_found
        ));
    }
}
