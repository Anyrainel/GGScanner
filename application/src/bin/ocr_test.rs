//! Quick CLI tool to OCR a given image using ppocrv4 and ppocrv5.
//!
//! Usage:
//!   ocr_test <image_path> [--crop-right <pixels>]
//!   ocr_test --equip <image_path>
//!   ocr_test --char-name <image_path>
//!   ocr_test --eval-ocr <debug_images_dir>
//!   ocr_test --reprocess <images_dir> [--output <json_path>]
//!
//! Default mode: runs both engines, prints raw text and stat parsing results.
//! --equip mode: tests the full equip pipeline (OCR → parse → fuzzy match).
//! --char-name mode: tests character name pipeline (OCR → parse → fuzzy match).
//! --eval-ocr mode: batch evaluate v4 vs v5 accuracy on character names and equip text.
//! --reprocess mode: re-runs artifact scanner on dumped full.png images, outputs GOOD JSON.

use anyhow::Result;
use genshin_scanner::scanner::common::capture_frame::CaptureFrame;
use genshin_scanner::scanner::common::equip_parser;
use genshin_scanner::scanner::common::fuzzy_match::fuzzy_match_map;
use genshin_scanner::scanner::common::mappings::{MappingManager, NameOverrides};
use genshin_scanner::scanner::common::ocr_factory::create_ocr_model;

fn main() -> Result<()> {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: ocr_test <image_path> [--crop-right <pixels>]");
        eprintln!("       ocr_test --equip <image_path>");
        eprintln!("       ocr_test --char-name <image_path>");
        eprintln!("       ocr_test --eval-ocr <debug_images_dir>");
        eprintln!("       ocr_test --reprocess <images_dir> [--output <json_path>]");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "--sel-verify" => {
            if args.len() < 4 {
                eprintln!("Usage: ocr_test --sel-verify <screenshot_dir> <export.json>");
                std::process::exit(1);
            }
            run_sel_verify(&args[2], &args[3])
        },
        "--sel-ocr" => {
            if args.len() < 3 {
                eprintln!("Usage: ocr_test --sel-ocr <screenshot_or_dir>");
                std::process::exit(1);
            }
            run_sel_ocr(&args[2])
        },
        "--equip" => {
            if args.len() < 3 {
                eprintln!("Usage: ocr_test --equip <image_path>");
                std::process::exit(1);
            }
            run_equip_test(&args[2])
        },
        "--char-name" => {
            if args.len() < 3 {
                eprintln!("Usage: ocr_test --char-name <image_path>");
                std::process::exit(1);
            }
            run_char_name_test(&args[2])
        },
        "--eval-ocr" => {
            if args.len() < 3 {
                eprintln!("Usage: ocr_test --eval-ocr <debug_images_dir>");
                std::process::exit(1);
            }
            run_eval_ocr(&args[2])
        },
        "--reprocess" => {
            if args.len() < 3 {
                eprintln!("Usage: ocr_test --reprocess <images_dir> [--output <json_path>]");
                std::process::exit(1);
            }
            let output = if args.len() >= 5 && args[3] == "--output" {
                Some(args[4].as_str())
            } else {
                None
            };
            run_reprocess(&args[2], output)
        },
        _ => run_ocr_test(&args),
    }
}

fn run_ocr_test(args: &[String]) -> Result<()> {
    let image_path = &args[1];
    let mut crop_right: u32 = 0;
    if args.len() >= 4 && args[2] == "--crop-right" {
        crop_right = args[3].parse().unwrap_or(0);
    }

    // Load image
    let img = image::open(image_path)?.to_rgb8();
    println!("Image: {} ({}x{})", image_path, img.width(), img.height());

    // Optionally crop right side
    let img = if crop_right > 0 && crop_right < img.width() {
        let new_w = img.width() - crop_right;
        println!(
            "Cropping {}px from right -> {}x{}",
            crop_right,
            new_w,
            img.height()
        );
        image::imageops::crop_imm(&img, 0, 0, new_w, img.height()).to_image()
    } else {
        img
    };

    // Load models
    println!("Loading models...");
    let v4 = create_ocr_model("ppocrv4")?;
    let v5 = create_ocr_model("ppocrv5")?;

    // Run OCR
    let result_v4 = v4.image_to_text(&img, false)?;
    let result_v5 = v5.image_to_text(&img, false)?;

    println!();
    println!("ppocrv4: {:?}", result_v4);
    println!("ppocrv5: {:?}", result_v5);

    // Also try stat parsing
    let parsed_v4 =
        genshin_scanner::scanner::common::stat_parser::parse_stat_from_text(result_v4.trim());
    let parsed_v5 =
        genshin_scanner::scanner::common::stat_parser::parse_stat_from_text(result_v5.trim());

    println!();
    if let Some(p) = &parsed_v4 {
        println!(
            "ppocrv4 parsed: key={}, value={}, inactive={}",
            p.key, p.value, p.inactive
        );
    } else {
        println!("ppocrv4 parsed: None");
    }
    if let Some(p) = &parsed_v5 {
        println!(
            "ppocrv5 parsed: key={}, value={}, inactive={}",
            p.key, p.value, p.inactive
        );
    } else {
        println!("ppocrv5 parsed: None");
    }

    Ok(())
}

/// Test selection view OCR on full screen captures.
/// Crops each region, binarizes, and OCRs. Also runs solver validation.
fn run_sel_ocr(path: &str) -> Result<()> {
    use genshin_scanner::scanner::common::roll_solver::{self, OcrCandidate, SolverInput};
    use genshin_scanner::scanner::common::stat_parser;

    // Selection view crop regions (base 1920x1080)
    const MAIN_STAT: (f64, f64, f64, f64) = (1440.0, 217.0, 250.0, 30.0);
    const LEVEL: (f64, f64, f64, f64) = (1443.0, 310.0, 100.0, 26.0);
    const STAR4_POS: (f64, f64) = (1578.0, 280.0);
    const STAR5_POS: (f64, f64) = (1611.0, 280.0);
    const SUBS: [(f64, f64, f64, f64); 4] = [
        (1460.0, 349.0, 256.0, 30.0),
        (1460.0, 383.0, 256.0, 30.0),
        (1460.0, 417.0, 256.0, 30.0),
        (1460.0, 451.0, 336.0, 30.0),
    ];
    const SET_NAME: (f64, f64, f64, f64) = (1430.0, 489.0, 300.0, 30.0);
    const SUB_SPACING: f64 = 34.0;

    fn crop_binarize(img: &image::RgbImage, rect: (f64, f64, f64, f64)) -> image::RgbImage {
        let scale = img.width() as f64 / 1920.0;
        let x = (rect.0 * scale).round() as u32;
        let y = (rect.1 * scale).round() as u32;
        let w = (rect.2 * scale).round().min((img.width() - x) as f64) as u32;
        let h = (rect.3 * scale).round().min((img.height() - y) as f64) as u32;
        let mut cropped = image::imageops::crop_imm(img, x, y, w, h).to_image();
        for pixel in cropped.pixels_mut() {
            let brightness = (pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32) / 3;
            if brightness > 160 {
                pixel[0] = 0;
                pixel[1] = 0;
                pixel[2] = 0;
            } else {
                pixel[0] = 255;
                pixel[1] = 255;
                pixel[2] = 255;
            }
        }
        cropped
    }

    fn check_star(img: &image::RgbImage, pos: (f64, f64)) -> bool {
        let scale = img.width() as f64 / 1920.0;
        let px = (pos.0 * scale).round() as u32;
        let py = (pos.1 * scale).round() as u32;
        if px < img.width() && py < img.height() {
            let p = img.get_pixel(px, py);
            p[0] > 150 && p[1] > 100 && p[2] < 100
        } else {
            false
        }
    }

    fn process_image(
        img: &image::RgbImage,
        ocr: &dyn yas::ocr::ImageToText<image::RgbImage>,
        mappings: &MappingManager,
        label: &str,
    ) {
        println!("\n=== {} ({}x{}) ===", label, img.width(), img.height());

        // Rarity
        let rarity = if check_star(img, STAR5_POS) {
            5
        } else if check_star(img, STAR4_POS) {
            4
        } else {
            3
        };
        println!("rarity: {}*", rarity);

        // Level
        let level_img = crop_binarize(img, LEVEL);
        let level_text = ocr.image_to_text(&level_img, false).unwrap_or_default();
        let level_text = level_text.trim().to_string();
        let digits: String = level_text.chars().filter(|c| c.is_ascii_digit()).collect();
        let level: i32 = if digits.is_empty() {
            -1
        } else {
            digits.parse().unwrap_or(-1)
        };
        println!("level: OCR='{}' parsed={}", level_text, level);

        // Main stat
        let main_img = crop_binarize(img, MAIN_STAT);
        let main_text = ocr.image_to_text(&main_img, false).unwrap_or_default();
        let main_text = main_text.trim().to_string();
        if let Some(parsed) = stat_parser::parse_stat_from_text(&main_text) {
            let key = stat_parser::main_stat_key_fixup(&parsed.key);
            println!("main: OCR='{}' => key='{}'", main_text, key);
        } else {
            println!("main: OCR='{}' => (parse failed)", main_text);
        }

        // Substats
        let mut sub_candidates: Vec<Vec<OcrCandidate>> = Vec::new();
        let mut parsed_count = 0usize;
        for (i, rect) in SUBS.iter().enumerate() {
            let sub_img = crop_binarize(img, *rect);
            let text = ocr.image_to_text(&sub_img, false).unwrap_or_default();
            let text = text.trim().to_string();
            if text.is_empty() || text.contains("件套") {
                println!(
                    "sub{}: {}",
                    i,
                    if text.contains("件套") {
                        format!("stop marker ({})", text)
                    } else {
                        "(empty)".to_string()
                    }
                );
                break;
            }
            if let Some(parsed) = stat_parser::parse_stat_from_text(&text) {
                println!(
                    "sub{}: OCR='{}' => key='{}' val={} inactive={}",
                    i, text, parsed.key, parsed.value, parsed.inactive
                );
                sub_candidates.push(vec![OcrCandidate {
                    key: parsed.key,
                    value: parsed.value,
                    inactive: parsed.inactive,
                }]);
                parsed_count += 1;
            } else {
                // Parse failed — likely set name bleeding into sub line; stop here
                println!("sub{}: OCR='{}' => (parse failed, stopping)", i, text);
                break;
            }
        }

        // Solver
        if level >= 0 && rarity >= 4 && parsed_count > 0 {
            let input = SolverInput {
                rarity,
                level_candidates: vec![level],
                substat_candidates: sub_candidates,
            };
            match roll_solver::solve(&input) {
                Some(result) => {
                    println!(
                        "solver: OK total_rolls={} init={}",
                        result.total_rolls, result.initial_substat_count
                    );
                    for s in &result.substats {
                        println!(
                            "  solved: {}={} rolls={} inactive={}",
                            s.key, s.value, s.roll_count, s.inactive
                        );
                    }
                },
                None => println!("solver: FAILED"),
            }
        }

        // Set name (adjust Y for missing subs)
        let missing = 4usize.saturating_sub(parsed_count);
        let set_rect = (
            SET_NAME.0,
            SET_NAME.1 - missing as f64 * SUB_SPACING,
            SET_NAME.2,
            SET_NAME.3,
        );
        let set_img = crop_binarize(img, set_rect);
        let set_text = ocr.image_to_text(&set_img, false).unwrap_or_default();
        let set_text = set_text.trim().to_string();
        let cleaned = set_text
            .trim_end_matches('：')
            .trim_end_matches(':')
            .trim_end_matches('；')
            .trim_end_matches(';')
            .trim();
        if let Some(set_key) = fuzzy_match_map(cleaned, &mappings.artifact_set_map) {
            println!(
                "set: OCR='{}' => '{}' (y_adj=-{})",
                set_text,
                set_key,
                missing as f64 * SUB_SPACING
            );
        } else {
            println!(
                "set: OCR='{}' => (no match) (y_adj=-{})",
                set_text,
                missing as f64 * SUB_SPACING
            );
        }
    }

    println!("Loading models...");
    let v4 = create_ocr_model("ppocrv4")?;
    println!("Loading mappings...");
    let mappings = MappingManager::new(&NameOverrides::default())?;

    let p = std::path::Path::new(path);
    if p.is_dir() {
        // Process all .png files in directory
        let mut files: Vec<_> = std::fs::read_dir(p)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "png").unwrap_or(false))
            .collect();
        files.sort_by_key(|e| e.file_name());
        println!("Found {} images in {}", files.len(), path);
        for entry in &files {
            let img = image::open(entry.path())?.to_rgb8();
            process_image(&img, &*v4, &mappings, &entry.file_name().to_string_lossy());
        }
    } else {
        let img = image::open(p)?.to_rgb8();
        process_image(&img, &*v4, &mappings, path);
    }

    Ok(())
}

/// OCR each screenshot, build a GoodArtifact, and verify against ground truth export.
fn run_sel_verify(dir: &str, json_path: &str) -> Result<()> {
    use genshin_scanner::scanner::common::models::{GoodArtifact, GoodExport, GoodSubStat};
    use genshin_scanner::scanner::common::roll_solver::{self, OcrCandidate, SolverInput};
    use genshin_scanner::scanner::common::stat_parser;

    // Selection view crop regions (base 1920x1080)
    const LEVEL: (f64, f64, f64, f64) = (1443.0, 310.0, 100.0, 26.0);
    const STAR4_POS: (f64, f64) = (1578.0, 280.0);
    const STAR5_POS: (f64, f64) = (1611.0, 280.0);
    const SUBS: [(f64, f64, f64, f64); 4] = [
        (1460.0, 349.0, 256.0, 30.0),
        (1460.0, 383.0, 256.0, 30.0),
        (1460.0, 417.0, 256.0, 30.0),
        (1460.0, 451.0, 336.0, 30.0),
    ];
    const SUB_SPACING: f64 = 34.0;
    const SET_NAME: (f64, f64, f64, f64) = (1430.0, 489.0, 300.0, 30.0);
    const VALUE_TOLERANCE: f64 = 0.100001;

    fn crop_binarize(img: &image::RgbImage, rect: (f64, f64, f64, f64)) -> image::RgbImage {
        let scale = img.width() as f64 / 1920.0;
        let x = (rect.0 * scale).round() as u32;
        let y = (rect.1 * scale).round() as u32;
        let w = (rect.2 * scale).round().min((img.width() - x) as f64) as u32;
        let h = (rect.3 * scale).round().min((img.height() - y) as f64) as u32;
        let mut cropped = image::imageops::crop_imm(img, x, y, w, h).to_image();
        for pixel in cropped.pixels_mut() {
            let brightness = (pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32) / 3;
            if brightness > 160 {
                pixel[0] = 0;
                pixel[1] = 0;
                pixel[2] = 0;
            } else {
                pixel[0] = 255;
                pixel[1] = 255;
                pixel[2] = 255;
            }
        }
        cropped
    }

    fn check_star(img: &image::RgbImage, pos: (f64, f64)) -> bool {
        let scale = img.width() as f64 / 1920.0;
        let px = (pos.0 * scale).round() as u32;
        let py = (pos.1 * scale).round() as u32;
        if px < img.width() && py < img.height() {
            let p = img.get_pixel(px, py);
            p[0] > 150 && p[1] > 100 && p[2] < 100
        } else {
            false
        }
    }

    // Load GT
    println!("Loading ground truth from {}...", json_path);
    let json_str = std::fs::read_to_string(json_path)?;
    let export: GoodExport = serde_json::from_str(&json_str)?;
    let gt_artifacts = export.artifacts.unwrap_or_default();
    println!("Ground truth: {} artifacts", gt_artifacts.len());

    println!("Loading models...");
    let v4 = create_ocr_model("ppocrv4")?;
    println!("Loading mappings...");
    let mappings = MappingManager::new(&NameOverrides::default())?;

    // Collect images
    let p = std::path::Path::new(dir);
    let mut files: Vec<_> = std::fs::read_dir(p)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "png").unwrap_or(false))
        .collect();
    files.sort_by_key(|e| e.file_name());
    println!("Found {} images\n", files.len());

    let mut matched = 0;
    let mut unmatched = 0;
    let mut skipped = 0; // empty cells, duplicate clicks on same item

    for entry in &files {
        let label = entry.file_name().to_string_lossy().to_string();
        let img = image::open(entry.path())?.to_rgb8();

        // Rarity
        let rarity = if check_star(&img, STAR5_POS) {
            5
        } else if check_star(&img, STAR4_POS) {
            4
        } else {
            3
        };

        // Level
        let level_img = crop_binarize(&img, LEVEL);
        let level_text = v4.image_to_text(&level_img, false).unwrap_or_default();
        let digits: String = level_text
            .trim()
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        let level: i32 = if digits.is_empty() {
            -1
        } else {
            digits.parse().unwrap_or(-1)
        };

        if level < 0 {
            println!(
                "[SKIP] {} — level parse failed ('{}')",
                label,
                level_text.trim()
            );
            skipped += 1;
            continue;
        }

        // Substats
        let mut sub_candidates: Vec<Vec<OcrCandidate>> = Vec::new();
        let mut parsed_subs: Vec<(String, f64, bool)> = Vec::new();
        for rect in &SUBS {
            let sub_img = crop_binarize(&img, *rect);
            let text = v4.image_to_text(&sub_img, false).unwrap_or_default();
            let text = text.trim().to_string();
            if text.is_empty() || text.contains("件套") {
                break;
            }
            if let Some(parsed) = stat_parser::parse_stat_from_text(&text) {
                sub_candidates.push(vec![OcrCandidate {
                    key: parsed.key.clone(),
                    value: parsed.value,
                    inactive: parsed.inactive,
                }]);
                parsed_subs.push((parsed.key, parsed.value, parsed.inactive));
            } else {
                break; // set name bleeding in
            }
        }

        // Solver
        let solved = if rarity >= 4 && !sub_candidates.is_empty() {
            let input = SolverInput {
                rarity,
                level_candidates: vec![level],
                substat_candidates: sub_candidates,
            };
            roll_solver::solve(&input)
        } else {
            None
        };

        // Build substats from solver result (more accurate) or raw OCR
        let (active_subs, inactive_subs) = if let Some(ref result) = solved {
            let mut active = Vec::new();
            let mut inactive = Vec::new();
            for s in &result.substats {
                let sub = GoodSubStat {
                    key: s.key.clone(),
                    value: s.value,
                    initial_value: None,
                    rolls: vec![],
                };
                if s.inactive {
                    inactive.push(sub);
                } else {
                    active.push(sub);
                }
            }
            (active, inactive)
        } else {
            let mut active = Vec::new();
            let mut inactive = Vec::new();
            for (key, value, is_inactive) in &parsed_subs {
                let sub = GoodSubStat {
                    key: key.clone(),
                    value: *value,
                    initial_value: None,
                    rolls: vec![],
                };
                if *is_inactive {
                    inactive.push(sub);
                } else {
                    active.push(sub);
                }
            }
            (active, inactive)
        };

        // Set name (adjust Y for sub count)
        let parsed_count = parsed_subs.len();
        let missing = 4usize.saturating_sub(parsed_count);
        let set_rect = (
            SET_NAME.0,
            SET_NAME.1 - missing as f64 * SUB_SPACING,
            SET_NAME.2,
            SET_NAME.3,
        );
        let set_img = crop_binarize(&img, set_rect);
        let set_text = v4.image_to_text(&set_img, false).unwrap_or_default();
        let cleaned = set_text
            .trim()
            .trim_end_matches('：')
            .trim_end_matches(':')
            .trim_end_matches('；')
            .trim_end_matches(';')
            .trim();
        let set_key = fuzzy_match_map(cleaned, &mappings.artifact_set_map).unwrap_or_default();

        if set_key.is_empty() {
            println!(
                "[SKIP] {} — set name not matched (OCR: '{}')",
                label,
                set_text.trim()
            );
            skipped += 1;
            continue;
        }

        // Extract slot from filename (e.g. "flower_r0_c1.png" → "flower")
        let slot_key = label.split('_').next().unwrap_or("").to_string();

        // Main stat — apply slot-aware fixup (flower=hp, plume=atk, others=percentage)
        let main_rect = (1440.0, 217.0, 250.0, 30.0);
        let main_img = crop_binarize(&img, main_rect);
        let main_text = v4.image_to_text(&main_img, false).unwrap_or_default();
        let main_key = stat_parser::parse_stat_from_text(main_text.trim())
            .map(|p| {
                match slot_key.as_str() {
                    "flower" => "hp".to_string(), // always flat HP
                    "plume" => "atk".to_string(), // always flat ATK
                    _ => stat_parser::main_stat_key_fixup(&p.key),
                }
            })
            .unwrap_or_default();

        // Build scanned artifact (ignore elixir/location/lock)
        let scanned = GoodArtifact {
            set_key: set_key.clone(),
            slot_key: slot_key.clone(),
            rarity,
            level,
            main_stat_key: main_key.clone(),
            substats: active_subs,
            unactivated_substats: inactive_subs,
            location: String::new(),
            lock: false,
            astral_mark: false,
            elixir_crafted: false,
            total_rolls: None,
        };

        // Find match in GT — relaxed: ignore slot, location, lock, elixir, astral
        let found = gt_artifacts.iter().any(|gt| {
            gt.set_key == scanned.set_key
                && gt.rarity == scanned.rarity
                && gt.level == scanned.level
                && gt.main_stat_key == scanned.main_stat_key
                && gt.substats.len() == scanned.substats.len()
                && gt.unactivated_substats.len() == scanned.unactivated_substats.len()
                && scanned.substats.iter().all(|ss| {
                    gt.substats
                        .iter()
                        .any(|gs| gs.key == ss.key && (gs.value - ss.value).abs() < VALUE_TOLERANCE)
                })
                && scanned.unactivated_substats.iter().all(|ss| {
                    gt.unactivated_substats
                        .iter()
                        .any(|gs| gs.key == ss.key && (gs.value - ss.value).abs() < VALUE_TOLERANCE)
                })
        });

        if found {
            println!(
                "[MATCH] {} — {}* lv{} {} {} ({} subs)",
                label, rarity, level, set_key, main_key, parsed_count
            );
            matched += 1;
        } else {
            println!(
                "[MISS]  {} — {}* lv{} {} {} ({} subs)",
                label, rarity, level, set_key, main_key, parsed_count
            );
            for s in &scanned.substats {
                println!("          sub: {}={}", s.key, s.value);
            }
            for s in &scanned.unactivated_substats {
                println!("          unsub: {}={}", s.key, s.value);
            }
            if solved.is_none() {
                println!("          (solver failed)");
            }
            unmatched += 1;
        }
    }

    println!("\n========================================");
    println!(
        "Results: {} matched, {} missed, {} skipped",
        matched, unmatched, skipped
    );
    println!(
        "Match rate: {:.1}% ({}/{})",
        if matched + unmatched > 0 {
            matched as f64 / (matched + unmatched) as f64 * 100.0
        } else {
            0.0
        },
        matched,
        matched + unmatched
    );
    println!("========================================");

    Ok(())
}

fn run_reprocess(images_dir: &str, output_path: Option<&str>) -> Result<()> {
    use genshin_scanner::scanner::artifact::{
        ArtifactOcrRegions, ArtifactScanResult, GoodArtifactScanner, GoodArtifactScannerConfig,
    };
    use genshin_scanner::scanner::common::coord_scaler::CoordScaler;
    use genshin_scanner::scanner::common::models::GoodExport;

    // Discover NNNN/full.png entries
    let mut entries: Vec<(usize, std::path::PathBuf)> = Vec::new();
    let artifacts_dir = std::path::Path::new(images_dir);
    if !artifacts_dir.is_dir() {
        anyhow::bail!("{} is not a directory", images_dir);
    }
    for entry in std::fs::read_dir(artifacts_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Ok(idx) = name.parse::<usize>() {
            let full_path = entry.path().join("full.png");
            if full_path.exists() {
                entries.push((idx, full_path));
            }
        }
    }
    entries.sort_by_key(|(idx, _)| *idx);
    eprintln!("Found {} artifact images in {}", entries.len(), images_dir);

    // Load models + mappings
    eprintln!("Loading OCR models...");
    let v4 = create_ocr_model("ppocrv4")?;
    let v5 = create_ocr_model("ppocrv5")?;
    eprintln!("Loading mappings...");
    let mappings = MappingManager::new(&NameOverrides::default())?;

    let regions = ArtifactOcrRegions::new();
    let config = GoodArtifactScannerConfig {
        continue_on_failure: true,
        dump_images: false,
        verbose: true,
        min_rarity: 1,
        ..Default::default()
    };

    let mut artifacts = Vec::new();
    let mut errors = 0;

    for (idx, path) in &entries {
        let img = image::open(path)?.to_rgb8();
        let scaler = CoordScaler::new(img.width(), img.height());
        let frame = CaptureFrame::full(img);

        match GoodArtifactScanner::scan_single_artifact(
            &*v5, &*v4, &frame, &scaler, &regions, &mappings, &config, *idx, None,
        ) {
            Ok(ArtifactScanResult::Artifact(artifact)) => {
                artifacts.push(artifact);
            },
            Ok(ArtifactScanResult::Stop) => {
                eprintln!("[{:04}] low rarity, skipped", idx);
            },
            Ok(ArtifactScanResult::Skip | ArtifactScanResult::EnhancementMaterial) => {
                eprintln!("[{:04}] skipped", idx);
            },
            Err(e) => {
                eprintln!("[{:04}] ERROR: {}", idx, e);
                errors += 1;
            },
        }
    }

    eprintln!(
        "Reprocessed: {} artifacts, {} errors",
        artifacts.len(),
        errors
    );

    let export = GoodExport::new(None, None, Some(artifacts));
    let json = serde_json::to_string_pretty(&export)?;

    if let Some(out) = output_path {
        std::fs::write(out, &json)?;
        eprintln!("Written to {}", out);
    } else {
        println!("{}", json);
    }

    Ok(())
}

/// Parse a character name from OCR text using the same logic as the scanner.
/// Extracts the name part after "/" and fuzzy matches against the character map.
fn parse_char_name(
    text: &str,
    char_map: &std::collections::HashMap<String, String>,
) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let slash_char = if text.contains('/') {
        Some('/')
    } else if text.contains('\u{FF0F}') {
        Some('\u{FF0F}')
    } else {
        None
    };
    if let Some(slash) = slash_char {
        let idx = text.find(slash).unwrap();
        let raw_name: String = text[idx + slash.len_utf8()..]
            .chars()
            .filter(|c| {
                matches!(*c, '\u{4E00}'..='\u{9FFF}' | '\u{300C}' | '\u{300D}' | 'a'..='z' | 'A'..='Z' | '0'..='9')
            })
            .collect();
        fuzzy_match_map(&raw_name, char_map)
    } else {
        fuzzy_match_map(text, char_map)
    }
}

fn run_char_name_test(image_path: &str) -> Result<()> {
    let img = image::open(image_path)?.to_rgb8();
    println!("Image: {} ({}x{})", image_path, img.width(), img.height());

    println!("Loading mappings...");
    let mappings = MappingManager::new(&NameOverrides::default())?;

    println!("Loading models...");
    let v4 = create_ocr_model("ppocrv4")?;
    let v5 = create_ocr_model("ppocrv5")?;

    let text_v4 = v4.image_to_text(&img, false)?;
    let text_v5 = v5.image_to_text(&img, false)?;

    let name_v4 = parse_char_name(&text_v4, &mappings.character_name_map);
    let name_v5 = parse_char_name(&text_v5, &mappings.character_name_map);

    println!();
    println!("v4 OCR:   {:?}", text_v4);
    println!("v4 match: {:?}", name_v4);
    println!();
    println!("v5 OCR:   {:?}", text_v5);
    println!("v5 match: {:?}", name_v5);

    println!();
    println!("=== Combined (v4 → v5 fallback) ===");
    if let Some(ref n) = name_v4 {
        println!("v4 matched: {}", n);
    } else if let Some(ref n) = name_v5 {
        println!("v4 failed, v5 fallback matched: {}", n);
    } else {
        println!("BOTH FAILED");
    }

    Ok(())
}

fn run_eval_ocr(debug_dir: &str) -> Result<()> {
    use std::path::Path;

    let base = Path::new(debug_dir);

    println!("Loading models...");
    let v4 = create_ocr_model("ppocrv4")?;
    let v5 = create_ocr_model("ppocrv5")?;
    println!("Loading mappings...");
    let mappings = MappingManager::new(&NameOverrides::default())?;

    // Evaluate character names
    let char_dir = base.join("characters");
    if char_dir.is_dir() {
        println!();
        println!("=== Character Name OCR (v4 vs v5) ===");
        let mut entries: Vec<(usize, std::path::PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&char_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(idx) = name.parse::<usize>() {
                let path = entry.path().join("name.png");
                if path.exists() {
                    entries.push((idx, path));
                }
            }
        }
        entries.sort_by_key(|(idx, _)| *idx);

        let mut v4_ok = 0;
        let mut v5_ok = 0;
        let mut both_fail = 0;
        let mut disagree = Vec::new();

        for (idx, path) in &entries {
            let img = image::open(path)?.to_rgb8();
            let text_v4 = v4.image_to_text(&img, false)?;
            let text_v5 = v5.image_to_text(&img, false)?;
            let name_v4 = parse_char_name(&text_v4, &mappings.character_name_map);
            let name_v5 = parse_char_name(&text_v5, &mappings.character_name_map);

            let v4_matched = name_v4.is_some();
            let v5_matched = name_v5.is_some();
            if v4_matched {
                v4_ok += 1;
            }
            if v5_matched {
                v5_ok += 1;
            }
            if !v4_matched && !v5_matched {
                both_fail += 1;
            }

            if name_v4 != name_v5 {
                disagree.push(format!(
                    "  [{:04}] v4={:?} (OCR: {:?})  v5={:?} (OCR: {:?})",
                    idx,
                    name_v4.as_deref().unwrap_or("FAIL"),
                    text_v4.trim(),
                    name_v5.as_deref().unwrap_or("FAIL"),
                    text_v5.trim(),
                ));
            }
        }

        println!("Total: {} characters", entries.len());
        println!(
            "v4 matched: {}/{} ({:.1}%)",
            v4_ok,
            entries.len(),
            v4_ok as f64 / entries.len() as f64 * 100.0
        );
        println!(
            "v5 matched: {}/{} ({:.1}%)",
            v5_ok,
            entries.len(),
            v5_ok as f64 / entries.len() as f64 * 100.0
        );
        println!("Both failed: {}", both_fail);
        if !disagree.is_empty() {
            println!("Disagreements ({}):", disagree.len());
            for line in &disagree {
                println!("{}", line);
            }
        }
    }

    // Evaluate weapon equip
    let weapon_dir = base.join("weapons");
    if weapon_dir.is_dir() {
        println!();
        println!("=== Weapon Equip OCR (v4 vs v5) ===");
        let mut entries: Vec<(usize, std::path::PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&weapon_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(idx) = name.parse::<usize>() {
                let path = entry.path().join("equip.png");
                if path.exists() {
                    entries.push((idx, path));
                }
            }
        }
        entries.sort_by_key(|(idx, _)| *idx);

        let mut v4_ok = 0;
        let mut v5_ok = 0;
        let mut both_fail = 0;
        let mut disagree = Vec::new();
        let mut has_text = 0;

        for (idx, path) in &entries {
            let img = image::open(path)?.to_rgb8();
            let text_v4 = v4.image_to_text(&img, false)?;
            let text_v5 = v5.image_to_text(&img, false)?;

            // Skip empty equip slots (no text at all)
            if text_v4.trim().is_empty() && text_v5.trim().is_empty() {
                continue;
            }
            has_text += 1;

            let loc_v4 = equip_parser::parse_equip_location(&text_v4, &mappings.character_name_map);
            let loc_v5 = equip_parser::parse_equip_location(&text_v5, &mappings.character_name_map);

            let v4_matched = !loc_v4.is_empty();
            let v5_matched = !loc_v5.is_empty();
            if v4_matched {
                v4_ok += 1;
            }
            if v5_matched {
                v5_ok += 1;
            }
            if !v4_matched && !v5_matched {
                both_fail += 1;
            }

            if loc_v4 != loc_v5 {
                disagree.push(format!(
                    "  [{:04}] v4={:?} (OCR: {:?})  v5={:?} (OCR: {:?})",
                    idx,
                    if v4_matched { &loc_v4 } else { "FAIL" },
                    text_v4.trim(),
                    if v5_matched { &loc_v5 } else { "FAIL" },
                    text_v5.trim(),
                ));
            }
        }

        println!("Total: {} weapons ({} equipped)", entries.len(), has_text);
        if has_text > 0 {
            println!(
                "v4 matched: {}/{} ({:.1}%)",
                v4_ok,
                has_text,
                v4_ok as f64 / has_text as f64 * 100.0
            );
            println!(
                "v5 matched: {}/{} ({:.1}%)",
                v5_ok,
                has_text,
                v5_ok as f64 / has_text as f64 * 100.0
            );
            println!("Both failed: {}", both_fail);
        }
        if !disagree.is_empty() {
            println!("Disagreements ({}):", disagree.len());
            for line in &disagree {
                println!("{}", line);
            }
        }
    }

    // Evaluate artifact equip
    let artifact_dir = base.join("artifacts");
    if artifact_dir.is_dir() {
        println!();
        println!("=== Artifact Equip OCR (v4 vs v5) ===");
        let mut entries: Vec<(usize, std::path::PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&artifact_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(idx) = name.parse::<usize>() {
                let path = entry.path().join("equip.png");
                if path.exists() {
                    entries.push((idx, path));
                }
            }
        }
        entries.sort_by_key(|(idx, _)| *idx);

        let mut v4_ok = 0;
        let mut v5_ok = 0;
        let mut both_fail = 0;
        let mut disagree = Vec::new();
        let mut has_text = 0;

        for (idx, path) in &entries {
            let img = image::open(path)?.to_rgb8();
            let text_v4 = v4.image_to_text(&img, false)?;
            let text_v5 = v5.image_to_text(&img, false)?;

            if text_v4.trim().is_empty() && text_v5.trim().is_empty() {
                continue;
            }
            has_text += 1;

            let loc_v4 = equip_parser::parse_equip_location(&text_v4, &mappings.character_name_map);
            let loc_v5 = equip_parser::parse_equip_location(&text_v5, &mappings.character_name_map);

            let v4_matched = !loc_v4.is_empty();
            let v5_matched = !loc_v5.is_empty();
            if v4_matched {
                v4_ok += 1;
            }
            if v5_matched {
                v5_ok += 1;
            }
            if !v4_matched && !v5_matched {
                both_fail += 1;
            }

            if loc_v4 != loc_v5 {
                disagree.push(format!(
                    "  [{:04}] v4={:?} (OCR: {:?})  v5={:?} (OCR: {:?})",
                    idx,
                    if v4_matched { &loc_v4 } else { "FAIL" },
                    text_v4.trim(),
                    if v5_matched { &loc_v5 } else { "FAIL" },
                    text_v5.trim(),
                ));
            }
        }

        println!("Total: {} artifacts ({} equipped)", entries.len(), has_text);
        if has_text > 0 {
            println!(
                "v4 matched: {}/{} ({:.1}%)",
                v4_ok,
                has_text,
                v4_ok as f64 / has_text as f64 * 100.0
            );
            println!(
                "v5 matched: {}/{} ({:.1}%)",
                v5_ok,
                has_text,
                v5_ok as f64 / has_text as f64 * 100.0
            );
            println!("Both failed: {}", both_fail);
        }
        if !disagree.is_empty() {
            println!("Disagreements ({}):", disagree.len());
            for line in &disagree {
                println!("{}", line);
            }
        }
    }

    Ok(())
}

fn run_equip_test(image_path: &str) -> Result<()> {
    let img = image::open(image_path)?.to_rgb8();
    println!("Image: {} ({}x{})", image_path, img.width(), img.height());

    // Load mappings + models
    println!("Loading mappings...");
    let mappings = MappingManager::new(&NameOverrides::default())?;
    println!("  {} characters loaded", mappings.character_name_map.len());

    println!("Loading models...");
    let v4 = create_ocr_model("ppocrv4")?;
    let v5 = create_ocr_model("ppocrv5")?;

    // OCR with both engines
    let text_v4 = v4.image_to_text(&img, false)?;
    let text_v5 = v5.image_to_text(&img, false)?;

    println!();
    println!("=== Equip Pipeline Test ===");
    println!();

    // v4 path
    let loc_v4 = equip_parser::parse_equip_location(&text_v4, &mappings.character_name_map);
    println!("v4 OCR:   {:?}", text_v4);
    println!(
        "v4 match: {:?}",
        if loc_v4.is_empty() {
            "(empty)"
        } else {
            &loc_v4
        }
    );

    // v5 path
    let loc_v5 = equip_parser::parse_equip_location(&text_v5, &mappings.character_name_map);
    println!();
    println!("v5 OCR:   {:?}", text_v5);
    println!(
        "v5 match: {:?}",
        if loc_v5.is_empty() {
            "(empty)"
        } else {
            &loc_v5
        }
    );

    // Combined path (v4 primary, v5 fallback — as scanner does it)
    println!();
    println!("=== Combined (v4 → v5 fallback) ===");
    let final_loc = if !loc_v4.is_empty() {
        println!("v4 matched: {}", loc_v4);
        loc_v4
    } else if !loc_v5.is_empty() {
        println!("v4 failed, v5 fallback matched: {}", loc_v5);
        loc_v5
    } else {
        println!("BOTH FAILED");
        println!("  v4 raw: {:?}", text_v4);
        println!("  v5 raw: {:?}", text_v5);
        String::new()
    };

    if !final_loc.is_empty() {
        println!("Result: {}", final_loc);
    }

    Ok(())
}
