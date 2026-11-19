//! Enter the signed-in world or return to its title screen for a new login capture.
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use image::RgbImage;
use yas::ocr::ImageToText;

use super::capture_frame::CaptureFrame;
use super::coord_scaler::CoordScaler;
use super::game_controller::GenshinGameController;
use super::ocr_factory::create_ocr_model;

type Rect = (f64, f64, f64, f64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    World,
    Menu,
    ExitOptions,
    Title,
    Door,
    Unknown,
}

struct Observation<'a> {
    frame: &'a CaptureFrame,
    scaler: &'a CoordScaler,
    ocr: &'a dyn ImageToText<RgbImage>,
}

impl Observation<'_> {
    fn read(&self, rect: Rect) -> Result<String> {
        let crop = self
            .frame
            .crop(rect, 0.0, self.scaler)
            .context("empty game UI region")?;
        Ok(self.ocr.image_to_text(&crop, false)?.trim().to_lowercase())
    }

    fn label(&self, rect: Rect, labels: &[&str]) -> Result<bool> {
        let actual = self.read(rect)?;
        let compact: String = actual.chars().filter(|c| c.is_alphanumeric()).collect();
        Ok(labels.contains(&compact.as_str()))
    }

    fn screen(&self) -> Result<Screen> {
        // Neither a white Paimon button nor a bright loading screen establishes the HUD.
        // The active character's HP fraction shares a line with the bottom-center gauge.
        let icon = [(62., 51.), (53., 47.), (49., 35.), (55., 70.), (67., 77.)];
        if icon
            .iter()
            .filter(|&&(x, y)| {
                let [r, g, b] = self.frame.pixel(self.scaler, x, y);
                u32::from(r) + u32::from(g) + u32::from(b) > 480
            })
            .count()
            >= 3
        {
            // At 1280×720 the chat Enter keycap spans x=58..104 at y=692.
            // Its label and the level beside the HP gauge are on opposite HUD edges.
            let health_bar = (1008..1017).any(|y| {
                (815..1105)
                    .step_by(5)
                    .filter(|&x| {
                        let [r, g, b] = self.frame.pixel(self.scaler, x as f64, y as f64);
                        g > 110 && g.saturating_sub(r) > 18 && g.saturating_sub(b) > 18
                    })
                    .count()
                    >= 14
            });
            if self.label((85., 1022., 74., 32.), &["enter"])?
                && (health_bar || level_label(&self.read((723., 991., 94., 35.))?))
            {
                return Ok(Screen::World);
            }
            for y in [991., 1002., 980.] {
                if hp_fraction(&self.read((866., y, 189., 26.))?) {
                    return Ok(Screen::World);
                }
            }
        }
        // The three-way menu offers both a return to this login and a desktop quit.
        if self.label(
            (826., 395., 310., 49.),
            &["继续游戏", "continueplaying", "continuegame"],
        )? && self.label(
            (826., 515., 310., 49.),
            &["退出至登录界面", "quittotitlescreen", "exittologinscreen"],
        )? && self.label(
            (826., 635., 310., 49.),
            &["退出至桌面", "quittodesktop", "exittodesktop"],
        )? {
            return Ok(Screen::ExitOptions);
        }
        // The parchment between tiles stays bright only when the menu is unobstructed.
        // The exit glyph has a gold door frame and a left arrow around a dark center.
        let menu_paper = [(148.5, 489.), (111., 480.), (723., 490.5)]
            .iter()
            .all(|&(x, y)| {
                let [r, g, b] = self.frame.pixel(self.scaler, x, y);
                r > 167 && g > 160 && b > 144
            });
        let exit_icon = [
            (45., 1008.),
            (60., 1014.),
            (60., 1033.5),
            (37.5, 1024.5),
            (45., 1045.5),
        ]
        .iter()
        .filter(|&&(x, y)| {
            let [r, g, b] = self.frame.pixel(self.scaler, x, y);
            r > 158 && r.saturating_sub(b) > 24 && g.saturating_sub(b) > 9
        })
        .count()
            >= 4
            && [1017., 1035.]
                .iter()
                .all(|&y| self.frame.pixel(self.scaler, 46.5, y)[0] < 120);
        if menu_paper
            && exit_icon
            && self.label((593., 437., 143., 37.), &["成就", "achievements"])?
            && self.label((123., 437., 148., 37.), &["商城", "shop"])?
        {
            return Ok(Screen::Menu);
        }
        if self.label((789., 465., 342., 70.), &["开始游戏", "startgame"])?
            && self.label(
                (879., 568., 162., 43.),
                &["点击进入", "clicktoenter", "clicktobegin"],
            )?
            && self.label(
                (830., 901., 260., 54.),
                &[
                    "天空岛",
                    "世界树",
                    "america",
                    "europe",
                    "asia",
                    "twhkmo",
                    "sar",
                ],
            )?
        {
            return Ok(Screen::Title);
        }
        // The second title step has four undimmed circular controls on the right,
        // a door in the middle, and the build string on the bottom left.
        let controls = [710., 801., 891., 982.].iter().all(|&y| {
            [1802., 1845.].iter().all(|&x| {
                let rgb = self.frame.pixel(self.scaler, x, y);
                rgb.iter().all(|&channel| channel >= 215)
            })
        });
        if controls
            && self.label(
                (865., 996., 190., 45.),
                &["点击进入", "clicktobegin", "clicktoenter"],
            )?
        {
            let brightness = |x, y| {
                self.frame
                    .pixel(self.scaler, x, y)
                    .into_iter()
                    .map(i32::from)
                    .sum::<i32>()
            };
            let door = [520., 590., 690., 720.]
                .iter()
                .filter(|&&y| {
                    let middle = brightness(960., y);
                    (brightness(810., y) - middle).abs() > 60
                        && (brightness(1085., y) - middle).abs() > 60
                })
                .count()
                >= 3;
            let version = self.read((19., 1035., 620., 40.))?;
            if door && (version.starts_with("osrelwin") || version.starts_with("cnrelwin")) {
                return Ok(Screen::Door);
            }
        }
        Ok(Screen::Unknown)
    }
}

fn hp_fraction(text: &str) -> bool {
    let Some((hp, max)) = text.trim().split_once(['/', '／']) else {
        return false;
    };
    let (Ok(hp), Ok(max)) = (hp.trim().parse::<u32>(), max.trim().parse::<u32>()) else {
        return false;
    };
    max >= 100 && hp <= max
}

fn level_label(text: &str) -> bool {
    let compact: String = text.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    compact
        .strip_prefix("lv")
        .and_then(|n| n.parse::<u16>().ok())
        .is_some_and(|n| (1..=100).contains(&n))
}

fn check_game(ctrl: &GenshinGameController) -> Result<()> {
    if ctrl.check_rmb() {
        bail!("Game session entry cancelled.");
    }
    if !ctrl.game_info.native_window().is_foreground() {
        bail!("The game lost focus. Session entry stopped.");
    }
    Ok(())
}

fn stopped(frame: &CaptureFrame, expected: Option<Screen>, actual: Screen) -> anyhow::Error {
    let path = crate::cli::exe_dir().join("data/gi_session_entry/stopped.png");
    let save = std::fs::create_dir_all(path.parent().unwrap())
        .map_err(anyhow::Error::from)
        .and_then(|()| Ok(frame.image.save(&path)?));
    match save {
        Ok(()) => anyhow::anyhow!("Session entry stopped on {actual:?} (expected {expected:?}). Screenshot: {}", path.display()),
        Err(e) => anyhow::anyhow!("Session entry stopped on {actual:?} (expected {expected:?}). Saving the game screenshot failed: {e}"),
    }
}

/// Only dispatch actions for recognized game screens; no caller-defined input is accepted.
pub fn enter_or_refresh_world(ctrl: &mut GenshinGameController) -> Result<()> {
    let ocr = create_ocr_model("ppocrv5")?;
    ctrl.focus_game_window();
    let mut expected = None;
    let mut deadline = Instant::now();
    loop {
        check_game(ctrl)?;
        let frame = CaptureFrame::full(ctrl.capture_game()?);
        let screen = Observation {
            frame: &frame,
            scaler: &ctrl.scaler,
            ocr: &*ocr,
        }
        .screen()?;
        if let Some(next) = expected {
            if screen != next && !(next == Screen::Door && screen == Screen::World) {
                if Instant::now() >= deadline {
                    return Err(stopped(&frame, expected, screen));
                }
                std::thread::sleep(Duration::from_millis(400));
                continue;
            }
            if screen == Screen::World {
                return Ok(());
            }
        }
        check_game(ctrl)?;
        expected = Some(match screen {
            Screen::World => {
                ctrl.key_press(enigo::Key::Escape);
                Screen::Menu
            },
            Screen::Menu => {
                ctrl.click_at(46.5, 1026.);
                Screen::ExitOptions
            },
            Screen::ExitOptions => {
                ctrl.click_at(960., 540.);
                Screen::Title
            },
            Screen::Title => {
                ctrl.click_at(960., 590.);
                Screen::Door
            },
            Screen::Door => {
                ctrl.click_at(960., 1020.);
                Screen::World
            },
            Screen::Unknown => return Err(stopped(&frame, expected, screen)),
        });
        deadline = Instant::now()
            + Duration::from_secs(
                if matches!(expected, Some(Screen::Title | Screen::Door | Screen::World)) {
                    45
                } else {
                    6
                },
            );
        std::thread::sleep(Duration::from_millis(450));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each cropped label's top-left pixel selects a deterministic OCR response.
    // This exercises the actual fixed screen layout without storing account screenshots.
    struct Labels;

    impl ImageToText<RgbImage> for Labels {
        fn image_to_text(&self, crop: &RgbImage, _: bool) -> Result<String> {
            let labels = [
                "",
                "继续游戏",
                "退出至登录界面",
                "退出至桌面",
                "开始游戏",
                "点击进入",
                "America",
                "账号登录",
            ];
            Ok(labels
                .get(crop.get_pixel(0, 0)[0] as usize)
                .unwrap_or(&"")
                .to_string())
        }

        fn get_average_inference_time(&self) -> Option<Duration> {
            None
        }
    }

    fn classify(markers: &[(u32, u32, u8)]) -> Screen {
        let mut image = RgbImage::new(1920, 1080);
        for &(x, y, id) in markers {
            image.put_pixel(x, y, image::Rgb([id, 0, 0]));
        }
        Observation {
            frame: &CaptureFrame::full(image),
            scaler: &CoordScaler::new(1920, 1080),
            ocr: &Labels,
        }
        .screen()
        .unwrap()
    }

    #[test]
    fn requires_the_return_option_between_resume_and_quit_to_desktop() {
        let mut rows = [(826, 395, 1), (826, 515, 2), (826, 635, 3)];
        assert_eq!(classify(&rows), Screen::ExitOptions);
        rows[1].2 = 3;
        rows[2].2 = 2;
        assert_eq!(classify(&rows), Screen::Unknown);
        rows[1].2 = 2;
        rows[2].2 = 0;
        assert_eq!(classify(&rows), Screen::Unknown);
    }

    #[test]
    fn requires_the_signed_in_title_and_selected_server() {
        let mut rows = [(789, 465, 4), (879, 568, 5), (830, 901, 6)];
        assert_eq!(classify(&rows), Screen::Title);
        rows[0].2 = 7;
        assert_eq!(classify(&rows), Screen::Unknown);
        rows[0].2 = 4;
        rows[2].2 = 0;
        assert_eq!(classify(&rows), Screen::Unknown);
    }

    #[test]
    fn requires_a_health_fraction_to_confirm_the_world_hud() {
        for valid in ["23542 / 23542", " 138／12500 ", "0/8600"] {
            assert!(hp_fraction(valid));
        }
        for invalid in ["UID 145789", "52", "1/4", "7/0", "2/3/4", "16000/12000"] {
            assert!(!hp_fraction(invalid));
        }
    }
}
