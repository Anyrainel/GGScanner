//! Pinned ONNX binaries from NuGet (both platforms) and GitHub release mirrors.
//! Range requests retrieve just the native library from the multi-platform nupkg.
use std::io::{Cursor, Read};

use anyhow::{anyhow, bail, ensure, Context, Result};
use sha2::{Digest, Sha256};

use crate::download;

const NUGET_SOURCES: &[&str] = &[
    "https://mirrors.huaweicloud.com/artifactory/api/nuget/v3/nuget-remote/microsoft.ml.onnxruntime/1.22.0/microsoft.ml.onnxruntime.1.22.0.nupkg",
    "https://globalcdn.nuget.org/packages/microsoft.ml.onnxruntime.1.22.0.nupkg",
];
const MAX_ARCHIVE: usize = 150 * 1024 * 1024;
const MAX_LIBRARY: usize = 40 * 1024 * 1024;
const ZIP_TAIL: u64 = 65_557;

pub(crate) struct Library {
    entry: &'static str,
    archive: &'static str,
    release_zip_entry: Option<&'static str>,
    sha256: &'static str,
}

// Verified against Microsoft's 1.22.0 NuGet AND GitHub release artifacts.
#[cfg(any(target_os = "windows", test))]
pub(crate) const WINDOWS: Library = Library {
    entry: "runtimes/win-x64/native/onnxruntime.dll",
    archive: "https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-win-x64-1.22.0.zip",
    release_zip_entry: Some("onnxruntime-win-x64-1.22.0/lib/onnxruntime.dll"),
    sha256: "579b636403983254346a5c1d80bd28f1519cd1e284cd204f8d4ff41f8d711559",
};
#[cfg(any(target_os = "linux", test))]
pub(crate) const LINUX: Library = Library {
    entry: "runtimes/linux-x64/native/libonnxruntime.so",
    archive: "https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-linux-x64-1.22.0.tgz",
    release_zip_entry: None,
    sha256: "3da6146e14e7b8aaec625dde11d6114c7457c87a5f93d744897da8781e35c673",
};

pub(crate) fn fetch(
    library: &Library,
    extract_release: impl Fn(&[u8]) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    download::run(async {
        let client = download::client(true)?;
        let sources: Vec<_> = NUGET_SOURCES
            .iter()
            .map(|url| (true, (*url).to_owned()))
            .chain(
                download::github_sources(library.archive)
                    .into_iter()
                    .map(|url| (false, url)),
            )
            .collect();
        download::first_valid(sources.iter().map(|(nuget, url)| {
            let client = &client;
            let extract_release = &extract_release;
            move || async move {
                let result: Result<Vec<u8>> = async {
                    let bytes = if *nuget {
                        nuget_library(client, url, library.entry).await?
                    } else if let Some(entry) = library.release_zip_entry {
                        // The Windows release ZIP contains large debug symbols too.
                        // Proxy Range support avoids downloading those on every route.
                        match nuget_library(client, url, entry).await {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                yas::log_debug!(
                                    "源不支持按需提取，尝试完整压缩包: {:#}",
                                    "Ranged extraction failed; trying the complete archive: {:#}",
                                    error
                                );
                                extract_release(&download::get(client, url, MAX_ARCHIVE).await?)?
                            },
                        }
                    } else {
                        extract_release(&download::get(client, url, MAX_ARCHIVE).await?)?
                    };
                    verify(&bytes, library.sha256)?;
                    yas::log_debug!("运行库下载源完成: {}", "Runtime source completed: {}", url);
                    Ok(bytes)
                }
                .await;
                result.with_context(|| format!("ONNX source {url}"))
            }
        }))
        .await
    })
}

fn verify(bytes: &[u8], expected: &str) -> Result<()> {
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == expected,
        "ONNX Runtime SHA-256 mismatch"
    );
    Ok(())
}

fn zip_library(bytes: &[u8], entry: &str) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    let file = zip.by_name(entry)?;
    ensure!(
        file.size() <= MAX_LIBRARY as u64,
        "ONNX library exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.take(MAX_LIBRARY as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_LIBRARY,
        "ONNX library exceeds size limit"
    );
    Ok(bytes)
}

enum RangeBody {
    Partial {
        start: u64,
        total: u64,
        bytes: Vec<u8>,
    },
    Complete(Vec<u8>),
}

async fn range(client: &reqwest::Client, url: &str, spec: &str) -> Result<RangeBody> {
    let response = download::send(
        client
            .get(url)
            .header("Range", format!("bytes={spec}"))
            .header("Accept-Encoding", "identity"),
    )
    .await?;
    if response.status() == reqwest::StatusCode::OK {
        return Ok(RangeBody::Complete(
            download::body(response, MAX_ARCHIVE).await?,
        ));
    }
    ensure!(
        response.status() == reqwest::StatusCode::PARTIAL_CONTENT,
        "Expected HTTP 200 or 206"
    );
    let header = response
        .headers()
        .get("Content-Range")
        .context("Missing Content-Range")?
        .to_str()?;
    let (bounds, total) = header
        .strip_prefix("bytes ")
        .context("Invalid Content-Range")?
        .split_once('/')
        .context("Invalid Content-Range")?;
    let (start, end) = bounds.split_once('-').context("Invalid Content-Range")?;
    let (start, end, total): (u64, u64, u64) = (start.parse()?, end.parse()?, total.parse()?);
    ensure!(
        start <= end && end < total && total <= MAX_ARCHIVE as u64,
        "Invalid range bounds"
    );
    if let Some(suffix) = spec.strip_prefix('-') {
        let suffix: u64 = suffix.parse()?;
        ensure!(
            end + 1 == total && start == total.saturating_sub(suffix),
            "Wrong suffix response"
        );
    } else {
        ensure!(
            spec == format!("{start}-{end}"),
            "Server returned the wrong byte range"
        );
    }
    let bytes = download::body(response, (end - start + 1) as usize).await?;
    ensure!(
        bytes.len() as u64 == end - start + 1,
        "Truncated range response"
    );
    Ok(RangeBody::Partial {
        start,
        total,
        bytes,
    })
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .context("Truncated ZIP header")?
            .try_into()?,
    ))
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .context("Truncated ZIP header")?
            .try_into()?,
    ))
}

struct ZipEntry {
    offset: u64,
    compressed: u64,
    uncompressed: u64,
    method: u16,
}

fn find_entry(directory: &[u8], name: &str) -> Result<ZipEntry> {
    let mut offset = 0;
    while offset < directory.len() {
        ensure!(
            directory.get(offset..offset + 4) == Some(b"PK\x01\x02"),
            "Invalid ZIP directory"
        );
        let size = u16_at(directory, offset + 28)? as usize;
        let extra = u16_at(directory, offset + 30)? as usize;
        let comment = u16_at(directory, offset + 32)? as usize;
        let filename = directory
            .get(offset + 46..offset + 46 + size)
            .context("Truncated ZIP filename")?;
        if filename == name.as_bytes() {
            ensure!(
                u16_at(directory, offset + 8)? & 1 == 0,
                "Encrypted ZIP entry"
            );
            let entry = ZipEntry {
                offset: u32_at(directory, offset + 42)? as u64,
                compressed: u32_at(directory, offset + 20)? as u64,
                uncompressed: u32_at(directory, offset + 24)? as u64,
                method: u16_at(directory, offset + 10)?,
            };
            ensure!(
                entry.compressed > 0
                    && entry.compressed <= MAX_LIBRARY as u64
                    && entry.uncompressed <= MAX_LIBRARY as u64,
                "Invalid ZIP library size"
            );
            return Ok(entry);
        }
        offset += 46 + size + extra + comment;
    }
    bail!("ONNX library missing from NuGet archive")
}

async fn nuget_library(client: &reqwest::Client, url: &str, name: &str) -> Result<Vec<u8>> {
    let (start, total, tail) = match range(client, url, &format!("-{ZIP_TAIL}")).await? {
        RangeBody::Complete(bytes) => return zip_library(&bytes, name),
        RangeBody::Partial {
            start,
            total,
            bytes,
        } => (start, total, bytes),
    };
    let end = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&offset| {
            tail.get(offset..offset + 4) == Some(b"PK\x05\x06")
                && u16_at(&tail, offset + 20)
                    .is_ok_and(|len| offset + 22 + len as usize == tail.len())
        })
        .context("Missing ZIP end record")?;
    ensure!(
        u16_at(&tail, end + 4)? == 0 && u16_at(&tail, end + 6)? == 0,
        "Multi-disk ZIP unsupported"
    );
    let size = u32_at(&tail, end + 12)? as u64;
    let offset = u32_at(&tail, end + 16)? as u64;
    ensure!(
        size > 0 && size <= download::METADATA_LIMIT as u64 && offset + size <= start + end as u64,
        "Invalid ZIP directory bounds"
    );
    let directory = if offset >= start {
        tail.get((offset - start) as usize..(offset - start + size) as usize)
            .context("Truncated ZIP directory")?
            .to_vec()
    } else {
        match range(client, url, &format!("{offset}-{}", offset + size - 1)).await? {
            RangeBody::Complete(bytes) => return zip_library(&bytes, name),
            RangeBody::Partial {
                total: next_total,
                bytes,
                ..
            } => {
                ensure!(next_total == total, "Archive changed during download");
                bytes
            },
        }
    };
    let entry = find_entry(&directory, name)?;
    ensure!(
        entry.offset + 30 < offset,
        "Invalid ZIP local header offset"
    );
    let header = match range(
        client,
        url,
        &format!("{}-{}", entry.offset, entry.offset + 29),
    )
    .await?
    {
        RangeBody::Complete(bytes) => return zip_library(&bytes, name),
        RangeBody::Partial {
            total: next_total,
            bytes,
            ..
        } => {
            ensure!(next_total == total, "Archive changed during download");
            bytes
        },
    };
    ensure!(
        header.get(..4) == Some(b"PK\x03\x04"),
        "Invalid ZIP local header"
    );
    let data_start = entry.offset + 30 + u16_at(&header, 26)? as u64 + u16_at(&header, 28)? as u64;
    ensure!(
        data_start + entry.compressed <= offset,
        "Invalid ZIP payload bounds"
    );
    let compressed = match range(
        client,
        url,
        &format!("{data_start}-{}", data_start + entry.compressed - 1),
    )
    .await?
    {
        RangeBody::Complete(bytes) => return zip_library(&bytes, name),
        RangeBody::Partial {
            total: next_total,
            bytes,
            ..
        } => {
            ensure!(next_total == total, "Archive changed during download");
            bytes
        },
    };
    let bytes = match entry.method {
        0 => compressed,
        8 => {
            let mut bytes = Vec::new();
            flate2::read::DeflateDecoder::new(&compressed[..])
                .take(MAX_LIBRARY as u64 + 1)
                .read_to_end(&mut bytes)?;
            bytes
        },
        method => return Err(anyhow!("Unsupported ZIP method {method}")),
    };
    ensure!(
        bytes.len() as u64 == entry.uncompressed,
        "Wrong uncompressed library size"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn fixture() -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("unrelated-platform.bin", options).unwrap();
        let mut state = 1234567u32;
        let noise: Vec<_> = (0..512 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        zip.write_all(&noise).unwrap();
        zip.start_file(WINDOWS.entry, options).unwrap();
        zip.write_all(b"test native library").unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn range_extraction_skips_other_platforms_and_handles_range_ignored() {
        for use_range in [true, false] {
            let bytes = fixture();
            let size = bytes.len();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}", server.server_addr());
            let transferred = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&transferred);
            let worker = std::thread::spawn(move || {
                for request in server
                    .incoming_requests()
                    .take(if use_range { 3 } else { 1 })
                {
                    let range = request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Range"))
                        .unwrap()
                        .value
                        .as_str();
                    let spec = range.strip_prefix("bytes=").unwrap();
                    let (start, end) = if let Some(suffix) = spec.strip_prefix('-') {
                        (size.saturating_sub(suffix.parse().unwrap()), size - 1)
                    } else {
                        let (start, end) = spec.split_once('-').unwrap();
                        (start.parse().unwrap(), end.parse().unwrap())
                    };
                    let payload = if use_range {
                        bytes[start..=end].to_vec()
                    } else {
                        bytes.clone()
                    };
                    counter.fetch_add(payload.len(), Ordering::SeqCst);
                    let mut response = tiny_http::Response::from_data(payload);
                    if use_range {
                        response = response.with_status_code(206).with_header(
                            tiny_http::Header::from_bytes(
                                "Content-Range",
                                format!("bytes {start}-{end}/{size}"),
                            )
                            .unwrap(),
                        );
                    }
                    request.respond(response).unwrap();
                }
            });
            let library = download::run(async {
                nuget_library(&download::client(true)?, &url, WINDOWS.entry).await
            })
            .unwrap();
            assert_eq!(library, b"test native library");
            worker.join().unwrap();
            if use_range {
                assert!(transferred.load(Ordering::SeqCst) < size / 4);
            } else {
                assert_eq!(transferred.load(Ordering::SeqCst), size);
            }
        }
    }

    #[test]
    fn wrong_range_and_tampered_library_are_rejected() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            server
                .recv()
                .unwrap()
                .respond(
                    tiny_http::Response::from_data(fixture())
                        .with_status_code(206)
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Range", "bytes 0-9/10").unwrap(),
                        ),
                )
                .unwrap();
        });
        assert!(download::run(async {
            nuget_library(&download::client(true)?, &url, WINDOWS.entry).await
        })
        .is_err());
        worker.join().unwrap();
        assert!(verify(b"tampered library", WINDOWS.sha256).is_err());
        assert!(find_entry(b"invalid central directory", WINDOWS.entry).is_err());
    }

    #[test]
    #[ignore = "live external mirror verification; no files are installed"]
    fn live_nuget_both_platforms() {
        download::run(async {
            let client = download::client(true)?;
            for library in [&WINDOWS, &LINUX] {
                for url in NUGET_SOURCES {
                    let start = std::time::Instant::now();
                    let bytes = nuget_library(&client, url, library.entry).await?;
                    verify(&bytes, library.sha256)?;
                    println!(
                        "{}: {} verified bytes from {} in {:?}",
                        library.entry,
                        bytes.len(),
                        url,
                        start.elapsed()
                    );
                }
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    #[ignore = "live Windows runtime race; no files are installed"]
    fn live_windows_runtime_race() {
        let start = std::time::Instant::now();
        let bytes = fetch(&WINDOWS, |bytes| {
            zip_library(bytes, WINDOWS.release_zip_entry.unwrap())
        })
        .unwrap();
        println!(
            "Concurrent runtime download: {} verified bytes in {:?}",
            bytes.len(),
            start.elapsed()
        );
    }
}
