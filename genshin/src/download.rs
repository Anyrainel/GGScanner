//! External downloads race real requests, without a geolocation dependency.
//! Dropping losing futures cancels their requests (no detached download threads).
use std::future::Future;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use futures::{
    future::{AbortHandle, Abortable},
    stream::FuturesUnordered,
    StreamExt,
};

pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const METADATA_LIMIT: usize = 2 * 1024 * 1024;
const GITHUB_MIRRORS: &[&str] = &["", "https://gh-proxy.com/", "https://ghfast.top/"];
const SAMPLE_WINDOW: Duration = Duration::from_secs(3);
tokio::task_local! { static TRANSFERRED: Arc<AtomicU64>; }

/// Only GitHub resources get public proxies. Hosted game data stays on our sites.
pub(crate) fn github_sources(url: &str) -> Vec<String> {
    let parsed = reqwest::Url::parse(url).ok();
    let mirrors = match parsed
        .as_ref()
        .filter(|url| url.scheme() == "https")
        .and_then(|url| url.host_str())
    {
        Some("github.com") => GITHUB_MIRRORS,
        Some("api.github.com") => &GITHUB_MIRRORS[..2], // ghfast does not proxy API requests.
        _ => return vec![url.to_owned()],
    };
    mirrors
        .iter()
        .map(|prefix| format!("{prefix}{url}"))
        .collect()
}

pub(crate) fn client(redirects: bool) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .read_timeout(RESPONSE_TIMEOUT)
        .timeout(Duration::from_secs(300))
        .redirect(if redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        })
        .user_agent("GGScanner")
        .build()?)
}

/// Synchronous entry points already run on GUI workers or the CLI thread.
pub(crate) fn run<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(future)
}

pub(crate) async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    // Bound the entire header/redirect phase too, not just individual socket reads.
    Ok(tokio::time::timeout(RESPONSE_TIMEOUT, request.send())
        .await??
        .error_for_status()?)
}

pub(crate) async fn body(mut response: reqwest::Response, max_size: usize) -> Result<Vec<u8>> {
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= max_size as u64),
        "Download exceeds size limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= max_size.saturating_sub(bytes.len()),
            "Download exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
        let _ =
            TRANSFERRED.try_with(|count| count.fetch_add(chunk.len() as u64, Ordering::Relaxed));
    }
    Ok(bytes)
}

pub(crate) async fn get(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let response = send(client.get(url)).await?;
    ensure!(
        response.status().is_success(),
        "Unexpected download status {}",
        response.status()
    );
    body(response, limit).await
}

/// Validation is part of the race: fast HTML errors never beat valid artifacts.
pub(crate) async fn first_valid<T, F: Future<Output = Result<T>>, Factory: Fn() -> F>(
    factories: impl IntoIterator<Item = Factory>,
) -> Result<T> {
    first_valid_with_window(factories, SAMPLE_WINDOW).await
}

async fn first_valid_with_window<T, F: Future<Output = Result<T>>, Factory: Fn() -> F>(
    factories: impl IntoIterator<Item = Factory>,
    window: Duration,
) -> Result<T> {
    let factories: Vec<_> = factories.into_iter().collect();
    let mut pending: Vec<_> = (0..factories.len()).collect();
    let mut errors = Vec::new();
    let mut may_prune = true;
    while !pending.is_empty() {
        let counters: Vec<_> = (0..factories.len())
            .map(|_| Arc::new(AtomicU64::new(0)))
            .collect();
        let mut handles = Vec::new();
        let mut jobs = FuturesUnordered::new();
        for index in pending.drain(..) {
            let (handle, registration) = AbortHandle::new_pair();
            handles.push((index, handle));
            let job = TRANSFERRED.scope(
                Arc::clone(&counters[index]),
                Abortable::new(factories[index](), registration),
            );
            jobs.push(async move { (index, job.await) });
        }
        let sample = tokio::time::sleep(window);
        tokio::pin!(sample);
        let mut sampling = may_prune;
        while !jobs.is_empty() {
            tokio::select! {
                _ = &mut sample, if sampling => {
                    sampling = false;
                    let fastest = handles.iter().map(|(index, _)| counters[*index].load(Ordering::Relaxed)).max().unwrap_or(0);
                    // Require substantial progress, preserving all sources when the
                    // entire network is slow. Keep peers within half the leader's
                    // throughput; cancel clear laggards after the sample window.
                    if fastest >= 256 * 1024 {
                        for (index, handle) in &handles {
                            if counters[*index].load(Ordering::Relaxed) < fastest / 2 {
                                handle.abort();
                                pending.push(*index);
                            }
                        }
                    }
                },
                Some((index, result)) = jobs.next() => {
                    handles.retain(|(active, _)| *active != index);
                    match result {
                        Ok(Ok(value)) => return Ok(value),
                        Ok(Err(error)) => {
                            yas::log_debug!("下载候选失败: {:#}", "Download candidate failed: {:#}", error);
                            errors.push(format!("{error:#}"));
                        },
                        Err(_) => {}, // Slower source retained as a recovery candidate.
                    }
                }
            }
        }
        // If every fast source failed validation or stalled, restart cancelled
        // candidates once without pruning. A good slow source must remain usable.
        may_prune = false;
    }
    Err(anyhow!(
        "All download sources failed: {}",
        errors.join("; ")
    ))
}

/// Metadata may be stale on a proxy. Gather bounded responses, then let the
/// caller choose the newest version rather than accepting the fastest cache.
pub(crate) async fn collect_valid<T, F: Future<Output = Result<T>>>(
    jobs: impl IntoIterator<Item = F>,
) -> Vec<T> {
    let mut jobs: FuturesUnordered<_> = jobs.into_iter().collect();
    let deadline = tokio::time::sleep(RESPONSE_TIMEOUT);
    tokio::pin!(deadline);
    let mut values = Vec::new();
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            result = jobs.next() => match result {
                Some(Ok(value)) => values.push(value),
                Some(Err(error)) => yas::log_debug!("下载元数据失败: {:#}", "Metadata candidate failed: {:#}", error),
                None => break,
            }
        }
    }
    values
}

pub(crate) fn validated_bytes<T>(
    urls: &[String],
    limit: usize,
    validate: impl Fn(&[u8]) -> Result<T>,
) -> Result<T> {
    run(async {
        let client = client(true)?;
        first_valid(urls.iter().map(|url| {
            let client = &client;
            let validate = &validate;
            move || async move {
                let bytes = get(client, url, limit).await.with_context(|| url.clone())?;
                let value =
                    validate(&bytes).with_context(|| format!("Invalid download from {url}"))?;
                yas::log_debug!("下载源完成: {}", "Download source completed: {}", url);
                Ok(value)
            }
        }))
        .await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn hosted_data_never_gets_public_proxies() {
        for url in [
            "https://ggartifact.com/good/data.json",
            "https://hsr.ggartifact.com/good/data.json",
            "https://github.com.evil.example/file",
        ] {
            assert_eq!(github_sources(url), vec![url]);
        }
        assert_eq!(
            github_sources("https://github.com/a/b/releases/download/v1/app.exe").len(),
            3
        );
        assert_eq!(
            github_sources("https://api.github.com/repos/a/b/releases/latest").len(),
            2
        );
    }

    #[test]
    fn fast_error_page_does_not_win() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let worker = thread::spawn(move || {
            for request in server.incoming_requests().take(2) {
                let text = if request.url() == "/bad" {
                    "<html>proxy error</html>"
                } else {
                    "valid artifact"
                };
                request
                    .respond(tiny_http::Response::from_string(text))
                    .unwrap();
            }
        });
        let result = validated_bytes(
            &[format!("{base}/bad"), format!("{base}/good")],
            1024,
            |bytes| {
                ensure!(bytes == b"valid artifact", "invalid artifact");
                Ok(bytes.to_vec())
            },
        )
        .unwrap();
        assert_eq!(result, b"valid artifact");
        worker.join().unwrap();
    }

    #[test]
    fn winner_cancels_an_already_streaming_loser() {
        let slow = TcpListener::bind("127.0.0.1:0").unwrap();
        let fast = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let urls = vec![
            format!("http://{}", slow.local_addr().unwrap()),
            format!("http://{}", fast.server_addr()),
        ];
        let (started_tx, started_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let slow_worker = thread::spawn(move || {
            let (mut socket, _) = slow.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\nx",
                )
                .unwrap();
            started_tx.send(()).unwrap();
            let disconnected = match socket.read(&mut [0; 1]) {
                Ok(0) => true,
                Err(error) => error.kind() == std::io::ErrorKind::ConnectionReset,
                _ => false,
            };
            closed_tx.send(disconnected).unwrap();
        });
        let fast_worker = thread::spawn(move || {
            let request = fast.recv().unwrap();
            started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            request
                .respond(tiny_http::Response::from_string("winner"))
                .unwrap();
        });
        let result = validated_bytes(&urls, 1024 * 1024, |bytes| Ok(bytes.to_vec())).unwrap();
        assert_eq!(result, b"winner");
        assert!(closed_rx.recv_timeout(Duration::from_secs(3)).unwrap());
        slow_worker.join().unwrap();
        fast_worker.join().unwrap();
    }

    #[test]
    fn stalled_body_and_oversized_response_fail() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let worker = thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(tiny_http::Response::from_string("too large"))
                .unwrap();
        });
        assert!(validated_bytes(&[url], 3, |_| Ok(())).is_err());
        worker.join().unwrap();

        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            socket.read(&mut [0; 2048]).unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nx")
                .unwrap();
            thread::sleep(Duration::from_millis(400));
        });
        let error = run(async {
            let client = reqwest::Client::builder()
                .read_timeout(Duration::from_millis(80))
                .build()?;
            get(&client, &url, 1024).await
        })
        .unwrap_err();
        assert!(error.downcast_ref::<reqwest::Error>().unwrap().is_timeout());
        worker.join().unwrap();
    }

    #[test]
    fn metadata_compares_all_completed_responses() {
        let values = run(async {
            Ok(collect_valid([1, 3, 2].map(|value| async move {
                tokio::time::sleep(Duration::from_millis(value * 10)).await;
                Ok(value)
            }))
            .await)
        })
        .unwrap();
        assert_eq!(values.into_iter().max(), Some(3));
    }

    #[test]
    fn pruned_slow_source_is_retried_if_the_leader_fails() {
        let fast = TcpListener::bind("127.0.0.1:0").unwrap();
        let slow = TcpListener::bind("127.0.0.1:0").unwrap();
        let urls = [
            format!("http://{}", fast.local_addr().unwrap()),
            format!("http://{}", slow.local_addr().unwrap()),
        ];
        let fast_worker = thread::spawn(move || {
            let (mut socket, _) = fast.accept().unwrap();
            socket.read(&mut [0; 2048]).unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            socket.write_all(&vec![b'x'; 512 * 1024]).unwrap();
            thread::sleep(Duration::from_millis(300));
            // Drop a truncated response after establishing a clear throughput lead.
        });
        let slow_worker = thread::spawn(move || {
            let (mut socket, _) = slow.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx")
                .unwrap();
            let closed = socket.read(&mut [0; 1]);
            assert!(
                matches!(closed, Ok(0))
                    || closed
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
            );
            drop(socket);
            let (mut socket, _) = slow.accept().unwrap();
            socket.read(&mut [0; 2048]).unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nwinner",
                )
                .unwrap();
        });
        let bytes = run(async {
            let client = client(true)?;
            first_valid_with_window(
                urls.iter().map(|url| {
                    let client = &client;
                    move || async move { get(client, url, 2 * 1024 * 1024).await }
                }),
                Duration::from_millis(100),
            )
            .await
        })
        .unwrap();
        assert_eq!(bytes, b"winner");
        fast_worker.join().unwrap();
        slow_worker.join().unwrap();
    }
}
