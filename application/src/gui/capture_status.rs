use eframe::egui;

use super::{
    state::Lang,
    task_progress::{self, Step, StepState},
};

/// Both capture adapters report the same user journey from their actual monitor state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Ready,
    Starting,
    Login,
    Data,
    Achievements,
    Stopping,
    Exporting,
    Done,
    Stopped,
    Failed,
}

impl Stage {
    pub fn headline(self, lang: Lang) -> &'static str {
        match self {
            Self::Ready => lang.t("请先关闭游戏", "Close the game first"),
            Self::Starting => lang.t("正在准备抓包", "Preparing capture"),
            Self::Login => lang.t("请启动游戏并登录", "Launch the game and log in"),
            Self::Data => lang.t("正在接收游戏数据", "Receiving game data"),
            Self::Achievements => lang.t("请在游戏中打开成就", "Open Achievements in the game"),
            Self::Stopping => lang.t("正在停止抓包", "Stopping capture"),
            Self::Exporting => lang.t("正在导出数据", "Exporting data"),
            Self::Done => lang.t("数据已导出", "Data exported"),
            Self::Stopped => lang.t("抓包已停止 · 未导出", "Capture stopped · no export"),
            Self::Failed => lang.t("抓包未完成", "Capture failed"),
        }
    }
}

pub(super) fn waiting(
    key_exchange_captured: bool,
    missing_inventory: bool,
    missing_achievements: bool,
) -> Stage {
    if !key_exchange_captured {
        Stage::Login
    } else if !missing_inventory && missing_achievements {
        Stage::Achievements
    } else {
        Stage::Data
    }
}

pub(super) fn show_connection(
    ui: &mut egui::Ui,
    lang: Lang,
    stage: Stage,
    key_exchange_captured: bool,
) {
    let mut step = Step::new("key_exchange", "登录握手", "Login handshake");
    step.show_count = false;
    step.state = if key_exchange_captured {
        StepState::Complete
    } else if matches!(stage, Stage::Stopped | Stage::Failed) {
        StepState::Interrupted
    } else if stage == Stage::Login {
        StepState::Running
    } else {
        StepState::Pending
    };
    task_progress::row(ui, lang, &step);
}

pub(super) fn show_counters(
    ui: &mut egui::Ui,
    lang: Lang,
    elapsed: u64,
    packets: usize,
    commands: usize,
) {
    ui.label(lang.t(
        &format!("耗时 {elapsed} 秒 · 收包 {packets} · 解码 {commands}"),
        &format!("{elapsed}s elapsed · {packets} packets · {commands} decoded messages"),
    ));
}

#[cfg(test)]
pub(crate) mod feedback_tests {
    use super::*;

    #[test]
    fn handshake_step_never_shows_a_meaningless_item_count() {
        let content = texts(|ui| show_connection(ui, Lang::En, Stage::Login, false));
        assert!(content.iter().any(|text| text == "Login handshake"));
        assert!(!content.iter().any(|text| text == "0"));
    }

    #[test]
    fn only_a_captured_exchange_marks_the_handshake_complete() {
        for stage in [Stage::Login, Stage::Data, Stage::Done, Stage::Failed] {
            let missing = colored_texts(|ui| show_connection(ui, Lang::En, stage, false));
            assert!(missing.iter().any(|(text, color)| text == "Login handshake"
                && *color != super::super::theme::palette(true).success));
            let captured = colored_texts(|ui| show_connection(ui, Lang::En, stage, true));
            assert!(captured
                .iter()
                .any(|(text, color)| text == "Login handshake"
                    && *color == super::super::theme::palette(true).success));
        }
    }

    pub(crate) fn texts(draw: impl FnMut(&mut egui::Ui)) -> Vec<String> {
        colored_texts(draw)
            .into_iter()
            .map(|(text, _)| text)
            .collect()
    }

    pub(crate) fn colored_texts(
        mut draw: impl FnMut(&mut egui::Ui),
    ) -> Vec<(String, egui::Color32)> {
        fn collect(shape: &egui::Shape, texts: &mut Vec<(String, egui::Color32)>) {
            match shape {
                egui::Shape::Text(text) => {
                    for section in &text.galley.job.sections {
                        texts.push((
                            text.galley.job.text
                                [section.byte_range.start.0..section.byte_range.end.0]
                                .into(),
                            section.format.color,
                        ));
                    }
                },
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect(s, texts)),
                _ => {},
            }
        }
        let ctx = egui::Context::default();
        super::super::theme::setup(&ctx);
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(370.0, 700.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| draw(ui));
                },
            );
            output.textures_delta.clear();
            texts.clear();
            for clipped in output.shapes {
                collect(&clipped.shape, &mut texts);
            }
        }
        texts
    }
}
