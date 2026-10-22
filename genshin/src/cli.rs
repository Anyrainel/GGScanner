use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{command, ArgMatches, Args, FromArgMatches};
use serde::{Deserialize, Serialize};
use yas::{log_debug, log_error, log_info, log_warn};

use yas::capture::constants::DEFAULT_HDR_WHITE_POINT;
use yas::capture::CaptureMethod;
use yas::game_info::{GameInfo, GameInfoBuilder};

use crate::scanner::achievement::{
    GoodAchievementScannerConfig, DEFAULT_CATEGORY_DELAY as DEFAULT_ACHIEVEMENT_CATEGORY_DELAY,
    DEFAULT_OPEN_DELAY as DEFAULT_ACHIEVEMENT_OPEN_DELAY,
    DEFAULT_SCROLL_DELAY as DEFAULT_ACHIEVEMENT_SCROLL_DELAY,
};
use crate::scanner::artifact::GoodArtifactScannerConfig;
use crate::scanner::character::GoodCharacterScannerConfig;
use crate::scanner::common::constants::*;
use crate::scanner::common::game_controller::GenshinGameController;
use crate::scanner::common::mappings::{MappingManager, NameOverrides};
use crate::scanner::common::models::GoodExport;
use crate::scanner::common::ocr_pool::{
    OcrBackends, OcrPoolConfig, SharedOcrPools, DEFAULT_ARTIFACT_OCR, DEFAULT_CHARACTER_OCR,
    DEFAULT_WEAPON_OCR,
};
use crate::scanner::common::scan_runner::{
    run_scan_phases, ScanFailurePolicy, ScanPhaseResult, ScanRunOptions,
};
use crate::scanner::weapon::GoodWeaponScannerConfig;

/// Config file path relative to the executable directory.
const CONFIG_FILE_REL: &str = "data/good_config.json";

/// Get the full path to the config file.
pub fn config_path() -> PathBuf {
    exe_dir().join(CONFIG_FILE_REL)
}

/// Get the directory containing the running executable.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

// ================================================================
// ONNX Runtime auto-download
// ================================================================

/// The DLL name that `ort` loads at runtime.
#[cfg(target_os = "windows")]
const ORT_DLL_NAME: &str = "onnxruntime.dll";

/// Minimum expected size for onnxruntime.dll (v1.22.0 x64 is ~12.6 MB).
/// Anything smaller is almost certainly a corrupted or partial download.
#[cfg(target_os = "windows")]
const ORT_DLL_MIN_SIZE: u64 = 5 * 1024 * 1024; // 5 MB

/// VC++ runtime DLLs required by onnxruntime.dll (from VS 2015-2022 Redistributable x64).
#[cfg(target_os = "windows")]
const VCPP_REQUIRED_DLLS: &[&str] = &[
    "VCRUNTIME140.dll",
    "VCRUNTIME140_1.dll",
    "MSVCP140.dll",
    "MSVCP140_1.dll",
];

/// Check if the Visual C++ 2015-2022 Redistributable (x64) is installed by
/// verifying that all required runtime DLLs can be loaded.
///
/// Returns `Ok(())` if all DLLs are present, or `Err` with a user-facing
/// message listing the missing DLLs and a download link.
#[cfg(target_os = "windows")]
pub fn check_vcpp_runtime() -> Result<()> {
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryExW;
    use windows_sys::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32;

    let mut missing = Vec::new();
    for &dll_name in VCPP_REQUIRED_DLLS {
        let wide: Vec<u16> = dll_name.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if handle.is_null() {
            missing.push(dll_name);
        } else {
            unsafe { windows_sys::Win32::Foundation::FreeLibrary(handle) };
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    const VCPP_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";
    let missing_list = missing.join(", ");
    log_error!(
        "缺少 Visual C++ 运行库: {}",
        "Missing Visual C++ runtime DLLs: {}",
        missing_list
    );
    log_error!(
        "请安装 VC++ 2015-2022 Redistributable (x64): {}",
        "Install VC++ 2015-2022 Redistributable (x64): {}",
        VCPP_URL
    );
    Err(anyhow!(
        "缺少 Visual C++ 运行库 ({})，OCR引擎无法加载。\n\
         请下载安装: {}\n\
         / Missing Visual C++ runtime ({}). The OCR engine cannot load.\n\
         Please install from: {}",
        missing_list,
        VCPP_URL,
        missing_list,
        VCPP_URL,
    ))
}

/// Check if ONNX Runtime is available. Returns true if found, false if download needed.
/// Also detects corrupted/truncated files by checking the DLL size.
#[cfg(target_os = "windows")]
pub fn check_onnxruntime() -> bool {
    let dll_path = exe_dir().join(ORT_DLL_NAME);
    match std::fs::metadata(&dll_path) {
        Ok(meta) if meta.len() >= ORT_DLL_MIN_SIZE => {
            std::env::set_var("ORT_DYLIB_PATH", &dll_path);
            true
        },
        Ok(meta) => {
            log_warn!(
                "onnxruntime.dll 文件异常（{} 字节），将重新下载",
                "onnxruntime.dll looks corrupted ({} bytes), will re-download",
                meta.len()
            );
            // Remove the bad file so download_onnxruntime_inner can write fresh
            if let Err(error) = std::fs::remove_file(&dll_path) {
                log_warn!(
                    "无法删除损坏的 OCR 运行库文件；重新下载可能失败。完整错误详情: {:#}",
                    "The damaged OCR runtime file could not be removed; the replacement download may fail. Full error details: {:#}",
                    error,
                );
            }
            false
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            log_warn!(
                "无法检查 OCR 运行库文件；程序将尝试重新下载。完整错误详情: {:#}",
                "The OCR runtime file could not be inspected; the application will try to download it again. Full error details: {:#}",
                error,
            );
            false
        },
    }
}

/// Download ONNX Runtime without interactive prompts (for GUI mode).
#[cfg(target_os = "windows")]
pub fn download_onnxruntime() -> Result<()> {
    let dll_path = exe_dir().join(ORT_DLL_NAME);
    log_info!("正在下载 ONNX Runtime...", "Downloading ONNX Runtime...");
    download_onnxruntime_inner(&dll_path)
}

/// Both platforms use the same cancellable race and validate before installation.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn download_onnxruntime_inner(path: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    let bytes =
        crate::runtime_download::fetch(&crate::runtime_download::WINDOWS, extract_onnxruntime_dll)?;
    #[cfg(target_os = "linux")]
    let bytes =
        crate::runtime_download::fetch(&crate::runtime_download::LINUX, extract_onnxruntime_so)?;

    let staged = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&staged, path)?;
        Ok(())
    })();
    if result.is_err() {
        if let Err(error) = crate::fs_utils::remove_file_if_exists(&staged) {
            log_warn!(
                "无法清理运行库临时文件: {:#}",
                "Cannot clean up runtime staging file: {:#}",
                error
            );
        }
    }
    result?;
    std::env::set_var("ORT_DYLIB_PATH", path);
    log_info!(
        "ONNX Runtime 已安装到: {}",
        "ONNX Runtime installed to: {}",
        path.display()
    );
    Ok(())
}

/// Ensure onnxruntime.dll is available next to the exe; if not, offer to download it.
///
/// When the DLL exists locally, sets `ORT_DYLIB_PATH` so `ort` uses our copy
/// instead of any older/incompatible system DLL that might be on PATH.
#[cfg(target_os = "windows")]
fn ensure_onnxruntime() -> Result<()> {
    check_vcpp_runtime()?;

    if check_onnxruntime() {
        return Ok(());
    }

    println!();
    println!("=======================================================");
    println!(
        "  {} {}",
        yas::lang::localize("未找到 / Not found:"),
        ORT_DLL_NAME
    );
    println!("=======================================================");
    println!();
    println!(
        "{}",
        yas::lang::localize(
            "OCR引擎需要ONNX Runtime运行库。 / The OCR engine requires the ONNX Runtime library."
        )
    );
    println!();
    println!("{}", yas::lang::localize("按回车自动下载运行库，或按 Ctrl+C 退出。 / Press Enter to download the runtime automatically, or Ctrl+C to exit."));
    let _ = std::io::stdin().read_line(&mut String::new());

    download_onnxruntime()
}

/// Extract onnxruntime.dll from the downloaded zip archive.
///
/// Supports two layouts:
/// - GitHub release zip: `onnxruntime-win-x64-*/lib/onnxruntime.dll`
/// - NuGet .nupkg: `runtimes/win-x64/native/onnxruntime.dll`
#[cfg(target_os = "windows")]
fn extract_onnxruntime_dll(zip_bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::{Cursor, Read};
    let reader = Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(reader).context("Cannot open ONNX zip archive")?;

    let suffixes = [
        "lib/onnxruntime.dll",                     // GitHub release zip
        "runtimes/win-x64/native/onnxruntime.dll", // NuGet .nupkg
    ];

    for i in 0..archive.len() {
        let file = archive.by_index(i).context("Cannot read ONNX zip entry")?;
        let name = file.name()?.into_owned();
        if suffixes.iter().any(|s| name.ends_with(s)) {
            let mut buf = Vec::new();
            file.take(40 * 1024 * 1024 + 1).read_to_end(&mut buf)?;
            anyhow::ensure!(
                buf.len() <= 40 * 1024 * 1024,
                "ONNX library exceeds size limit"
            );
            return Ok(buf);
        }
    }

    Err(anyhow!("onnxruntime.dll not found in zip archive"))
}

// ================================================================
// ONNX Runtime bootstrap (Linux) — detection order:
//   1. ORT_DYLIB_PATH env var (explicit user override)
//   2. libonnxruntime.so next to the exe (app-managed copy)
//   3. system-wide install (package manager)
//   4. prompt + auto-download from GitHub (with mirrors)
// ================================================================

#[cfg(target_os = "linux")]
const ORT_SO_NAME: &str = "libonnxruntime.so";

#[cfg(target_os = "linux")]
const ORT_SO_MIN_SIZE: u64 = 5 * 1024 * 1024;

#[cfg(all(test, target_os = "linux"))]
mod linux_runtime_tests {
    use super::*;

    #[test]
    #[ignore = "live Linux runtime race; no files are installed"]
    fn live_linux_runtime_race() {
        let start = std::time::Instant::now();
        let bytes =
            crate::runtime_download::fetch(&crate::runtime_download::LINUX, extract_onnxruntime_so)
                .unwrap();
        println!(
            "Concurrent Linux runtime download: {} verified bytes in {:?}",
            bytes.len(),
            start.elapsed()
        );
    }

    #[test]
    fn extracts_versioned_library_instead_of_empty_symlink() {
        let mut tar = tar::Builder::new(Vec::new());
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o755);
        link.set_link_name("libonnxruntime.so.1.22.0").unwrap();
        link.set_cksum();
        tar.append_data(&mut link, "onnxruntime/lib/libonnxruntime.so", &[][..])
            .unwrap();
        let library = vec![42; ORT_SO_MIN_SIZE as usize];
        let mut file = tar::Header::new_gnu();
        file.set_size(library.len() as u64);
        file.set_mode(0o755);
        file.set_cksum();
        tar.append_data(
            &mut file,
            "onnxruntime/lib/libonnxruntime.so.1.22.0",
            &library[..],
        )
        .unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gzip, &tar.into_inner().unwrap()).unwrap();
        let archive = gzip.finish().unwrap();
        let dest = std::env::temp_dir().join(format!("ort-extraction-{}.so", std::process::id()));
        std::fs::write(&dest, extract_onnxruntime_so(&archive).unwrap()).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), library);
        std::fs::remove_file(&dest).unwrap();
        assert!(extract_onnxruntime_so(&archive[..archive.len() / 2]).is_err());
        assert!(
            !dest.exists(),
            "truncated archives must not install a partial library"
        );
    }
}

/// Standard sonames probed via the dynamic loader for a package-manager
/// install (dlopen honors ldconfig and LD_LIBRARY_PATH, so no hardcoded
/// library directories are needed).
#[cfg(target_os = "linux")]
const ORT_SYSTEM_SONAMES: &[&str] = &[ORT_SO_NAME, "libonnxruntime.so.1"];

#[cfg(target_os = "linux")]
pub fn check_onnxruntime() -> bool {
    // 1. Explicit override wins.
    if let Some(path) = std::env::var_os("ORT_DYLIB_PATH") {
        let path = std::path::PathBuf::from(&path);
        if path.is_file() {
            log_info!(
                "使用 ORT_DYLIB_PATH 指定的 ONNX Runtime: {}",
                "Using ONNX Runtime from ORT_DYLIB_PATH: {}",
                path.display()
            );
            return true;
        }
        log_warn!(
            "ORT_DYLIB_PATH 已设置但文件不存在（{}），将尝试其他位置",
            "ORT_DYLIB_PATH is set but the file does not exist ({}); trying other locations",
            path.display()
        );
        std::env::remove_var("ORT_DYLIB_PATH");
    }

    // 2. App-managed copy next to the exe.
    let so_path = exe_dir().join(ORT_SO_NAME);
    match std::fs::metadata(&so_path) {
        Ok(meta) if meta.len() >= ORT_SO_MIN_SIZE => {
            std::env::set_var("ORT_DYLIB_PATH", &so_path);
            return true;
        },
        Ok(meta) => {
            log_warn!(
                "{} 文件异常（{} 字节），将重新下载",
                "{} looks corrupted ({} bytes), will re-download",
                ORT_SO_NAME,
                meta.len()
            );
            if let Err(error) = std::fs::remove_file(&so_path) {
                log_warn!(
                    "无法删除损坏的 OCR 运行库文件；重新下载可能失败。完整错误详情: {:#}",
                    "The damaged OCR runtime file could not be removed; the replacement download may fail. Full error details: {:#}",
                    error,
                );
            }
        },
        Err(_) => {},
    }

    // 3. System-wide install (e.g. distro package). Note: ort needs the
    //    matching API version; an incompatible system copy fails at OCR init
    //    with a clear ort version error, after which the user can re-run and
    //    let this tool download its own copy.
    for soname in ORT_SYSTEM_SONAMES {
        // Probe-only load; the library is dropped immediately and ort loads
        // it again through ORT_DYLIB_PATH.
        if unsafe { libloading::Library::new(soname) }.is_ok() {
            log_info!(
                "使用系统安装的 ONNX Runtime: {}",
                "Using system-installed ONNX Runtime: {}",
                soname,
            );
            std::env::set_var("ORT_DYLIB_PATH", soname);
            return true;
        }
    }

    false
}

/// Download ONNX Runtime without interactive prompts (for GUI mode).
#[cfg(target_os = "linux")]
pub fn download_onnxruntime() -> Result<()> {
    anyhow::ensure!(std::env::consts::ARCH == "x86_64", "Automatic ONNX Runtime download supports Linux x86_64 only; set ORT_DYLIB_PATH to a compatible 1.22 library on this architecture");
    let so_path = exe_dir().join(ORT_SO_NAME);
    log_info!("正在下载 ONNX Runtime...", "Downloading ONNX Runtime...");
    download_onnxruntime_inner(&so_path)
}

/// Ensure libonnxruntime.so is available; if not, offer to download it.
#[cfg(target_os = "linux")]
fn ensure_onnxruntime() -> Result<()> {
    if check_onnxruntime() {
        return Ok(());
    }

    println!();
    println!("=======================================================");
    println!(
        "  {} {}",
        yas::lang::localize("未找到 / Not found:"),
        ORT_SO_NAME
    );
    println!("=======================================================");
    println!();
    println!(
        "{}",
        yas::lang::localize(
            "OCR引擎需要ONNX Runtime运行库。 / The OCR engine requires the ONNX Runtime library."
        )
    );
    println!();
    println!(
        "{}",
        yas::lang::localize(
            "按回车自动下载（约25MB），或自行安装后重试，或按 Ctrl+C 退出。 / Press Enter to download automatically (~25MB), install it yourself and retry, or press Ctrl+C to exit."
        )
    );
    let _ = std::io::stdin().read_line(&mut String::new());

    download_onnxruntime()
}

/// Extract lib/libonnxruntime.so from the downloaded .tgz archive.
#[cfg(target_os = "linux")]
fn extract_onnxruntime_so(tgz_bytes: &[u8]) -> Result<Vec<u8>> {
    let reader = std::io::Cursor::new(tgz_bytes);
    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);

    for entry in archive
        .entries()
        .map_err(|e| anyhow!("Cannot read tar archive: {e}"))?
    {
        let entry = entry.map_err(|e| anyhow!("Cannot read tar entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| anyhow!("Cannot read entry path: {e}"))?
            .to_path_buf();
        if entry.header().entry_type().is_file()
            && path
                .file_name()
                .is_some_and(|n| n == "libonnxruntime.so.1.22.0")
            && path.parent().is_some_and(|p| p.ends_with("lib"))
        {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(
                &mut std::io::Read::take(entry, 40 * 1024 * 1024 + 1),
                &mut buf,
            )?;
            anyhow::ensure!(
                buf.len() <= 40 * 1024 * 1024,
                "ONNX library exceeds size limit"
            );
            anyhow::ensure!(
                buf.len() as u64 >= ORT_SO_MIN_SIZE,
                "ONNX Runtime library is truncated"
            );
            return Ok(buf);
        }
    }

    Err(anyhow!("libonnxruntime.so.1.22.0 not found in tar archive"))
}

// ================================================================
// Rayon thread pool with larger stack
// ================================================================
/// Initialize the global rayon thread pool with an 8 MB per-thread stack.
///
/// ONNX Runtime's C++ inference code uses deep call stacks that can overflow
/// rayon's default 1 MB thread stack on Windows, triggering 0xc0000409
/// (STATUS_STACK_BUFFER_OVERRUN / __fastfail).  Call this once before any
/// scan or server work.  Harmless if called multiple times — the second call
/// is a no-op (rayon ignores `build_global` after the pool is already built).
pub fn init_rayon_pool() {
    let _ = rayon::ThreadPoolBuilder::new()
        .stack_size(8 * 1024 * 1024)
        .build_global();
}

// ================================================================
// User config (good_config.json)
// ================================================================

fn default_char_tab_delay() -> u64 {
    DEFAULT_DELAY_CHAR_TAB_SWITCH
}
fn default_char_next_delay() -> u64 {
    DEFAULT_DELAY_CHAR_NEXT
}
fn default_open_delay() -> u64 {
    DEFAULT_DELAY_OPEN_SCREEN
}
fn default_close_delay() -> u64 {
    DEFAULT_DELAY_CLOSE_SCREEN
}
fn default_scroll_delay() -> u64 {
    DEFAULT_DELAY_SCROLL
}
fn default_tab_delay() -> u64 {
    DEFAULT_DELAY_INV_TAB_SWITCH
}
fn default_weapon_panel_delay() -> u64 {
    DEFAULT_WEAPON_PANEL_DELAY
}
fn default_artifact_initial_wait() -> u64 {
    DEFAULT_ARTIFACT_INITIAL_WAIT
}
fn default_artifact_panel_timeout() -> u64 {
    DEFAULT_ARTIFACT_PANEL_TIMEOUT
}
fn default_artifact_extra_delay() -> u64 {
    DEFAULT_ARTIFACT_EXTRA_DELAY
}
fn default_achievement_scroll_delay() -> u64 {
    DEFAULT_ACHIEVEMENT_SCROLL_DELAY
}
fn default_achievement_category_delay() -> u64 {
    DEFAULT_ACHIEVEMENT_CATEGORY_DELAY
}
fn default_achievement_open_delay() -> u64 {
    DEFAULT_ACHIEVEMENT_OPEN_DELAY
}

fn default_mgr_transition() -> u64 {
    1500
}
fn default_mgr_action() -> u64 {
    800
}
fn default_mgr_cell() -> u64 {
    100
}
fn default_mgr_scroll() -> u64 {
    400
}

fn default_true() -> bool {
    true
}
fn default_hdr_white_point() -> f32 {
    DEFAULT_HDR_WHITE_POINT
}
fn default_server_port() -> u16 {
    8765
}

/// Deserialize a u64 that may arrive as a number, a numeric string, or an
/// empty/invalid string.  Non-numeric values silently fall back to 0 so that
/// `#[serde(default = "…")]` can supply the real default.
fn deserialize_u64_lenient<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct U64LenientVisitor;

    impl<'de> de::Visitor<'de> for U64LenientVisitor {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a u64 or a numeric string")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<u64, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<u64, E> {
            Ok(v.max(0) as u64)
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<u64, E> {
            Ok(v.max(0.0) as u64)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<u64, E> {
            Ok(v.trim().parse::<u64>().unwrap_or(0))
        }

        fn visit_none<E: de::Error>(self) -> std::result::Result<u64, E> {
            Ok(0)
        }
        fn visit_unit<E: de::Error>(self) -> std::result::Result<u64, E> {
            Ok(0)
        }
    }

    deserializer.deserialize_any(U64LenientVisitor)
}

fn is_default_char_tab_delay(v: &u64) -> bool {
    *v == DEFAULT_DELAY_CHAR_TAB_SWITCH
}
fn is_default_char_next_delay(v: &u64) -> bool {
    *v == DEFAULT_DELAY_CHAR_NEXT
}
fn is_default_open_delay(v: &u64) -> bool {
    *v == DEFAULT_DELAY_OPEN_SCREEN
}
fn is_default_close_delay(v: &u64) -> bool {
    *v == DEFAULT_DELAY_CLOSE_SCREEN
}
fn is_default_scroll_delay(v: &u64) -> bool {
    *v == DEFAULT_DELAY_SCROLL
}
fn is_default_tab_delay(v: &u64) -> bool {
    *v == DEFAULT_DELAY_INV_TAB_SWITCH
}
fn is_default_weapon_panel_delay(v: &u64) -> bool {
    *v == DEFAULT_WEAPON_PANEL_DELAY
}
fn is_default_artifact_initial_wait(v: &u64) -> bool {
    *v == DEFAULT_ARTIFACT_INITIAL_WAIT
}
fn is_default_artifact_panel_timeout(v: &u64) -> bool {
    *v == DEFAULT_ARTIFACT_PANEL_TIMEOUT
}
fn is_default_artifact_extra_delay(v: &u64) -> bool {
    *v == DEFAULT_ARTIFACT_EXTRA_DELAY
}
fn is_default_achievement_scroll_delay(v: &u64) -> bool {
    *v == DEFAULT_ACHIEVEMENT_SCROLL_DELAY
}
fn is_default_achievement_category_delay(v: &u64) -> bool {
    *v == DEFAULT_ACHIEVEMENT_CATEGORY_DELAY
}
fn is_default_achievement_open_delay(v: &u64) -> bool {
    *v == DEFAULT_ACHIEVEMENT_OPEN_DELAY
}

/// Fields in GoodUserConfig that must be unsigned integers.
/// If the JSON has an invalid value (e.g. empty string from old config versions),
/// we remove the field so serde fills in its default.
const U64_FIELDS: &[&str] = &[
    "char_tab_delay",
    "char_next_delay",
    "char_open_delay",
    "char_close_delay",
    "inv_scroll_delay",
    "inv_tab_delay",
    "inv_open_delay",
    "weapon_panel_delay",
    "artifact_initial_wait",
    "artifact_panel_timeout",
    "artifact_extra_delay",
    "achievement_scroll_delay",
    "achievement_category_delay",
    "achievement_open_delay",
    "mgr_transition_delay",
    "mgr_action_delay",
    "mgr_cell_delay",
    "mgr_scroll_delay",
    // Old aliases — also sanitize in case they appear
    "weapon_scroll_delay",
    "artifact_scroll_delay",
    "weapon_tab_delay",
    "artifact_tab_delay",
    "weapon_open_delay",
    "artifact_open_delay",
];

/// Migrate retired capture settings and remove invalid numeric fields so serde
/// defaults apply (e.g. empty strings from old config migrations).
fn sanitize_config_json(val: &mut serde_json::Value) {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    // Unversioned files saved continue_on_failure (the inverse of the new
    // stop_on_failure). Preserve explicit choices; absent fields use defaults.
    if !obj.contains_key("schema_version") {
        if !obj.contains_key("stop_on_failure") {
            if let Some(continue_scanning) = obj
                .get("continue_on_failure")
                .and_then(serde_json::Value::as_bool)
            {
                obj.insert("stop_on_failure".to_owned(), (!continue_scanning).into());
            }
        }
        obj.insert(
            "schema_version".to_owned(),
            good_config_schema_version().into(),
        );
    }
    for key in [
        "continue_on_failure",
        "update_inventory",
        "ocr_pool_v4_override",
        "ocr_pool_v5_override",
    ] {
        obj.remove(key);
    }
    for &field in U64_FIELDS {
        let should_remove = match obj.get(field) {
            Some(serde_json::Value::Number(_)) => false,
            Some(_) => true, // string, null, bool, etc. — not a valid u64
            None => false,
        };
        if should_remove {
            obj.remove(field);
        }
    }
}

fn good_config_schema_version() -> u32 {
    1
}

pub fn parse_user_config(contents: &str) -> Result<GoodUserConfig> {
    let mut value: serde_json::Value = serde_json::from_str(contents)?;
    sanitize_config_json(&mut value);
    let config: GoodUserConfig = serde_json::from_value(value)?;
    anyhow::ensure!(
        config.schema_version == good_config_schema_version(),
        "unsupported good_config schema version: {}",
        config.schema_version
    );
    Ok(config)
}

/// User config stored in `data/good_config.json`.
///
/// Holds user-specific in-game names and scanner timing settings.
/// Created interactively on first run; subsequent runs read from the file.
/// New fields are added with serde defaults so old config files still load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoodUserConfig {
    #[serde(default = "good_config_schema_version")]
    pub schema_version: u32,
    /// In-game Traveler name (leave empty if not renamed)
    #[serde(default)]
    pub traveler_name: String,
    /// In-game Wanderer name (leave empty if not renamed)
    #[serde(default)]
    pub wanderer_name: String,
    /// In-game Manekin name (leave empty if not renamed)
    #[serde(default)]
    pub manekin_name: String,
    /// In-game Manekina name (leave empty if not renamed)
    #[serde(default)]
    pub manekina_name: String,

    // --- Timing / delay settings ---
    // Only serialized when user set a value different from default.
    #[serde(
        default = "default_char_tab_delay",
        skip_serializing_if = "is_default_char_tab_delay",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub char_tab_delay: u64,
    #[serde(
        default = "default_char_next_delay",
        skip_serializing_if = "is_default_char_next_delay",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub char_next_delay: u64,
    #[serde(
        default = "default_open_delay",
        skip_serializing_if = "is_default_open_delay",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub char_open_delay: u64,
    #[serde(
        default = "default_close_delay",
        skip_serializing_if = "is_default_close_delay",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub char_close_delay: u64,

    #[serde(
        default = "default_scroll_delay",
        skip_serializing_if = "is_default_scroll_delay",
        deserialize_with = "deserialize_u64_lenient",
        alias = "weapon_scroll_delay",
        alias = "artifact_scroll_delay"
    )]
    pub inv_scroll_delay: u64,
    #[serde(
        default = "default_tab_delay",
        skip_serializing_if = "is_default_tab_delay",
        deserialize_with = "deserialize_u64_lenient",
        alias = "weapon_tab_delay",
        alias = "artifact_tab_delay"
    )]
    pub inv_tab_delay: u64,
    #[serde(
        default = "default_open_delay",
        skip_serializing_if = "is_default_open_delay",
        deserialize_with = "deserialize_u64_lenient",
        alias = "weapon_open_delay",
        alias = "artifact_open_delay"
    )]
    pub inv_open_delay: u64,

    /// Weapon: fixed delay (ms) before panel stability check.
    #[serde(
        default = "default_weapon_panel_delay",
        skip_serializing_if = "is_default_weapon_panel_delay",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub weapon_panel_delay: u64,

    /// Artifact: initial wait (ms) after click before starting panel detection.
    #[serde(
        default = "default_artifact_initial_wait",
        skip_serializing_if = "is_default_artifact_initial_wait",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub artifact_initial_wait: u64,

    /// Artifact: timeout (ms) for fingerprint-based panel load detection.
    #[serde(
        default = "default_artifact_panel_timeout",
        skip_serializing_if = "is_default_artifact_panel_timeout",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub artifact_panel_timeout: u64,

    /// Artifact: extra delay (ms) after panel load, before capture.
    #[serde(
        default = "default_artifact_extra_delay",
        skip_serializing_if = "is_default_artifact_extra_delay",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub artifact_extra_delay: u64,

    /// Achievement list: wait after each wheel tick (ms). Isolated from backpack scroll.
    #[serde(
        default = "default_achievement_scroll_delay",
        skip_serializing_if = "is_default_achievement_scroll_delay",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub achievement_scroll_delay: u64,

    /// Achievement list: wait after clicking a left-side category (ms).
    #[serde(
        default = "default_achievement_category_delay",
        skip_serializing_if = "is_default_achievement_category_delay",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub achievement_category_delay: u64,

    /// Achievement screen: wait after opening from the Paimon menu (ms).
    #[serde(
        default = "default_achievement_open_delay",
        skip_serializing_if = "is_default_achievement_open_delay",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub achievement_open_delay: u64,

    // --- Manager delay settings ---
    /// Screen transition delay for the manager (ms). Default: 1500.
    #[serde(
        default = "default_mgr_transition",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub mgr_transition_delay: u64,
    /// Action button delay for the manager (ms). Default: 800.
    #[serde(
        default = "default_mgr_action",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub mgr_action_delay: u64,
    /// Grid cell click delay for the manager (ms). Default: 100.
    #[serde(
        default = "default_mgr_cell",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub mgr_cell_delay: u64,
    /// Scroll settle delay for the manager (ms). Default: 400.
    #[serde(
        default = "default_mgr_scroll",
        deserialize_with = "deserialize_u64_lenient"
    )]
    pub mgr_scroll_delay: u64,

    /// GUI language preference: "zh" or "en".
    #[serde(default)]
    pub lang: String,

    // --- GUI advanced settings (persisted so they survive restarts) ---
    #[serde(default = "default_true")]
    pub scan_characters: bool,
    #[serde(default = "default_true")]
    pub scan_weapons: bool,
    #[serde(default = "default_true")]
    pub scan_artifacts: bool,
    #[serde(default)]
    pub scan_achievements: bool,
    #[serde(default)]
    pub verbose: bool,
    #[serde(default)]
    pub stop_on_failure: bool,
    #[serde(default)]
    pub dump_images: bool,
    #[serde(default)]
    pub hdr_mode: bool,
    #[serde(default = "default_hdr_white_point")]
    pub hdr_white_point: f32,
    #[serde(default)]
    pub dump_job_data: bool,
    #[serde(default = "default_true")]
    pub only_keep_latest_export: bool,
    #[serde(default)]
    pub char_max_count: usize,
    #[serde(default)]
    pub weapon_max_count: usize,
    #[serde(default)]
    pub artifact_max_count: usize,
    #[serde(default)]
    pub achievement_max_count: usize,
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    #[serde(default = "default_true", alias = "manage_recent_artifacts")]
    pub filter_involved_sets: bool,

    // --- GGScanner capture GUI settings ---
    #[serde(default = "default_true")]
    pub capture_include_characters: bool,
    #[serde(default = "default_true")]
    pub capture_include_weapons: bool,
    #[serde(default = "default_true")]
    pub capture_include_artifacts: bool,
    #[serde(default = "default_true")]
    pub capture_include_achievements: bool,
    #[serde(default)]
    pub capture_dump_packets: bool,
    #[serde(default = "default_true")]
    pub capture_only_keep_latest_export: bool,
}

impl GoodUserConfig {
    fn opt(s: &str) -> Option<String> {
        if s.trim().is_empty() {
            None
        } else {
            Some(s.trim().to_string())
        }
    }

    pub fn to_overrides(&self) -> NameOverrides {
        NameOverrides {
            traveler_name: Self::opt(&self.traveler_name),
            wanderer_name: Self::opt(&self.wanderer_name),
            manekin_name: Self::opt(&self.manekin_name),
            manekina_name: Self::opt(&self.manekina_name),
        }
    }

    /// OCR concurrency follows available memory; there are no user overrides.
    pub fn resolve_ocr_pool_config(&self) -> OcrPoolConfig {
        OcrPoolConfig::detect()
    }
}

impl Default for GoodUserConfig {
    fn default() -> Self {
        Self {
            schema_version: good_config_schema_version(),
            traveler_name: String::new(),
            wanderer_name: String::new(),
            manekin_name: String::new(),
            manekina_name: String::new(),
            char_tab_delay: default_char_tab_delay(),
            char_next_delay: default_char_next_delay(),
            char_open_delay: default_open_delay(),
            char_close_delay: default_close_delay(),
            inv_scroll_delay: default_scroll_delay(),
            inv_tab_delay: default_tab_delay(),
            inv_open_delay: default_open_delay(),
            weapon_panel_delay: default_weapon_panel_delay(),
            artifact_initial_wait: default_artifact_initial_wait(),
            artifact_panel_timeout: default_artifact_panel_timeout(),
            artifact_extra_delay: default_artifact_extra_delay(),
            achievement_scroll_delay: default_achievement_scroll_delay(),
            achievement_category_delay: default_achievement_category_delay(),
            achievement_open_delay: default_achievement_open_delay(),
            mgr_transition_delay: default_mgr_transition(),
            mgr_action_delay: default_mgr_action(),
            mgr_cell_delay: default_mgr_cell(),
            mgr_scroll_delay: default_mgr_scroll(),
            lang: String::new(),
            scan_characters: true,
            scan_weapons: true,
            scan_artifacts: true,
            scan_achievements: false,
            verbose: false,
            stop_on_failure: false,
            dump_images: false,
            hdr_mode: false,
            hdr_white_point: default_hdr_white_point(),
            dump_job_data: false,
            only_keep_latest_export: true,
            char_max_count: 0,
            weapon_max_count: 0,
            artifact_max_count: 0,
            achievement_max_count: 0,
            server_port: default_server_port(),
            filter_involved_sets: true,
            capture_include_characters: true,
            capture_include_weapons: true,
            capture_include_artifacts: true,
            capture_include_achievements: true,
            capture_dump_packets: false,
            capture_only_keep_latest_export: true,
        }
    }
}

/// Load the user config from data/good_config.json without interactive prompts.
/// Returns defaults if the file does not exist or cannot be parsed.
pub fn load_config_or_default() -> GoodUserConfig {
    let path = config_path();
    if !path.exists() {
        return GoodUserConfig::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            // Parse as generic JSON first so we can sanitize invalid field types
            // (e.g. empty strings in u64 fields from old config versions).
            let config_result = parse_user_config(&contents);
            match config_result {
                Ok(config) => config,
                Err(e) => {
                    log_error!(
                        "配置文件解析失败，将使用默认值: {}: {}",
                        "Config parse error (using defaults): {}: {}",
                        path.display(),
                        e
                    );
                    GoodUserConfig::default()
                },
            }
        },
        Err(e) => {
            log_error!(
                "配置文件读取失败，将使用默认值: {}: {}",
                "Config read error (using defaults): {}: {}",
                path.display(),
                e
            );
            GoodUserConfig::default()
        },
    }
}

/// Save the user config to data/good_config.json.
pub fn save_config(config: &GoodUserConfig) -> Result<()> {
    let path = config_path();
    save_user_config_to_path(config, &path)
}

pub fn save_user_config_to_path(config: &GoodUserConfig, path: &std::path::Path) -> Result<()> {
    anyhow::ensure!(
        config.schema_version == good_config_schema_version(),
        "unsupported good_config schema version: {}",
        config.schema_version
    );
    if path.exists() {
        let existing: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        if let Some(version) = existing
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
        {
            anyhow::ensure!(
                version <= u64::from(good_config_schema_version()),
                "refusing to overwrite newer good_config schema version: {version}"
            );
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, &json)?;
    Ok(())
}

/// Load the user config from data/good_config.json (next to the executable).
/// If the file does not exist, return an error instructing the user to create it
/// (via the GUI, or manually).
fn load_or_create_config() -> Result<GoodUserConfig> {
    let path = config_path();
    log_debug!(
        "正在查找配置文件: {}",
        "Looking for config at: {}",
        path.display()
    );

    if !path.exists() {
        return Err(anyhow!(
            "配置文件不存在 / Config file not found: {}\n\
             请先运行 GUI（不带参数启动）创建配置文件，或手动创建。\n\
             Please run the GUI first (launch without arguments) to create the config file,\n\
             or create it manually at the path above.",
            path.display()
        ));
    }

    let contents = std::fs::read_to_string(&path)?;
    // Parse as generic JSON first so we can sanitize invalid field types
    // (e.g. empty strings in u64 fields from old config versions).
    let config = parse_user_config(&contents)
        .with_context(|| format!("配置解析失败 / Failed to parse {}", path.display()))?;
    log_debug!("已加载配置: {}", "Loaded config from {}", path.display());

    // Re-save to strip invalid/default entries and add any new default fields
    if let Err(e) = save_config(&config) {
        log_warn!("配置重新保存失败: {}", "Config re-save failed: {}", e);
    }

    Ok(config)
}

// ================================================================
// CLI config
// ================================================================

/// OCR engine selection, shared by the CLI, GUI and manager server.
#[derive(Clone, Debug, clap::Args)]
pub struct OcrEngineArgs {
    /// 覆盖各类别的辅助OCR后端 / Override every category's secondary OCR backend
    #[arg(
        long = "ocr-backend",
        help = "覆盖所有类别的辅助OCR后端（角色名/等级校验、武器装备回退、圣遗物等级；也用于成就）\n\
                Override the secondary OCR backend of every category (character name/level check, \
                weapon equip fallback, artifact level; also achievements)",
        help_heading = "通用选项 / Global Options"
    )]
    pub ocr_backend: Option<String>,

    /// 角色OCR后端 / Character OCR backend
    #[arg(
        long = "char-ocr",
        help = "角色界面通用OCR后端\nCharacter screen general OCR backend",
        default_value = DEFAULT_CHARACTER_OCR,
        help_heading = "扫描器配置 / Scanner Config"
    )]
    pub char_ocr: String,

    /// 武器OCR后端 / Weapon OCR backend
    #[arg(
        long = "weapon-ocr",
        help = "武器通用OCR后端\nWeapon general OCR backend",
        default_value = DEFAULT_WEAPON_OCR,
        help_heading = "扫描器配置 / Scanner Config"
    )]
    pub weapon_ocr: String,

    /// 圣遗物副词条OCR后端 / Artifact substat OCR backend
    #[arg(
        long = "artifact-substat-ocr",
        help = "圣遗物副词条/通用OCR后端\nArtifact substat/general OCR backend",
        default_value = DEFAULT_ARTIFACT_OCR,
        help_heading = "扫描器配置 / Scanner Config"
    )]
    pub artifact_substat_ocr: String,
}

impl Default for OcrEngineArgs {
    fn default() -> Self {
        Self {
            ocr_backend: None,
            char_ocr: DEFAULT_CHARACTER_OCR.to_string(),
            weapon_ocr: DEFAULT_WEAPON_OCR.to_string(),
            artifact_substat_ocr: DEFAULT_ARTIFACT_OCR.to_string(),
        }
    }
}

impl OcrEngineArgs {
    pub fn backends(&self) -> OcrBackends {
        OcrBackends::resolve(
            self.ocr_backend.as_deref(),
            &self.char_ocr,
            &self.weapon_ocr,
            &self.artifact_substat_ocr,
        )
    }
}

#[derive(Clone, clap::Args)]
#[command(about = "原神GOOD格式扫描器 / Genshin Impact GOOD Format Scanner")]
pub struct GoodScannerConfig {
    // === Scan targets ===
    /// 扫描角色 / Scan characters
    #[arg(
        long = "characters",
        help = "扫描角色\nScan characters",
        help_heading = "扫描目标 / Scan Targets"
    )]
    pub scan_characters: bool,

    /// 扫描武器 / Scan weapons
    #[arg(
        long = "weapons",
        help = "扫描武器\nScan weapons",
        help_heading = "扫描目标 / Scan Targets"
    )]
    pub scan_weapons: bool,

    /// 扫描圣遗物 / Scan artifacts
    #[arg(
        long = "artifacts",
        help = "扫描圣遗物\nScan artifacts",
        help_heading = "扫描目标 / Scan Targets"
    )]
    pub scan_artifacts: bool,

    /// 扫描成就 / Scan achievements
    #[arg(
        long = "achievements",
        help = "扫描成就\nScan achievements",
        help_heading = "扫描目标 / Scan Targets"
    )]
    pub scan_achievements: bool,

    /// 扫描全部 / Scan all
    #[arg(
        long = "all",
        help = "扫描全部（角色+武器+圣遗物）\nScan all (characters + weapons + artifacts)",
        help_heading = "扫描目标 / Scan Targets"
    )]
    pub scan_all: bool,

    // === Global options ===
    /// 显示详细扫描信息 / Show detailed scan info
    #[arg(
        long = "verbose",
        short = 'v',
        help = "显示详细扫描信息\nShow detailed scan info",
        help_heading = "通用选项 / Global Options"
    )]
    pub verbose: bool,

    /// 识别失败时停止 / Stop on OCR errors
    #[arg(
        long = "stop-on-failure",
        help = "识别失败时停止\nStop scanning on the first OCR error",
        help_heading = "通用选项 / Global Options"
    )]
    pub stop_on_failure: bool,

    /// 逐项显示扫描进度 / Log each scanned item
    #[arg(
        long = "log-progress",
        help = "逐项显示扫描进度\nLog each item as it is scanned",
        help_heading = "通用选项 / Global Options"
    )]
    pub log_progress: bool,

    /// 输出目录 / Output directory
    #[arg(
        long = "output-dir",
        help = "输出目录\nOutput directory",
        default_value = ".",
        help_heading = "通用选项 / Global Options"
    )]
    pub output_dir: String,

    #[command(flatten)]
    pub ocr: OcrEngineArgs,

    /// 保存OCR区域截图 / Dump OCR screenshots
    #[arg(
        long = "dump-images",
        help = "保存OCR区域截图到 debug_images/\nDump OCR region screenshots to debug_images/",
        help_heading = "通用选项 / Global Options"
    )]
    pub dump_images: bool,

    /// HDR显示模式 / HDR display mode
    #[arg(
        long = "hdr-mode",
        help = "使用HDR像素阈值（默认使用SDR阈值）\nUse HDR pixel thresholds (default uses SDR thresholds)",
        help_heading = "通用选项 / Global Options"
    )]
    pub hdr_mode: bool,
    #[clap(skip = 4.0f32)]
    pub hdr_white_point: f32,

    // === Scanner config ===
    /// 最低武器稀有度 / Min weapon rarity
    #[arg(
        long = "weapon-min-rarity",
        help = "保留的最低武器稀有度（3-5）\nMinimum weapon rarity to keep (3-5)",
        default_value_t = 3,
        help_heading = "扫描器配置 / Scanner Config"
    )]
    pub weapon_min_rarity: i32,

    /// 最低圣遗物稀有度 / Min artifact rarity
    #[arg(
        long = "artifact-min-rarity",
        help = "保留的最低圣遗物稀有度（4-5）\nMinimum artifact rarity to keep (4-5)",
        default_value_t = 4,
        help_heading = "扫描器配置 / Scanner Config"
    )]
    pub artifact_min_rarity: i32,

    /// 最大角色扫描数 / Max characters
    #[arg(
        long = "char-max-count",
        help = "最大角色扫描数（0=不限）\nMax characters to scan (0 = unlimited)",
        default_value_t = 0,
        help_heading = "扫描器配置 / Scanner Config"
    )]
    pub char_max_count: usize,

    /// 最大武器扫描数 / Max weapons
    #[arg(
        long = "weapon-max-count",
        help = "最大武器扫描数（0=不限）\nMax weapons to scan (0 = unlimited)",
        default_value_t = 0,
        help_heading = "扫描器配置 / Scanner Config"
    )]
    pub weapon_max_count: usize,

    /// 最大圣遗物扫描数 / Max artifacts
    #[arg(
        long = "artifact-max-count",
        help = "最大圣遗物扫描数（0=不限）\nMax artifacts to scan (0 = unlimited)",
        default_value_t = 0,
        help_heading = "扫描器配置 / Scanner Config"
    )]
    pub artifact_max_count: usize,

    /// 最大成就扫描数 / Max achievements
    #[arg(
        long = "achievement-max-count",
        help = "最大成就扫描数（0=不限）\nMax completed achievements to keep (0 = unlimited)",
        default_value_t = 0,
        help_heading = "扫描器配置 / Scanner Config"
    )]
    pub achievement_max_count: usize,

    // weapon_skip_delay and artifact_skip_delay removed — grid-based detection always used
    #[clap(skip = false)]
    pub artifact_keep_five_star_filter: bool,

    /// One-tick achievement list scroll calibration (dumps, then exits)
    #[arg(
        long = "debug-ach-scroll",
        help = "逐格滚动成就列表并保存截图后退出\nDump one-tick achievement list scrolls, then exit",
        help_heading = "调试 / Debug"
    )]
    pub debug_ach_scroll: bool,
}

// ================================================================
// Application
// ================================================================

pub struct GoodScannerApplication {
    arg_matches: ArgMatches,
}

impl GoodScannerApplication {
    pub fn new(matches: ArgMatches) -> Self {
        Self {
            arg_matches: matches,
        }
    }

    pub fn build_command() -> clap::Command {
        let cmd = command!().version(env!("GGSCANNER_BUILD_VERSION"));
        <GoodScannerConfig as Args>::augment_args_for_update(cmd)
    }

    pub fn get_game_info() -> Result<GameInfo> {
        GameInfoBuilder::new()
            .add_local_window_name("\u{539F}\u{795E}") // 原神
            .add_local_window_name("Genshin Impact")
            .add_cloud_window_name("\u{4E91}\u{00B7}\u{539F}\u{795E}") // 云·原神
            .build()
    }

    /// Build a character scanner config from global CLI flags + JSON config.
    pub fn make_char_config(
        config: &GoodScannerConfig,
        user_config: &GoodUserConfig,
    ) -> GoodCharacterScannerConfig {
        GoodCharacterScannerConfig {
            verbose: config.verbose,
            ocr_backend: config.ocr.backends().character.v4,
            tab_delay: user_config.char_tab_delay,
            next_delay: user_config.char_next_delay,
            open_delay: user_config.char_open_delay,
            close_delay: user_config.char_close_delay,
            continue_on_failure: !config.stop_on_failure,
            log_progress: config.log_progress,
            dump_images: config.dump_images,
            max_count: config.char_max_count,
        }
    }

    /// Build a weapon scanner config from global CLI flags + JSON config.
    pub fn make_weapon_config(
        config: &GoodScannerConfig,
        user_config: &GoodUserConfig,
    ) -> GoodWeaponScannerConfig {
        GoodWeaponScannerConfig {
            min_rarity: config.weapon_min_rarity,
            verbose: config.verbose,
            ocr_backend: config.ocr.backends().weapon.v4,
            delay_scroll: user_config.inv_scroll_delay,
            delay_tab: user_config.inv_tab_delay,
            open_delay: user_config.inv_open_delay,
            continue_on_failure: !config.stop_on_failure,
            log_progress: config.log_progress,
            dump_images: config.dump_images,
            max_count: config.weapon_max_count,
            panel_delay: user_config.weapon_panel_delay,
        }
    }

    /// Build an artifact scanner config from global CLI flags + JSON config.
    pub fn make_artifact_config(
        config: &GoodScannerConfig,
        user_config: &GoodUserConfig,
    ) -> GoodArtifactScannerConfig {
        let backends = config.ocr.backends().artifact;
        GoodArtifactScannerConfig {
            min_rarity: config.artifact_min_rarity,
            verbose: config.verbose,
            ocr_backend: backends.v5,
            substat_ocr_backend: backends.v4,
            delay_scroll: user_config.inv_scroll_delay,
            delay_tab: user_config.inv_tab_delay,
            open_delay: user_config.inv_open_delay,
            continue_on_failure: !config.stop_on_failure,
            log_progress: config.log_progress,
            dump_images: config.dump_images,
            max_count: config.artifact_max_count,
            initial_wait: user_config.artifact_initial_wait,
            panel_timeout: user_config.artifact_panel_timeout,
            extra_delay: user_config.artifact_extra_delay,
            keep_five_star_filter: config.artifact_keep_five_star_filter,
        }
    }

    /// Build an achievement scanner config from global CLI flags + JSON config.
    pub fn make_achievement_config(
        config: &GoodScannerConfig,
        user_config: &GoodUserConfig,
    ) -> GoodAchievementScannerConfig {
        GoodAchievementScannerConfig {
            verbose: config.verbose,
            ocr_backend: config
                .ocr
                .ocr_backend
                .clone()
                .unwrap_or_else(|| "ppocrv6tiny".to_string()),
            scroll_delay: user_config.achievement_scroll_delay,
            category_delay: user_config.achievement_category_delay,
            open_delay: user_config.achievement_open_delay,
            continue_on_failure: !config.stop_on_failure,
            log_progress: config.log_progress,
            dump_images: config.dump_images,
            max_count: config.achievement_max_count,
            scroll_calibrate: config.debug_ach_scroll,
        }
    }

    pub fn run(&self) -> Result<()> {
        println!(
            "{}",
            yas::lang::localize("正在启动扫描器... / GGScanner starting...")
        );

        init_rayon_pool();

        // Check for ONNX Runtime before doing anything else
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        ensure_onnxruntime()?;

        let arg_matches = &self.arg_matches;
        let config = GoodScannerConfig::from_arg_matches(arg_matches)?;

        // === Load user config (good_config.json) ===
        let user_config = load_or_create_config()?;

        // Determine what to scan (default: all if no flags specified)
        let no_flags = !config.scan_characters
            && !config.scan_weapons
            && !config.scan_artifacts
            && !config.scan_achievements
            && !config.scan_all
            && !config.debug_ach_scroll;

        let scan_config = ScanCoreConfig {
            scan_characters: if config.debug_ach_scroll {
                false
            } else {
                config.scan_characters || config.scan_all || no_flags
            },
            scan_weapons: if config.debug_ach_scroll {
                false
            } else {
                config.scan_weapons || config.scan_all || no_flags
            },
            scan_artifacts: if config.debug_ach_scroll {
                false
            } else {
                config.scan_artifacts || config.scan_all || no_flags
            },
            scan_achievements: config.scan_achievements || config.debug_ach_scroll,
            weapon_min_rarity: config.weapon_min_rarity,
            artifact_min_rarity: config.artifact_min_rarity,
            verbose: config.verbose,
            stop_on_failure: config.stop_on_failure,
            log_progress: config.log_progress,
            dump_images: config.dump_images || config.debug_ach_scroll,
            hdr_mode: config.hdr_mode || user_config.hdr_mode,
            hdr_white_point: DEFAULT_HDR_WHITE_POINT,
            output_dir: config.output_dir.clone(),
            ocr: config.ocr.clone(),
            char_max_count: config.char_max_count,
            weapon_max_count: config.weapon_max_count,
            artifact_max_count: config.artifact_max_count,
            achievement_max_count: config.achievement_max_count,
            artifact_keep_five_star_filter: false,
            debug_ach_scroll: config.debug_ach_scroll,
        };

        run_scan_core(&user_config, &scan_config, None, None)?;
        Ok(())
    }
}

/// Generate a local-time timestamp string like "2024-01-15_12-30-45".
#[cfg(target_os = "windows")]
pub fn chrono_timestamp() -> String {
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        _dow: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        _ms: u16,
    }
    extern "system" {
        fn GetLocalTime(lpSystemTime: *mut SystemTime);
    }
    let mut st = SystemTime {
        year: 0,
        month: 0,
        _dow: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        _ms: 0,
    };
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        st.year, st.month, st.day, st.hour, st.minute, st.second
    )
}

#[cfg(not(target_os = "windows"))]
pub fn chrono_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_per_day = 86400u64;
    let days = now / secs_per_day;
    let remaining = now % secs_per_day;
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    let seconds = remaining % 60;
    let mut y = 1970i32;
    let mut d = days as i32;
    loop {
        let dy = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
        if d < dy {
            break;
        }
        d -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let md: &[i32] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1;
    for &days_in_month in md {
        if d < days_in_month {
            break;
        }
        d -= days_in_month;
        m += 1;
    }
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        y,
        m,
        d + 1,
        hours,
        minutes,
        seconds
    )
}

// ================================================================
// Core functions for GUI reuse
// ================================================================

/// Standalone scan configuration (no clap dependency).
#[derive(Clone, Debug)]
pub struct ScanCoreConfig {
    pub scan_characters: bool,
    pub scan_weapons: bool,
    pub scan_artifacts: bool,
    pub scan_achievements: bool,
    pub weapon_min_rarity: i32,
    pub artifact_min_rarity: i32,
    pub verbose: bool,
    pub stop_on_failure: bool,
    pub log_progress: bool,
    pub dump_images: bool,
    pub hdr_mode: bool,
    pub hdr_white_point: f32,
    pub output_dir: String,
    pub ocr: OcrEngineArgs,
    pub char_max_count: usize,
    pub weapon_max_count: usize,
    pub artifact_max_count: usize,
    pub achievement_max_count: usize,
    /// If true, keep the artifact tab's 5-star acquired-time filter enabled.
    pub artifact_keep_five_star_filter: bool,
    /// Dump one-tick achievement list scrolls, then exit.
    pub debug_ach_scroll: bool,
}

impl Default for ScanCoreConfig {
    fn default() -> Self {
        Self {
            scan_characters: true,
            scan_weapons: true,
            scan_artifacts: true,
            scan_achievements: false,
            weapon_min_rarity: 3,
            artifact_min_rarity: 4,
            verbose: false,
            stop_on_failure: false,
            log_progress: false,
            dump_images: false,
            hdr_mode: false,
            hdr_white_point: default_hdr_white_point(),
            output_dir: ".".to_string(),
            ocr: OcrEngineArgs::default(),
            char_max_count: 0,
            weapon_max_count: 0,
            artifact_max_count: 0,
            achievement_max_count: 0,
            artifact_keep_five_star_filter: false,
            debug_ach_scroll: false,
        }
    }
}

impl ScanCoreConfig {
    /// Convert to the internal GoodScannerConfig fields needed by make_*_config.
    pub fn to_scanner_config(&self) -> GoodScannerConfig {
        GoodScannerConfig {
            scan_characters: self.scan_characters,
            scan_weapons: self.scan_weapons,
            scan_artifacts: self.scan_artifacts,
            scan_achievements: self.scan_achievements,
            scan_all: false,
            verbose: self.verbose,
            stop_on_failure: self.stop_on_failure,
            log_progress: self.log_progress,
            output_dir: self.output_dir.clone(),
            ocr: self.ocr.clone(),
            dump_images: self.dump_images,
            hdr_mode: self.hdr_mode,
            hdr_white_point: self.hdr_white_point,
            weapon_min_rarity: self.weapon_min_rarity,
            artifact_min_rarity: self.artifact_min_rarity,
            char_max_count: self.char_max_count,
            weapon_max_count: self.weapon_max_count,
            artifact_max_count: self.artifact_max_count,
            achievement_max_count: self.achievement_max_count,
            artifact_keep_five_star_filter: self.artifact_keep_five_star_filter,
            debug_ach_scroll: self.debug_ach_scroll,
        }
    }
}

/// Run a scan without CLI arg parsing. Returns the export path on success.
///
/// This is the core scan logic extracted from `GoodScannerApplication::run()`,
/// usable from both CLI and GUI.
pub fn run_scan_core(
    user_config: &GoodUserConfig,
    config: &ScanCoreConfig,
    status_fn: Option<&dyn Fn(&str)>,
    cancel_token: Option<yas::cancel::CancelToken>,
) -> Result<String> {
    run_scan_core_observed(user_config, config, status_fn, cancel_token, None, None)
}

pub fn run_scan_core_observed(
    user_config: &GoodUserConfig,
    config: &ScanCoreConfig,
    status_fn: Option<&dyn Fn(&str)>,
    cancel_token: Option<yas::cancel::CancelToken>,
    progress_fn: Option<&crate::scanner::common::progress::ProgressFn<'_>>,
    phase_fn: Option<&crate::scanner::common::progress::ScanPhaseFn<'_>>,
) -> Result<String> {
    init_rayon_pool();
    crate::scanner::common::annotator::init(config.dump_images);
    crate::scanner::common::pixel_profile::set_hdr_mode(config.hdr_mode);
    crate::scanner::common::pixel_profile::set_hdr_white_point(config.hdr_white_point);

    let report = |msg: &str| {
        if let Some(f) = status_fn {
            f(msg);
        }
    };

    #[cfg(target_os = "windows")]
    {
        yas::utils::ensure_admin()?;
    }

    // Fetch and load mappings
    report("加载映射数据 / Loading mappings...");
    let overrides = user_config.to_overrides();
    let mappings = Arc::new(MappingManager::new(&overrides)?);
    log_info!(
        "已加载: {} 角色, {} 武器, {} 圣遗物套装",
        "Loaded: {} characters, {} weapons, {} artifact sets",
        mappings.character_name_map.len(),
        mappings.weapon_name_map.len(),
        mappings.artifact_set_map.len()
    );

    // Find and focus the game window
    report("查找游戏窗口 / Finding game window...");
    let game_info = GoodScannerApplication::get_game_info()?;
    log_info!(
        "游戏窗口: {}x{}",
        "Game window: {}x{}",
        game_info.window.width,
        game_info.window.height,
    );

    report("初始化屏幕截图 / Initializing screen capture...");
    let capture_method = CaptureMethod::for_hdr_mode(config.hdr_mode);
    let mut ctrl = GenshinGameController::new(game_info, capture_method)
        .context("屏幕截图初始化失败 / Screen capture initialization failed")?;
    let token = cancel_token.unwrap_or_else(yas::cancel::CancelToken::new);

    // Create shared OCR pools for all scanners
    report("加载OCR模型 / Loading OCR models...");
    let pool_config = user_config.resolve_ocr_pool_config();
    let pools = Arc::new(SharedOcrPools::new(pool_config, &config.ocr.backends())?);

    let scan_result = run_scan_phases(
        &mut ctrl,
        mappings,
        pools,
        user_config,
        config,
        progress_fn,
        status_fn,
        phase_fn,
        token.clone(),
        ScanRunOptions {
            save_on_cancel: true,
            accept_cancelled_success: true,
            failure_policy: ScanFailurePolicy::ContinueOnError,
        },
    )?;

    let mut partial = token.is_cancelled()
        || matches!(
            scan_result.characters,
            ScanPhaseResult::Incomplete | ScanPhaseResult::Failed(_)
        )
        || matches!(
            scan_result.weapons,
            ScanPhaseResult::Incomplete | ScanPhaseResult::Failed(_)
        )
        || matches!(
            scan_result.artifacts,
            ScanPhaseResult::Incomplete | ScanPhaseResult::Failed(_)
        )
        || matches!(
            scan_result.achievements,
            ScanPhaseResult::Incomplete | ScanPhaseResult::Failed(_)
        );
    let characters = scan_result.characters.into_complete();
    let weapons = scan_result.weapons.into_complete();
    let artifacts = scan_result.artifacts.into_complete();
    let achievements = scan_result.achievements.into_complete();

    // A configured cap only makes the export partial when it was actually reached.
    partial |= characters
        .as_ref()
        .is_some_and(|items| config.char_max_count > 0 && items.len() >= config.char_max_count)
        || weapons.as_ref().is_some_and(|items| {
            config.weapon_max_count > 0 && items.len() >= config.weapon_max_count
        })
        || artifacts.as_ref().is_some_and(|items| {
            config.artifact_max_count > 0 && items.len() >= config.artifact_max_count
        })
        || achievements.as_ref().is_some_and(|items| {
            config.achievement_max_count > 0 && items.len() >= config.achievement_max_count
        });

    if characters.as_ref().is_none_or(Vec::is_empty)
        && weapons.as_ref().is_none_or(Vec::is_empty)
        && artifacts.as_ref().is_none_or(Vec::is_empty)
        && achievements.as_ref().is_none_or(Vec::is_empty)
    {
        if token.is_cancelled() {
            return Err(crate::scanner::common::progress::ScanCancelled.into());
        }
        anyhow::bail!("未识别出数据，未导出 / No data recognized; nothing exported");
    }

    // Wait for any pending debug image writes to finish
    crate::scanner::common::annotator::flush();

    // Export as GOOD v3
    report("正在导出数据 / Saving export");
    let export = GoodExport::new(characters, weapons, artifacts).with_achievements(achievements);
    let json = serde_json::to_string_pretty(&export)?;

    let path = crate::fs_utils::write_scan_export(
        &PathBuf::from(&config.output_dir),
        &json,
        &chrono_timestamp(),
        partial,
        user_config.only_keep_latest_export,
    )?;
    let path_str = path.display().to_string();
    log_info!("已导出: {}", "Exported to {}", path_str);

    Ok(path_str)
}

/// Run the artifact manager HTTP server (blocks until the server is stopped).
///
/// The `enabled` flag controls whether POST /manage requests are executed.
/// When false, the server still runs but returns 503 for manage requests.
/// Health and CORS endpoints always respond.
pub fn run_server_core(
    user_config: &GoodUserConfig,
    server_port: u16,
    ocr: &OcrEngineArgs,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop_on_all_matched: bool,
    filter_involved_sets: bool,
    dump_images: bool,
    dump_job_data: bool,
    status_fn: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
) -> Result<()> {
    run_server_core_observed(
        user_config,
        server_port,
        ocr,
        enabled,
        shutdown,
        stop_on_all_matched,
        filter_involved_sets,
        dump_images,
        dump_job_data,
        status_fn,
        None,
    )
}

pub fn run_server_core_observed(
    user_config: &GoodUserConfig,
    server_port: u16,
    ocr: &OcrEngineArgs,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop_on_all_matched: bool,
    filter_involved_sets: bool,
    dump_images: bool,
    dump_job_data: bool,
    status_fn: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
    native_job: Option<std::sync::Arc<std::sync::Mutex<crate::manager::models::JobState>>>,
) -> Result<()> {
    init_rayon_pool();
    crate::scanner::common::annotator::init(dump_images);
    crate::scanner::common::pixel_profile::set_hdr_mode(user_config.hdr_mode);
    crate::scanner::common::pixel_profile::set_hdr_white_point(DEFAULT_HDR_WHITE_POINT);

    #[cfg(target_os = "windows")]
    {
        yas::utils::ensure_admin()?;
    }

    log_info!("加载映射数据...", "Loading mappings...");
    let overrides = user_config.to_overrides();
    let mappings = Arc::new(MappingManager::new(&overrides)?);
    log_info!(
        "已加载: {}角色, {}武器, {}套装",
        "Loaded: {} characters, {} weapons, {} artifact sets",
        mappings.character_name_map.len(),
        mappings.weapon_name_map.len(),
        mappings.artifact_set_map.len()
    );

    let ocr_backends = ocr.backends();
    let scroll_delay = user_config.inv_scroll_delay;
    let capture_delay = user_config.artifact_extra_delay;
    let panel_timeout = user_config.artifact_panel_timeout;
    let initial_wait = user_config.artifact_initial_wait;
    let mappings_clone = mappings.clone();

    let mgr_delays = crate::manager::ui_actions::ManagerDelays {
        transition: user_config.mgr_transition_delay,
        action: user_config.mgr_action_delay,
        cell: user_config.mgr_cell_delay,
        scroll: user_config.mgr_scroll_delay,
    };

    let scan_defaults = ScanCoreConfig {
        scan_characters: true,
        scan_weapons: true,
        scan_artifacts: true,
        dump_images,
        hdr_mode: user_config.hdr_mode,
        ocr: ocr.clone(),
        ..ScanCoreConfig::default()
    };
    let exec_user_config = user_config.clone();

    // Fn closure so the server can retry init on subsequent jobs if the first
    // attempt fails (e.g. game window not open yet). All captures are cloned
    // inside the body so each call builds fresh state.
    let init_executor = move || -> anyhow::Result<Box<dyn crate::server::ManageExecutor>> {
        crate::manager::ui_actions::set_manager_delays(mgr_delays.clone());
        log_info!("查找游戏窗口...", "Finding game window...");
        let game_info = GoodScannerApplication::get_game_info()?;
        log_info!("初始化屏幕截图...", "Initializing screen capture...");
        let capture_method = CaptureMethod::for_hdr_mode(exec_user_config.hdr_mode);
        let ctrl = GenshinGameController::new(game_info, capture_method)?;
        log_info!("加载OCR模型...", "Loading OCR models...");
        let pool_config = exec_user_config.resolve_ocr_pool_config();
        let pools = Arc::new(SharedOcrPools::new(pool_config, &ocr_backends).context(
            "OCR模型加载失败，请确认内存充足（建议8GB以上）\
                     / OCR model load failed — ensure sufficient memory (8 GB+ recommended)",
        )?);
        let manager = crate::manager::orchestrator::ArtifactManager::new(
            mappings_clone.clone(),
            pools,
            capture_delay,
            scroll_delay,
            panel_timeout,
            initial_wait,
            stop_on_all_matched,
            filter_involved_sets,
            dump_images,
        );
        Ok(Box::new(crate::server::GameExecutor {
            ctrl,
            manager,
            user_config: exec_user_config.clone(),
            scan_defaults: scan_defaults.clone(),
        }))
    };

    crate::server::run_server_observed(
        server_port,
        init_executor,
        enabled,
        shutdown,
        dump_job_data,
        status_fn,
        native_job,
    )
}

/// Execute manage instructions from a JSON string.
pub fn run_manage_json(
    user_config: &GoodUserConfig,
    json_str: &str,
    ocr: &OcrEngineArgs,
    cancel_token: Option<yas::cancel::CancelToken>,
) -> Result<crate::manager::models::ManageResult> {
    crate::scanner::common::pixel_profile::set_hdr_mode(user_config.hdr_mode);
    crate::scanner::common::pixel_profile::set_hdr_white_point(DEFAULT_HDR_WHITE_POINT);
    #[cfg(target_os = "windows")]
    {
        yas::utils::ensure_admin()?;
    }

    crate::manager::ui_actions::set_manager_delays(crate::manager::ui_actions::ManagerDelays {
        transition: user_config.mgr_transition_delay,
        action: user_config.mgr_action_delay,
        cell: user_config.mgr_cell_delay,
        scroll: user_config.mgr_scroll_delay,
    });

    let request: crate::manager::models::LockManageRequest = serde_json::from_str(json_str)
        .map_err(|e| anyhow!("JSON解析失败 / JSON parse error: {}", e))?;

    let total = request.lock.len() + request.unlock.len();
    log_info!(
        "执行 {} 条管理请求（lock: {}, unlock: {}）",
        "Executing {} manage items (lock: {}, unlock: {})",
        total,
        request.lock.len(),
        request.unlock.len()
    );

    let overrides = user_config.to_overrides();
    let mappings = Arc::new(MappingManager::new(&overrides)?);

    let game_info = GoodScannerApplication::get_game_info()?;
    let capture_method = CaptureMethod::for_hdr_mode(user_config.hdr_mode);
    let mut ctrl = GenshinGameController::new(game_info, capture_method)?;
    let token = cancel_token.unwrap_or_else(yas::cancel::CancelToken::new);

    let pool_config = user_config.resolve_ocr_pool_config();
    let pools = Arc::new(SharedOcrPools::new(pool_config, &ocr.backends())?);
    let manager = crate::manager::orchestrator::ArtifactManager::new(
        mappings,
        pools,
        user_config.artifact_extra_delay,
        user_config.inv_scroll_delay,
        user_config.artifact_panel_timeout,
        user_config.artifact_initial_wait,
        false,
        false,
        false, // dump_images: offline JSON mode doesn't support it
    );

    let (result, _artifact_snapshot) = manager.execute(&mut ctrl, request, None, token);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::common::ocr_pool::OcrSlotBackends;

    #[test]
    fn stop_on_failure_controls_every_scanner_and_defaults_to_continue() {
        for (args, stop) in [(vec![], false), (vec!["--stop-on-failure"], true)] {
            let config = parse_scanner_args(&args);
            assert_eq!(config.stop_on_failure, stop);
            let user = GoodUserConfig::default();
            assert_eq!(
                GoodScannerApplication::make_char_config(&config, &user).continue_on_failure,
                !stop
            );
            assert_eq!(
                GoodScannerApplication::make_weapon_config(&config, &user).continue_on_failure,
                !stop
            );
            assert_eq!(
                GoodScannerApplication::make_artifact_config(&config, &user).continue_on_failure,
                !stop
            );
            assert_eq!(
                GoodScannerApplication::make_achievement_config(&config, &user).continue_on_failure,
                !stop
            );
        }
    }
    use clap::{Args, Command, FromArgMatches};

    fn parse_scanner_args(args: &[&str]) -> GoodScannerConfig {
        let cmd = GoodScannerConfig::augment_args(Command::new("scan"));
        cmd.clone().debug_assert();
        let matches = cmd.get_matches_from(std::iter::once("scan").chain(args.iter().copied()));
        GoodScannerConfig::from_arg_matches(&matches).unwrap()
    }

    #[test]
    fn ocr_flags_default_per_category() {
        let cfg = parse_scanner_args(&[]);

        assert_eq!(cfg.ocr.backends(), OcrBackends::default());
    }

    #[test]
    fn ocr_flags_override_each_category() {
        let cfg = parse_scanner_args(&[
            "--char-ocr",
            "ppocrv5",
            "--weapon-ocr",
            "ppocrv4",
            "--artifact-substat-ocr",
            "ppocrv4",
            "--ocr-backend",
            "ppocrv5",
        ]);

        let backends = cfg.ocr.backends();
        let expect = |v4: &str| OcrSlotBackends {
            v5: "ppocrv5".to_string(),
            v4: v4.to_string(),
        };
        assert_eq!(backends.character, expect("ppocrv5"));
        assert_eq!(backends.weapon, expect("ppocrv4"));
        assert_eq!(backends.artifact, expect("ppocrv4"));
    }

    #[test]
    fn retired_capture_method_migrates_without_losing_genshin_preferences() {
        for hdr_mode in [false, true] {
            let mut saved = serde_json::json!({
                "capture_method":"print_window", "hdr_mode":hdr_mode,
                "traveler_name":"Traveler", "char_tab_delay":750,
                "scan_artifacts":false, "dump_images":true, "save_on_cancel":true
            });
            sanitize_config_json(&mut saved);
            let config: GoodUserConfig = serde_json::from_value(saved).unwrap();
            assert_eq!(config.hdr_mode, hdr_mode);
            assert_eq!(config.traveler_name, "Traveler");
            assert_eq!(config.char_tab_delay, 750);
            assert!(!config.scan_artifacts);
            assert!(config.dump_images);
            let saved = serde_json::to_value(&config).unwrap();
            assert!(saved.get("capture_method").is_none());
            assert!(saved.get("save_on_cancel").is_none());
        }
    }

    #[test]
    fn user_config_accepts_legacy_recent_manager_key() {
        let cfg: GoodUserConfig =
            serde_json::from_str(r#"{"manage_recent_artifacts":true}"#).unwrap();

        assert!(cfg.filter_involved_sets);
    }

    #[test]
    fn user_config_persists_goodcapture_options() {
        let defaults: GoodUserConfig = serde_json::from_str("{}").unwrap();
        assert!(defaults.capture_include_characters);
        assert!(defaults.capture_include_weapons);
        assert!(defaults.capture_include_artifacts);
        assert!(defaults.capture_include_achievements);
        assert!(!defaults.capture_dump_packets);
        assert!(defaults.only_keep_latest_export);
        assert!(defaults.capture_only_keep_latest_export);

        let cfg: GoodUserConfig = serde_json::from_str(
            r#"{
                "capture_include_characters": false,
                "capture_include_weapons": true,
                "capture_include_artifacts": false,
                "capture_include_achievements": false,
                "capture_dump_packets": true,
                "capture_only_keep_latest_export": false,
                "only_keep_latest_export": false
            }"#,
        )
        .unwrap();

        assert!(!cfg.capture_include_characters);
        assert!(cfg.capture_include_weapons);
        assert!(!cfg.capture_include_artifacts);
        assert!(!cfg.capture_include_achievements);
        assert!(cfg.capture_dump_packets);
        assert!(!cfg.capture_only_keep_latest_export);
        assert!(!cfg.only_keep_latest_export);

        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["capture_include_characters"], false);
        assert_eq!(json["capture_include_weapons"], true);
        assert_eq!(json["capture_include_artifacts"], false);
        assert_eq!(json["capture_include_achievements"], false);
        assert_eq!(json["capture_dump_packets"], true);
        assert_eq!(json["capture_only_keep_latest_export"], false);
        assert_eq!(json["only_keep_latest_export"], false);
    }
}
