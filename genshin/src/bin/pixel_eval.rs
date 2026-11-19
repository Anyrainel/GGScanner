//! Pixel-only calibration/evaluation for HDR dumps.
//!
//! This intentionally does no OCR. It uses a known-good scan export as labels
//! and the debug image dump as the pixel source.

use anyhow::{Context, Result};
use clap::Parser;
use genshin_scanner::scanner::common::capture_frame::CaptureFrame;
use genshin_scanner::scanner::common::constants::{
    ARTIFACT_ASTRAL_POS1, ARTIFACT_ASTRAL_POS2, ARTIFACT_LOCK_POS1, ARTIFACT_LOCK_POS2,
    CONSTELLATION_NODES, CONSTELLATION_RING_INNER, CONSTELLATION_RING_OUTER,
};
use genshin_scanner::scanner::common::coord_scaler::CoordScaler;
use genshin_scanner::scanner::common::grid_icon_detector::{GridMode, GridPageDetection};
use genshin_scanner::scanner::common::pixel_profile;
use genshin_scanner::scanner::common::pixel_utils;
use image::{io::Reader as ImageReader, RgbImage};
use rayon::prelude::*;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const ELIXIR_SHIFT: f64 = 40.0;
const HDR_ARTIFACT_LOCK_SAMPLE_DX: f64 = 6.0;
const HDR_ARTIFACT_LOCK_SAMPLE_DY: f64 = -9.0;
const HDR_ARTIFACT_ASTRAL_SAMPLE_DX: f64 = -1.0;
const HDR_ARTIFACT_ASTRAL_SAMPLE_DY: f64 = -2.0;
const HDR_ARTIFACT_ELIXIR_SAMPLE_DX: f64 = -6.0;
const HDR_ARTIFACT_ELIXIR_SAMPLE_DY: f64 = -7.0;

#[derive(Parser, Debug)]
struct Args {
    /// HDR debug image dump directory, e.g. target/release/debug_images_hdr.
    #[arg(long, default_value = "target/release/debug_images_hdr")]
    hdr_dump: PathBuf,

    /// Label export. For HDR calibration this should be the same inventory scanned without HDR.
    #[arg(
        long,
        default_value = "target/release/good_export_2026-04-26_23-29-08.json"
    )]
    label_export: PathBuf,

    /// HDR export, only used to print current scanner output deltas.
    #[arg(
        long,
        default_value = "target/release/good_export_2026-04-26_23-07-53.json"
    )]
    hdr_export: PathBuf,

    /// Evaluate production pixel rules with the HDR profile enabled.
    #[arg(long)]
    hdr_mode: bool,

    /// Only run one section: all, summary, panel, grid, constellation, substat, search-elixir.
    #[arg(long, default_value = "all")]
    section: String,
}

#[derive(Debug, Deserialize)]
struct Export {
    #[serde(default)]
    artifacts: Vec<Artifact>,
    #[serde(default)]
    weapons: Vec<Weapon>,
    #[serde(default)]
    characters: Vec<Character>,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    #[serde(default)]
    lock: bool,
    #[serde(default, rename = "astralMark")]
    astral_mark: bool,
    #[serde(default, rename = "elixirCrafted", alias = "elixerCrafted")]
    elixir_crafted: bool,
    #[serde(default, rename = "substats")]
    substats: Vec<SubStat>,
    #[serde(default, rename = "unactivatedSubstats")]
    unactivated_substats: Vec<SubStat>,
}

#[derive(Debug, Deserialize)]
struct SubStat {
    #[allow(dead_code)]
    key: String,
    #[allow(dead_code)]
    value: f64,
}

#[derive(Debug, Deserialize)]
struct Weapon {
    #[serde(default)]
    lock: bool,
}

#[derive(Debug, Deserialize)]
struct Character {
    #[allow(dead_code)]
    key: String,
    #[serde(default)]
    constellation: i32,
}

#[derive(Debug, Clone, Copy, Default)]
struct RgbF {
    r: f64,
    g: f64,
    b: f64,
}

impl RgbF {
    fn brightness(self) -> f64 {
        (self.r + self.g + self.b) / 3.0
    }

    fn max_min(self) -> f64 {
        self.r.max(self.g).max(self.b) - self.r.min(self.g).min(self.b)
    }
}

#[derive(Debug, Clone)]
struct LabeledValue {
    truth: bool,
    value: f64,
}

#[derive(Debug, Clone)]
struct LabeledColor {
    truth: bool,
    color: RgbF,
}

#[derive(Debug, Clone, Copy)]
struct Confusion {
    fp: usize,
    fn_: usize,
}

impl Confusion {
    fn errors(self) -> usize {
        self.fp + self.fn_
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let labels: Export = read_json(&args.label_export)
        .with_context(|| format!("read label export {}", args.label_export.display()))?;
    let hdr: Export = read_json(&args.hdr_export)
        .with_context(|| format!("read hdr export {}", args.hdr_export.display()))?;
    pixel_profile::set_hdr_mode(args.hdr_mode);

    match args.section.as_str() {
        "all" => {
            print_summary(&labels, &hdr);
            panel_eval(&args.hdr_dump, &labels)?;
            grid_eval(&args.hdr_dump, &labels)?;
            constellation_eval(&args.hdr_dump, &labels)?;
            substat_eval(&args.hdr_dump, &labels)?;
        },
        "summary" => print_summary(&labels, &hdr),
        "panel" => panel_eval(&args.hdr_dump, &labels)?,
        "grid" => grid_eval(&args.hdr_dump, &labels)?,
        "constellation" => constellation_eval(&args.hdr_dump, &labels)?,
        "substat" => substat_eval(&args.hdr_dump, &labels)?,
        "search-elixir" => search_elixir_points(&args.hdr_dump, &labels)?,
        other => anyhow::bail!("unknown --section {other}"),
    }

    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = std::fs::File::open(path)?;
    Ok(serde_json::from_reader(file)?)
}

fn read_index_map(root: &Path, kind: &str) -> Result<Vec<usize>> {
    read_json(&root.join(kind).join("index_map.json"))
        .with_context(|| format!("read {kind}/index_map.json"))
}

fn load_rgb(path: &Path) -> Result<RgbImage> {
    Ok(ImageReader::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .decode()
        .with_context(|| format!("decode {}", path.display()))?
        .to_rgb8())
}

fn artifact_full_path(root: &Path, index_map: &[usize], idx: usize) -> PathBuf {
    root.join("artifacts")
        .join(format!("{:04}", index_map[idx]))
        .join("full.png")
}

fn weapon_full_path(root: &Path, index_map: &[usize], idx: usize) -> PathBuf {
    root.join("weapons")
        .join(format!("{:04}", index_map[idx]))
        .join("full.png")
}

fn character_full_constellation_path(root: &Path, idx: usize) -> PathBuf {
    root.join("characters")
        .join(format!("{idx:04}"))
        .join("full_constellation.png")
}

fn print_summary(labels: &Export, hdr: &Export) {
    println!("=== SUMMARY ===");
    println!(
        "labels: artifacts={} weapons={} characters={}",
        labels.artifacts.len(),
        labels.weapons.len(),
        labels.characters.len()
    );
    println!(
        "hdr export: artifacts={} weapons={} characters={}",
        hdr.artifacts.len(),
        hdr.weapons.len(),
        hdr.characters.len()
    );
    println!(
        "labels artifacts: lock={} astral={} elixir={}",
        labels.artifacts.iter().filter(|a| a.lock).count(),
        labels.artifacts.iter().filter(|a| a.astral_mark).count(),
        labels.artifacts.iter().filter(|a| a.elixir_crafted).count()
    );
    println!(
        "hdr artifacts:    lock={} astral={} elixir={}",
        hdr.artifacts.iter().filter(|a| a.lock).count(),
        hdr.artifacts.iter().filter(|a| a.astral_mark).count(),
        hdr.artifacts.iter().filter(|a| a.elixir_crafted).count()
    );
    println!(
        "labels weapon lock={} hdr weapon lock={}",
        labels.weapons.iter().filter(|w| w.lock).count(),
        hdr.weapons.iter().filter(|w| w.lock).count()
    );
    println!(
        "labels constellations: {:?}",
        histogram(labels.characters.iter().map(|c| c.constellation))
    );
    println!(
        "hdr constellations:    {:?}",
        histogram(hdr.characters.iter().map(|c| c.constellation))
    );
}

fn histogram(values: impl Iterator<Item = i32>) -> BTreeMap<i32, usize> {
    let mut map = BTreeMap::new();
    for value in values {
        *map.entry(value).or_insert(0) += 1;
    }
    map
}

fn panel_eval(root: &Path, labels: &Export) -> Result<()> {
    println!("\n=== ARTIFACT PANEL PIXELS ===");
    let index_map = read_index_map(root, "artifacts")?;

    #[derive(Debug)]
    struct Row {
        elixir: bool,
        lock: bool,
        astral: bool,
        old_elixir: [RgbF; 3],
        proposed_elixir: [RgbF; 3],
        lock_prod: bool,
        astral_prod: bool,
        lock_brightness: [f64; 2],
        astral_brightness: [f64; 2],
    }

    let rows: Vec<Row> = labels
        .artifacts
        .par_iter()
        .enumerate()
        .filter_map(|(idx, artifact)| {
            let image = load_rgb(&artifact_full_path(root, &index_map, idx)).ok()?;
            let scaler = CoordScaler::new(image.width(), image.height());
            let y_shift = if artifact.elixir_crafted {
                ELIXIR_SHIFT
            } else {
                0.0
            };
            let old_elixir = [
                pixel_rgb(&image, &scaler, 1510.0, 423.0),
                pixel_rgb(&image, &scaler, 1520.0, 423.0),
                pixel_rgb(&image, &scaler, 1530.0, 423.0),
            ];
            let proposed_elixir = [
                pixel_rgb(&image, &scaler, 1440.0, 430.0),
                pixel_rgb(&image, &scaler, 1462.0, 430.0),
                pixel_rgb(&image, &scaler, 1490.0, 432.0),
            ];
            let lock_brightness = [
                pixel_rgb(
                    &image,
                    &scaler,
                    ARTIFACT_LOCK_POS1.0,
                    ARTIFACT_LOCK_POS1.1 + y_shift,
                )
                .brightness(),
                pixel_rgb(
                    &image,
                    &scaler,
                    ARTIFACT_LOCK_POS2.0,
                    ARTIFACT_LOCK_POS2.1 + y_shift,
                )
                .brightness(),
            ];
            let astral_brightness = [
                pixel_rgb(
                    &image,
                    &scaler,
                    ARTIFACT_ASTRAL_POS1.0,
                    ARTIFACT_ASTRAL_POS1.1 + y_shift,
                )
                .brightness(),
                pixel_rgb(
                    &image,
                    &scaler,
                    ARTIFACT_ASTRAL_POS2.0,
                    ARTIFACT_ASTRAL_POS2.1 + y_shift,
                )
                .brightness(),
            ];
            let frame = CaptureFrame::full(image);
            let lock_prod = pixel_utils::detect_artifact_lock(&frame, &scaler, y_shift);
            let astral_prod = pixel_utils::detect_artifact_astral_mark(&frame, &scaler, y_shift);
            Some(Row {
                elixir: artifact.elixir_crafted,
                lock: artifact.lock,
                astral: artifact.astral_mark,
                old_elixir,
                proposed_elixir,
                lock_prod,
                astral_prod,
                lock_brightness,
                astral_brightness,
            })
        })
        .collect();

    println!(
        "rows={} elixir={} lock={} astral={}",
        rows.len(),
        rows.iter().filter(|r| r.elixir).count(),
        rows.iter().filter(|r| r.lock).count(),
        rows.iter().filter(|r| r.astral).count()
    );

    let old_values: Vec<LabeledColor> = rows
        .iter()
        .flat_map(|r| {
            r.old_elixir.into_iter().map(|color| LabeledColor {
                truth: r.elixir,
                color,
            })
        })
        .collect();
    println!("old elixir probes (1510/1520/1530,423)");
    print_color_metrics(&old_values);

    let proposed_values: Vec<LabeledColor> = rows
        .iter()
        .flat_map(|r| {
            r.proposed_elixir.into_iter().map(|color| LabeledColor {
                truth: r.elixir,
                color,
            })
        })
        .collect();
    println!("proposed elixir probes (1440,430 / 1462,430 / 1490,432)");
    print_color_metrics(&proposed_values);
    print_rule(
        "panel elixir proposed: 2 of 3 where R+G production rule passes",
        rows.iter().map(|r| {
            let threshold = if pixel_profile::is_hdr_mode() {
                412.0
            } else {
                314.0
            };
            let pred = r
                .proposed_elixir
                .iter()
                .filter(|&&c| c.r + c.g < threshold)
                .count()
                >= 2;
            (r.elixir, pred)
        }),
    );

    print_rule(
        "panel lock production",
        rows.iter().map(|r| (r.lock, r.lock_prod)),
    );
    print_rule(
        "panel astral production",
        rows.iter().map(|r| (r.astral, r.astral_prod)),
    );
    for (label, values) in [
        (
            "panel lock pos1 brightness",
            rows.iter()
                .map(|r| LabeledValue {
                    truth: r.lock,
                    value: r.lock_brightness[0],
                })
                .collect::<Vec<_>>(),
        ),
        (
            "panel astral pos1 brightness",
            rows.iter()
                .map(|r| LabeledValue {
                    truth: r.astral,
                    value: r.astral_brightness[0],
                })
                .collect::<Vec<_>>(),
        ),
    ] {
        println!("{label}");
        print_distribution(&values);
        print_best_threshold(&values, Ordering::Less);
    }

    Ok(())
}

fn grid_eval(root: &Path, labels: &Export) -> Result<()> {
    println!("\n=== GRID PIXELS ===");
    eval_grid_kind(
        root,
        "artifacts",
        labels.artifacts.len(),
        GridMode::Artifact,
        |idx| {
            (
                labels.artifacts[idx].lock,
                labels.artifacts[idx].astral_mark,
                labels.artifacts[idx].elixir_crafted,
            )
        },
    )?;
    eval_grid_kind(
        root,
        "weapons",
        labels.weapons.len(),
        GridMode::Weapon,
        |idx| (labels.weapons[idx].lock, false, false),
    )?;
    Ok(())
}

fn eval_grid_kind<F>(root: &Path, kind: &str, len: usize, mode: GridMode, truth: F) -> Result<()>
where
    F: Fn(usize) -> (bool, bool, bool) + Sync,
{
    let index_map = read_index_map(root, kind)?;

    #[derive(Clone)]
    struct Row {
        lock_truth: bool,
        astral_truth: bool,
        elixir_truth: bool,
        lock_pred: bool,
        astral_pred: bool,
        elixir_pred: bool,
        lock_color: RgbF,
        slot2_color: RgbF,
        slot3_color: RgbF,
        elixir_color: RgbF,
    }

    let rows: Vec<Row> = (0..len)
        .step_by(40)
        .collect::<Vec<_>>()
        .into_par_iter()
        .filter_map(|page_start| {
            let path = match kind {
                "artifacts" => artifact_full_path(root, &index_map, page_start),
                "weapons" => weapon_full_path(root, &index_map, page_start),
                _ => unreachable!(),
            };
            let image = load_rgb(&path).ok()?;
            let scaler = CoordScaler::new(image.width(), image.height());
            let page_items = (len - page_start).min(40);
            let mut detection = GridPageDetection::with_mode(page_start, page_items, mode);
            detection.detect_pass(&image, &scaler, page_start);
            let (cells, _) = detection.annotation_snapshot()?;
            Some(
                (0..page_items)
                    .filter_map(|page_idx| {
                        let idx = page_start + page_idx;
                        let pred = detection.get(idx)?;
                        let cell = &cells[page_idx];
                        let spacing = cell.astral_pos.1 - cell.lock_pos.1;
                        let slot3_pos = (cell.astral_pos.0, cell.astral_pos.1 + spacing);
                        let (lock_truth, astral_truth, elixir_truth) = truth(idx);
                        let lock_color = if mode == GridMode::Artifact {
                            artifact_lock_rgb(&image, &scaler, cell.lock_pos)
                        } else {
                            mean_rgb(&image, &scaler, cell.lock_pos.0, cell.lock_pos.1, 4.0)
                        };
                        let slot2_color = if mode == GridMode::Artifact {
                            artifact_astral_rgb(&image, &scaler, cell.astral_pos)
                        } else {
                            mean_rgb(&image, &scaler, cell.astral_pos.0, cell.astral_pos.1, 4.0)
                        };
                        let slot3_color = if mode == GridMode::Artifact {
                            artifact_elixir_rgb(&image, &scaler, slot3_pos)
                        } else {
                            mean_rgb(&image, &scaler, slot3_pos.0, slot3_pos.1, 4.0)
                        };
                        let elixir_pos = if lock_truth && astral_truth {
                            slot3_pos
                        } else if lock_truth {
                            cell.astral_pos
                        } else {
                            cell.lock_pos
                        };
                        let elixir_color = if mode == GridMode::Artifact {
                            artifact_elixir_rgb(&image, &scaler, elixir_pos)
                        } else {
                            RgbF::default()
                        };
                        Some(Row {
                            lock_truth,
                            astral_truth,
                            elixir_truth,
                            lock_pred: pred.lock,
                            astral_pred: pred.astral,
                            elixir_pred: pred.elixir,
                            lock_color,
                            slot2_color,
                            slot3_color,
                            elixir_color,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .reduce(Vec::new, |mut left, mut right| {
            left.append(&mut right);
            left
        });

    println!("{kind}: rows={}", rows.len());
    print_rule(
        &format!("{kind} grid lock production"),
        rows.iter().map(|r| (r.lock_truth, r.lock_pred)),
    );
    for (i, row) in rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.lock_truth != r.lock_pred)
        .take(10)
    {
        println!(
            "{kind} lock mismatch idx={i} truth={} pred={} lock=({:.1},{:.1},{:.1}) slot2=({:.1},{:.1},{:.1}) slot3=({:.1},{:.1},{:.1})",
            row.lock_truth,
            row.lock_pred,
            row.lock_color.r,
            row.lock_color.g,
            row.lock_color.b,
            row.slot2_color.r,
            row.slot2_color.g,
            row.slot2_color.b,
            row.slot3_color.r,
            row.slot3_color.g,
            row.slot3_color.b,
        );
    }
    if mode == GridMode::Artifact {
        print_rule(
            "artifacts grid astral production",
            rows.iter().map(|r| (r.astral_truth, r.astral_pred)),
        );
        print_rule(
            "artifacts grid elixir production",
            rows.iter().map(|r| (r.elixir_truth, r.elixir_pred)),
        );
    }

    println!("{kind} grid lock slot color metrics");
    print_color_metrics(
        &rows
            .iter()
            .map(|r| LabeledColor {
                truth: r.lock_truth,
                color: r.lock_color,
            })
            .collect::<Vec<_>>(),
    );

    if mode == GridMode::Artifact {
        println!("artifact grid slot2 color metrics for astral among locked artifacts");
        print_color_metrics(
            &rows
                .iter()
                .filter(|r| r.lock_truth)
                .map(|r| LabeledColor {
                    truth: r.astral_truth,
                    color: r.slot2_color,
                })
                .collect::<Vec<_>>(),
        );
        println!("artifact grid elixir slot metrics");
        let elixir_samples: Vec<LabeledColor> = rows
            .iter()
            .map(|r| LabeledColor {
                truth: r.elixir_truth,
                color: r.elixir_color,
            })
            .collect();
        print_color_metrics(&elixir_samples);
    }

    Ok(())
}

fn constellation_eval(root: &Path, labels: &Export) -> Result<()> {
    println!("\n=== CONSTELLATION PIXELS ===");
    #[derive(Debug)]
    struct Row {
        level: i32,
        pred: i32,
        brightness: [f64; 6],
    }

    let rows: Vec<Row> = labels
        .characters
        .par_iter()
        .enumerate()
        .filter_map(|(idx, character)| {
            let path = character_full_constellation_path(root, idx);
            let image = load_rgb(&path).ok()?;
            let scaler = CoordScaler::new(image.width(), image.height());
            let result = pixel_utils::detect_constellation_pixel(&image, &scaler);
            let mut brightness = [0.0; 6];
            for (ci, value) in brightness.iter_mut().enumerate() {
                *value = sample_constellation_brightness(&image, &scaler, ci);
            }
            Some(Row {
                level: character.constellation,
                pred: result.level,
                brightness,
            })
        })
        .collect();

    println!("rows={}", rows.len());
    print_rule(
        "constellation production exact",
        rows.iter().map(|r| (true, r.level == r.pred)),
    );
    for ci in 0..6 {
        let values: Vec<LabeledValue> = rows
            .iter()
            .map(|r| LabeledValue {
                truth: ci < r.level as usize,
                value: r.brightness[ci],
            })
            .collect();
        println!("C{} ring brightness", ci + 1);
        print_distribution(&values);
        print_best_threshold(&values, Ordering::Greater);
    }

    Ok(())
}

fn substat_eval(root: &Path, labels: &Export) -> Result<()> {
    println!("\n=== SUBSTAT DIM PIXELS ===");
    let index_map = read_index_map(root, "artifacts")?;

    #[derive(Debug)]
    struct Row {
        inactive: bool,
        bright_pct: f64,
        mid_pct: f64,
        prod: bool,
    }

    let rows: Vec<Row> = labels
        .artifacts
        .par_iter()
        .enumerate()
        .flat_map(|(idx, artifact)| {
            let folder = index_map[idx];
            (0..4)
                .filter_map(move |line| {
                    let path = root
                        .join("artifacts")
                        .join(format!("{folder:04}"))
                        .join(format!("sub[{line}].png"));
                    let image = load_rgb(&path).ok()?;
                    let active_count = artifact.substats.len();
                    let inactive_count = artifact.unactivated_substats.len();
                    let inactive = line >= active_count && line < active_count + inactive_count;
                    let (bright_pct, mid_pct) = substat_crop_percentages(&image);
                    let prod = mid_pct > 20.0 && bright_pct < 78.0;
                    Some(Row {
                        inactive,
                        bright_pct,
                        mid_pct,
                        prod,
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    println!(
        "rows={} inactive={}",
        rows.len(),
        rows.iter().filter(|r| r.inactive).count()
    );
    print_rule(
        "substat dim production on dumped crops",
        rows.iter().map(|r| (r.inactive, r.prod)),
    );
    for (label, values) in [
        (
            "bright_pct",
            rows.iter()
                .map(|r| LabeledValue {
                    truth: r.inactive,
                    value: r.bright_pct,
                })
                .collect::<Vec<_>>(),
        ),
        (
            "mid_pct",
            rows.iter()
                .map(|r| LabeledValue {
                    truth: r.inactive,
                    value: r.mid_pct,
                })
                .collect::<Vec<_>>(),
        ),
    ] {
        println!("{label}");
        print_distribution(&values);
    }

    Ok(())
}

fn search_elixir_points(root: &Path, labels: &Export) -> Result<()> {
    println!("\n=== ELIXIR POINT SEARCH ===");
    let index_map = read_index_map(root, "artifacts")?;
    let coords: Vec<(f64, f64)> = (430..=620)
        .step_by(2)
        .flat_map(|y| (1280..=1680).step_by(2).map(move |x| (x as f64, y as f64)))
        .collect();

    #[derive(Clone, Copy)]
    struct Summary {
        pos_min: f64,
        pos_max: f64,
        neg_min: f64,
        neg_max: f64,
    }
    impl Default for Summary {
        fn default() -> Self {
            Self {
                pos_min: f64::INFINITY,
                pos_max: f64::NEG_INFINITY,
                neg_min: f64::INFINITY,
                neg_max: f64::NEG_INFINITY,
            }
        }
    }
    impl Summary {
        fn push(&mut self, truth: bool, value: f64) {
            if truth {
                self.pos_min = self.pos_min.min(value);
                self.pos_max = self.pos_max.max(value);
            } else {
                self.neg_min = self.neg_min.min(value);
                self.neg_max = self.neg_max.max(value);
            }
        }
        fn merge(&mut self, other: Self) {
            self.pos_min = self.pos_min.min(other.pos_min);
            self.pos_max = self.pos_max.max(other.pos_max);
            self.neg_min = self.neg_min.min(other.neg_min);
            self.neg_max = self.neg_max.max(other.neg_max);
        }
    }

    let summaries = labels
        .artifacts
        .par_iter()
        .enumerate()
        .fold(
            || vec![Summary::default(); coords.len()],
            |mut acc, (idx, artifact)| {
                let path = artifact_full_path(root, &index_map, idx);
                if let Ok(image) = load_rgb(&path) {
                    let scaler = CoordScaler::new(image.width(), image.height());
                    for (ci, &(x, y)) in coords.iter().enumerate() {
                        let color = pixel_rgb(&image, &scaler, x, y);
                        acc[ci].push(artifact.elixir_crafted, purple_without_green_score(color));
                    }
                }
                acc
            },
        )
        .reduce(
            || vec![Summary::default(); coords.len()],
            |mut a, b| {
                for (left, right) in a.iter_mut().zip(b) {
                    left.merge(right);
                }
                a
            },
        );

    let mut best: Vec<_> = coords
        .iter()
        .zip(summaries.iter())
        .filter_map(|(&(x, y), s)| {
            if s.pos_min > s.neg_max {
                Some((s.pos_min - s.neg_max, x, y, s.pos_min, s.neg_max))
            } else {
                None
            }
        })
        .collect();
    best.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));
    for (gap, x, y, pos_edge, neg_edge) in best.into_iter().take(40) {
        println!("gap={gap:.1} at=({x:.0},{y:.0}) pos_min={pos_edge:.1} neg_max={neg_edge:.1}");
    }
    Ok(())
}

fn pixel_rgb(image: &RgbImage, scaler: &CoordScaler, bx: f64, by: f64) -> RgbF {
    let x = scaler.x(bx) as u32;
    let y = scaler.y(by) as u32;
    if x < image.width() && y < image.height() {
        let p = image.get_pixel(x, y);
        RgbF {
            r: p[0] as f64,
            g: p[1] as f64,
            b: p[2] as f64,
        }
    } else {
        RgbF::default()
    }
}

fn mean_rgb(image: &RgbImage, scaler: &CoordScaler, bx: f64, by: f64, half: f64) -> RgbF {
    let cx = scaler.scale_x(bx);
    let cy = scaler.scale_y(by);
    let half_w = scaler.scale_x(half);
    let half_h = scaler.scale_y(half);
    let x1 = ((cx - half_w) as u32).min(image.width().saturating_sub(1));
    let y1 = ((cy - half_h) as u32).min(image.height().saturating_sub(1));
    let x2 = ((cx + half_w) as u32).min(image.width());
    let y2 = ((cy + half_h) as u32).min(image.height());
    if x1 >= x2 || y1 >= y2 {
        return RgbF::default();
    }

    let mut sum = RgbF::default();
    let mut count = 0.0;
    for y in y1..y2 {
        for x in x1..x2 {
            let p = image.get_pixel(x, y);
            sum.r += p[0] as f64;
            sum.g += p[1] as f64;
            sum.b += p[2] as f64;
            count += 1.0;
        }
    }
    RgbF {
        r: sum.r / count,
        g: sum.g / count,
        b: sum.b / count,
    }
}

fn artifact_lock_rgb(image: &RgbImage, scaler: &CoordScaler, pos: (f64, f64)) -> RgbF {
    let (dx, dy) = if pixel_profile::is_hdr_mode() {
        (HDR_ARTIFACT_LOCK_SAMPLE_DX, HDR_ARTIFACT_LOCK_SAMPLE_DY)
    } else {
        (0.0, 0.0)
    };
    mean_rgb(image, scaler, pos.0 + dx, pos.1 + dy, 4.0)
}

fn artifact_astral_rgb(image: &RgbImage, scaler: &CoordScaler, pos: (f64, f64)) -> RgbF {
    let (dx, dy) = if pixel_profile::is_hdr_mode() {
        (HDR_ARTIFACT_ASTRAL_SAMPLE_DX, HDR_ARTIFACT_ASTRAL_SAMPLE_DY)
    } else {
        (0.0, 0.0)
    };
    mean_rgb(image, scaler, pos.0 + dx, pos.1 + dy, 4.0)
}

fn artifact_elixir_rgb(image: &RgbImage, scaler: &CoordScaler, pos: (f64, f64)) -> RgbF {
    let (dx, dy) = if pixel_profile::is_hdr_mode() {
        (HDR_ARTIFACT_ELIXIR_SAMPLE_DX, HDR_ARTIFACT_ELIXIR_SAMPLE_DY)
    } else {
        (0.0, 0.0)
    };
    mean_rgb(image, scaler, pos.0 + dx, pos.1 + dy, 4.0)
}

fn purple_without_green_score(color: RgbF) -> f64 {
    color.r.min(color.b) - (color.r - color.b).abs()
}

fn sample_constellation_brightness(image: &RgbImage, scaler: &CoordScaler, c_index: usize) -> f64 {
    let (cx, cy) = CONSTELLATION_NODES[c_index];
    let r_inner = CONSTELLATION_RING_INNER;
    let r_outer = CONSTELLATION_RING_OUTER;
    let r_inner_sq = (r_inner as f64) * (r_inner as f64);
    let r_outer_sq = (r_outer as f64) * (r_outer as f64);

    let mut sum = 0.0;
    let mut count = 0.0;
    for bx in ((cx as i32 - r_outer)..=(cx as i32 + r_outer)).step_by(2) {
        for dy in (-r_outer..=r_outer).step_by(2) {
            let dx = bx as f64 - cx;
            let dist_sq = dx * dx + (dy as f64) * (dy as f64);
            if dist_sq >= r_inner_sq && dist_sq <= r_outer_sq {
                let px = scaler.x(bx as f64) as u32;
                let py = scaler.y(cy + dy as f64) as u32;
                if px < image.width() && py < image.height() {
                    let p = image.get_pixel(px, py);
                    sum += (p[0] as f64 + p[1] as f64 + p[2] as f64) / 3.0;
                    count += 1.0;
                }
            }
        }
    }

    if count > 0.0 {
        sum / count
    } else {
        0.0
    }
}

fn substat_crop_percentages(image: &RgbImage) -> (f64, f64) {
    let start_x = image.width() / 3;
    let mut bright = 0.0;
    let mut mid = 0.0;
    let mut total = 0.0;
    for y in (0..image.height()).step_by(2) {
        for x in (start_x..image.width()).step_by(2) {
            let p = image.get_pixel(x, y);
            let b = (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3;
            total += 1.0;
            if b > 200 {
                bright += 1.0;
            } else if b > 130 {
                mid += 1.0;
            }
        }
    }
    if total == 0.0 {
        (0.0, 0.0)
    } else {
        (bright * 100.0 / total, mid * 100.0 / total)
    }
}

fn print_color_metrics(values: &[LabeledColor]) {
    for (name, f) in [
        ("R", (|c: RgbF| c.r) as fn(RgbF) -> f64),
        ("G", (|c: RgbF| c.g) as fn(RgbF) -> f64),
        ("B", (|c: RgbF| c.b) as fn(RgbF) -> f64),
        ("B-R", |c: RgbF| c.b - c.r),
        ("R-G", |c: RgbF| c.r - c.g),
        ("G-B", |c: RgbF| c.g - c.b),
        ("R-B", |c: RgbF| c.r - c.b),
        ("sat", RgbF::max_min as fn(RgbF) -> f64),
        ("brightness", RgbF::brightness as fn(RgbF) -> f64),
        (
            "purple_no_green",
            purple_without_green_score as fn(RgbF) -> f64,
        ),
    ] {
        let vals: Vec<LabeledValue> = values
            .iter()
            .map(|v| LabeledValue {
                truth: v.truth,
                value: f(v.color),
            })
            .collect();
        print!("{name:>16}: ");
        print_distribution_inline(&vals);
    }
    print_best_linear_projection(values);
}

fn print_best_linear_projection(values: &[LabeledColor]) {
    if values.iter().all(|v| v.truth) || values.iter().all(|v| !v.truth) {
        return;
    }

    #[derive(Clone, Copy)]
    struct Candidate {
        a: i32,
        b: i32,
        c: i32,
        threshold: f64,
        margin: f64,
        fp: usize,
        fn_: usize,
        positive_above: bool,
    }

    let mut best: Option<Candidate> = None;
    for a in -1..=1 {
        for b in -1..=1 {
            for c in -1..=1 {
                if a == 0 && b == 0 && c == 0 {
                    continue;
                }
                // Fix sign symmetry so the same hyperplane is only considered once.
                if a < 0 || (a == 0 && b < 0) || (a == 0 && b == 0 && c < 0) {
                    continue;
                }
                let norm = ((a * a + b * b + c * c) as f64).sqrt();
                let projected: Vec<LabeledValue> = values
                    .iter()
                    .map(|v| LabeledValue {
                        truth: v.truth,
                        value: a as f64 * v.color.r + b as f64 * v.color.g + c as f64 * v.color.b,
                    })
                    .collect();

                for &positive_above in &[true, false] {
                    let pos: Vec<f64> = projected
                        .iter()
                        .filter(|v| v.truth)
                        .map(|v| v.value)
                        .collect();
                    let neg: Vec<f64> = projected
                        .iter()
                        .filter(|v| !v.truth)
                        .map(|v| v.value)
                        .collect();
                    let (lower_edge, upper_edge) = if positive_above {
                        (
                            neg.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                            pos.iter().copied().fold(f64::INFINITY, f64::min),
                        )
                    } else {
                        (
                            pos.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                            neg.iter().copied().fold(f64::INFINITY, f64::min),
                        )
                    };
                    let threshold = (lower_edge + upper_edge) / 2.0;
                    let margin = (upper_edge - lower_edge) / (2.0 * norm);
                    let confusion = if positive_above {
                        confusion(projected.iter().map(|v| (v.truth, v.value >= threshold)))
                    } else {
                        confusion(projected.iter().map(|v| (v.truth, v.value < threshold)))
                    };
                    let candidate = Candidate {
                        a,
                        b,
                        c,
                        threshold,
                        margin,
                        fp: confusion.fp,
                        fn_: confusion.fn_,
                        positive_above,
                    };
                    let replace = match best {
                        None => true,
                        Some(current) => {
                            let cand_err = candidate.fp + candidate.fn_;
                            let cur_err = current.fp + current.fn_;
                            cand_err < cur_err
                                || (cand_err == cur_err && candidate.margin > current.margin + 1e-9)
                        },
                    };
                    if replace {
                        best = Some(candidate);
                    }
                }
            }
        }
    }

    if let Some(best) = best {
        let op = if best.positive_above { ">=" } else { "<" };
        println!(
            "{:>16}: value={}R {:+}G {:+}B {op} {:.1}: margin={:.1} fp={} fn={}",
            "best linear", best.a, best.b, best.c, best.threshold, best.margin, best.fp, best.fn_
        );
    }
}

fn print_distribution(values: &[LabeledValue]) {
    print!("  positive ");
    print_quantiles(values.iter().filter(|v| v.truth).map(|v| v.value));
    print!("  negative ");
    print_quantiles(values.iter().filter(|v| !v.truth).map(|v| v.value));
}

fn print_distribution_inline(values: &[LabeledValue]) {
    print!("pos ");
    print_quantiles(values.iter().filter(|v| v.truth).map(|v| v.value));
    print!("                  neg ");
    print_quantiles(values.iter().filter(|v| !v.truth).map(|v| v.value));
}

fn print_quantiles(iter: impl Iterator<Item = f64>) {
    let mut vals: Vec<f64> = iter.filter(|v| v.is_finite()).collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    if vals.is_empty() {
        println!("n=0");
        return;
    }
    let pct = |p: f64| -> f64 {
        let i = ((vals.len() - 1) as f64 * p).round() as usize;
        vals[i]
    };
    println!(
        "n={} p00={:.1} p01={:.1} p05={:.1} p50={:.1} p95={:.1} p99={:.1} p100={:.1}",
        vals.len(),
        pct(0.0),
        pct(0.01),
        pct(0.05),
        pct(0.50),
        pct(0.95),
        pct(0.99),
        pct(1.0)
    );
}

fn print_best_threshold(values: &[LabeledValue], positive_when: Ordering) {
    let pos: Vec<f64> = values.iter().filter(|v| v.truth).map(|v| v.value).collect();
    let neg: Vec<f64> = values
        .iter()
        .filter(|v| !v.truth)
        .map(|v| v.value)
        .collect();
    if pos.is_empty() || neg.is_empty() {
        return;
    }

    let (lower_edge, upper_edge) = match positive_when {
        Ordering::Greater => (
            neg.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            pos.iter().copied().fold(f64::INFINITY, f64::min),
        ),
        Ordering::Less => (
            pos.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            neg.iter().copied().fold(f64::INFINITY, f64::min),
        ),
        Ordering::Equal => unreachable!(),
    };
    let threshold = (lower_edge + upper_edge) / 2.0;
    let margin = (upper_edge - lower_edge) / 2.0;
    let confusion = match positive_when {
        Ordering::Less => confusion(values.iter().map(|v| (v.truth, v.value < threshold))),
        Ordering::Greater => confusion(values.iter().map(|v| (v.truth, v.value >= threshold))),
        Ordering::Equal => unreachable!(),
    };
    let op = if positive_when == Ordering::Less {
        "<"
    } else {
        ">="
    };
    println!(
        "  max-margin threshold value {op} {threshold:.1}: margin={margin:.1} fp={} fn={}",
        confusion.fp, confusion.fn_
    );
}

fn print_rule(label: &str, samples: impl Iterator<Item = (bool, bool)>) {
    let c = confusion(samples);
    println!("{label}: err={} fp={} fn={}", c.errors(), c.fp, c.fn_);
}

fn confusion(samples: impl Iterator<Item = (bool, bool)>) -> Confusion {
    let mut fp = 0;
    let mut fn_ = 0;
    for (truth, pred) in samples {
        if pred && !truth {
            fp += 1;
        } else if truth && !pred {
            fn_ += 1;
        }
    }
    Confusion { fp, fn_ }
}
