/// UDP packet capture for Genshin Impact traffic.
///
/// Ported from irminsul's `capture.rs`. Filters UDP on ports 22101–22102.
///
/// The backend is Windows-only (pktmon). Other platforms compile the same
/// public API but `PacketCapture::new` fails with a clear error so callers
/// and the monitor logic stay portable.
use std::fmt::{Debug, Display};

use anyhow::Error;
#[cfg(target_os = "windows")]
use futures::stream::FusedStream;
#[cfg(target_os = "windows")]
use futures::StreamExt;
#[cfg(target_os = "windows")]
use pktmon::filter::{PktMonFilter, TransportProtocol};
#[cfg(target_os = "windows")]
use pktmon::{Capture, Packet};

pub const PORT_RANGE: (u16, u16) = (22101, 22102);

#[derive(Debug)]
#[allow(dead_code)]
pub enum CaptureError {
    Filter(Error),
    Capture { has_captured: bool, error: Error },
    CaptureClosed,
}

impl Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Filter(e) => write!(f, "Filter error: {}", e),
            CaptureError::Capture {
                has_captured,
                error,
            } => write!(
                f,
                "Capture error (has_captured = {}): {}",
                has_captured, error
            ),
            CaptureError::CaptureClosed => write!(f, "Capture closed"),
        }
    }
}

pub type Result<T> = std::result::Result<T, CaptureError>;

#[cfg(target_os = "windows")]
pub struct PacketCapture {
    stream: Box<dyn FusedStream<Item = Packet> + Unpin + Send>,
}

#[cfg(target_os = "windows")]
impl PacketCapture {
    pub fn new() -> Result<Self> {
        let mut capture = Capture::new().map_err(|e| CaptureError::Capture {
            has_captured: false,
            error: yas::utils::packet_capture::initialization_error(e),
        })?;

        let filter1 = PktMonFilter {
            name: "UDP Filter".to_string(),
            transport_protocol: Some(TransportProtocol::UDP),
            port: PORT_RANGE.0.into(),
            ..PktMonFilter::default()
        };
        capture
            .add_filter(filter1)
            .map_err(|e| CaptureError::Filter(e.into()))?;

        let filter2 = PktMonFilter {
            name: "UDP Filter".to_string(),
            transport_protocol: Some(TransportProtocol::UDP),
            port: PORT_RANGE.1.into(),
            ..PktMonFilter::default()
        };
        capture
            .add_filter(filter2)
            .map_err(|e| CaptureError::Filter(e.into()))?;

        let stream = capture.stream().map_err(|error| CaptureError::Capture {
            has_captured: false,
            error: Error::from(error).context("pktmon packet stream could not be started"),
        })?;

        Ok(Self {
            stream: Box::new(stream.boxed().fuse()),
        })
    }

    pub async fn next_packet(&mut self) -> Result<Vec<u8>> {
        futures::select! {
            packet = self.stream.select_next_some() => {
                Ok(packet.payload.to_vec().clone())
            },
            complete => Err(CaptureError::CaptureClosed),
        }
    }
}

/// Timeout for the reader thread's `poll(2)`, so a stopped capture exits
/// promptly even when every interface is idle.
#[cfg(target_os = "linux")]
const POLL_TIMEOUT_MS: libc::c_int = 100;

/// Scratch buffer for one frame; no supported link type is larger.
#[cfg(target_os = "linux")]
const FRAME_BUFFER_LEN: usize = 65_536;

#[cfg(target_os = "linux")]
pub struct PacketCapture {
    packets: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl PacketCapture {
    pub fn new() -> Result<Self> {
        let sockets = open_capture_sockets()?;
        let (packet_tx, packets) = tokio::sync::mpsc::unbounded_channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_stop = std::sync::Arc::clone(&stop);
        let reader = std::thread::Builder::new()
            .name("gi-packet-capture".into())
            .spawn(move || read_packets(&sockets, &packet_tx, &reader_stop))
            .map_err(|error| CaptureError::Capture {
                has_captured: false,
                error: Error::from(error).context("packet capture thread could not be started"),
            })?;
        Ok(Self {
            packets,
            stop,
            reader: Some(reader),
        })
    }

    pub async fn next_packet(&mut self) -> Result<Vec<u8>> {
        self.packets.recv().await.ok_or(CaptureError::CaptureClosed)
    }
}

#[cfg(target_os = "linux")]
impl Drop for PacketCapture {
    fn drop(&mut self) {
        // The reader polls with a short timeout, so flagging plus joining ends
        // it well within one frame of the last read, and the sockets close only
        // after the reader has stopped touching them.
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Interfaces that carry IPv4 traffic, deduplicated by index and sorted by name.
#[cfg(target_os = "linux")]
fn capture_interfaces() -> std::io::Result<Vec<(String, libc::c_int)>> {
    // SAFETY: `getifaddrs`/`freeifaddrs` are paired, the linked list is only
    // read while alive, and every pointer is null-checked before use.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut interfaces: Vec<(String, libc::c_int)> = Vec::new();
        let mut entry = head;
        while !entry.is_null() {
            let interface = &*entry;
            let address = interface.ifa_addr;
            if !address.is_null() && libc::c_int::from((*address).sa_family) == libc::AF_INET {
                let name = std::ffi::CStr::from_ptr(interface.ifa_name)
                    .to_string_lossy()
                    .into_owned();
                let index = libc::if_nametoindex(interface.ifa_name) as libc::c_int;
                if index != 0 && !interfaces.iter().any(|(_, known)| *known == index) {
                    interfaces.push((name, index));
                }
            }
            entry = interface.ifa_next;
        }
        libc::freeifaddrs(head);
        interfaces.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(interfaces)
    }
}

#[cfg(target_os = "linux")]
fn open_capture_sockets() -> Result<Vec<std::os::fd::OwnedFd>> {
    let interfaces = capture_interfaces().map_err(|error| CaptureError::Capture {
        has_captured: false,
        error: Error::from(error).context("network interfaces could not be listed"),
    })?;
    if interfaces.is_empty() {
        return Err(CaptureError::Capture {
            has_captured: false,
            error: Error::msg("no IPv4 network interface is available for packet capture"),
        });
    }

    let mut sockets = Vec::with_capacity(interfaces.len());
    for (name, index) in interfaces {
        let socket = open_interface_socket(index).map_err(|error| CaptureError::Capture {
            has_captured: false,
            error: Error::from(error).context(format!(
                "raw capture socket for interface {name} could not be opened"
            )),
        })?;
        sockets.push(socket);
    }
    Ok(sockets)
}

#[cfg(target_os = "linux")]
fn open_interface_socket(index: libc::c_int) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;

    let protocol = i32::from((libc::ETH_P_ALL as u16).to_be());
    // SAFETY: plain libc socket setup. The descriptor is owned immediately so
    // every early return still closes it, and every pointer outlives its call.
    unsafe {
        let fd = libc::socket(
            libc::AF_PACKET,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            protocol,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let socket = std::os::fd::OwnedFd::from_raw_fd(fd);

        let mut address: libc::sockaddr_ll = std::mem::zeroed();
        address.sll_family = libc::AF_PACKET as libc::c_ushort;
        address.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
        address.sll_ifindex = index;
        if libc::bind(
            fd,
            &address as *const libc::sockaddr_ll as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }

        attach_udp_filter(fd)?;

        // Best effort: a larger receive buffer absorbs bursts while the reader
        // thread is scheduling; the kernel may clamp the requested size.
        let buffer_size: libc::c_int = 8 * 1024 * 1024;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &buffer_size as *const libc::c_int as *const libc::c_void,
            std::mem::size_of_val(&buffer_size) as libc::socklen_t,
        );
        Ok(socket)
    }
}

/// Reject everything except IPv4 UDP in the kernel.
///
/// Port selection stays in userspace (`is_game_udp_frame`) so the program is
/// small enough to review by hand; dropping TCP here removes the bulk of a
/// desktop's traffic without depending on the game's server address.
#[cfg(target_os = "linux")]
fn attach_udp_filter(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    const ACCEPT_LEN: u32 = 262_144;
    const ETHERTYPE_IPV4: u32 = 0x0800;
    const IPPROTO_UDP: u32 = 17;

    fn filter(code: u32, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
        libc::sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        }
    }

    // 0: A = ethertype; 1: IPv4?; 2: A = IPv4 protocol; 3: UDP?;
    // 4: accept; 5: reject.
    let program = [
        filter(libc::BPF_LD | libc::BPF_H | libc::BPF_ABS, 0, 0, 12),
        filter(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            0,
            3,
            ETHERTYPE_IPV4,
        ),
        filter(libc::BPF_LD | libc::BPF_B | libc::BPF_ABS, 0, 0, 23),
        filter(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            0,
            1,
            IPPROTO_UDP,
        ),
        filter(libc::BPF_RET | libc::BPF_K, 0, 0, ACCEPT_LEN),
        filter(libc::BPF_RET | libc::BPF_K, 0, 0, 0),
    ];
    let mut program = program;
    let mut sock_fprog = libc::sock_fprog {
        len: program.len() as libc::c_ushort,
        filter: program.as_mut_ptr(),
    };
    // SAFETY: the program array and sock_fprog outlive the call; the kernel
    // copies the program before `setsockopt` returns.
    let status = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ATTACH_FILTER,
            &mut sock_fprog as *mut libc::sock_fprog as *const libc::c_void,
            std::mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_packets(
    sockets: &[std::os::fd::OwnedFd],
    packets: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    stop: &std::sync::atomic::AtomicBool,
) {
    use std::os::fd::AsRawFd;

    let mut poll_fds: Vec<libc::pollfd> = sockets
        .iter()
        .map(|socket| libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let mut buffer = vec![0u8; FRAME_BUFFER_LEN];
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        for poll_fd in &mut poll_fds {
            poll_fd.revents = 0;
        }
        // SAFETY: the poll list and buffer outlive the call; lengths are passed
        // explicitly and every ready descriptor belongs to `sockets`.
        let ready = unsafe {
            libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as libc::nfds_t,
                POLL_TIMEOUT_MS,
            )
        };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        for (index, poll_fd) in poll_fds.iter().enumerate() {
            if poll_fd.revents & libc::POLLIN == 0 {
                continue;
            }
            // SAFETY: `index` addresses a live socket and `buffer` is a valid
            // writable region of its full length.
            let length = unsafe {
                libc::recv(
                    sockets[index].as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if length <= 0 {
                continue;
            }
            let frame = &buffer[..length as usize];
            if !is_game_udp_frame(frame) {
                continue;
            }
            if packets.send(frame.to_vec()).is_err() {
                return;
            }
        }
    }
}

/// True when `frame` is an Ethernet IPv4/UDP frame touching a game port.
///
/// Only the first fragment carries ports; later fragments can never match and
/// are dropped, which matches the Windows port filter.
#[cfg(target_os = "linux")]
pub(crate) fn is_game_udp_frame(frame: &[u8]) -> bool {
    const ETHERNET_HEADER_LEN: usize = 14;
    const MIN_IPV4_HEADER_LEN: usize = 20;
    const UDP_HEADER_LEN: usize = 8;
    const ETHERTYPE_IPV4: u16 = 0x0800;
    const IPPROTO_UDP: u8 = 17;

    if frame.len() < ETHERNET_HEADER_LEN + MIN_IPV4_HEADER_LEN + UDP_HEADER_LEN {
        return false;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE_IPV4 {
        return false;
    }
    let ip = &frame[ETHERNET_HEADER_LEN..];
    if ip[0] >> 4 != 4 {
        return false;
    }
    let header_len = usize::from(ip[0] & 0x0f) * 4;
    if header_len < MIN_IPV4_HEADER_LEN || ip[9] != IPPROTO_UDP {
        return false;
    }
    if u16::from_be_bytes([ip[6], ip[7]]) & 0x1fff != 0 {
        return false;
    }
    let udp_start = ETHERNET_HEADER_LEN + header_len;
    let Some(ports) = frame.get(udp_start..udp_start + 4) else {
        return false;
    };
    let source = u16::from_be_bytes([ports[0], ports[1]]);
    let destination = u16::from_be_bytes([ports[2], ports[3]]);
    (PORT_RANGE.0..=PORT_RANGE.1).contains(&source)
        || (PORT_RANGE.0..=PORT_RANGE.1).contains(&destination)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub struct PacketCapture {
    _platform: (),
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
impl PacketCapture {
    pub fn new() -> Result<Self> {
        Err(CaptureError::Capture {
            has_captured: false,
            error: Error::msg(
                "Packet capture has no native backend on this platform; use the Scanner to export data by reading the game screen.",
            ),
        })
    }

    pub async fn next_packet(&mut self) -> Result<Vec<u8>> {
        Err(CaptureError::CaptureClosed)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{is_game_udp_frame, PORT_RANGE};

    /// Build an Ethernet/IPv4/UDP frame; `options` adds IPv4 header options.
    fn frame(protocol: u8, source: u16, destination: u16, options: usize) -> Vec<u8> {
        let header_len = 20 + options;
        let mut bytes = vec![0u8; 14 + header_len + 8 + 4];
        bytes[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        bytes[14] = 0x40 | u8::try_from(header_len / 4).expect("header length fits IHL");
        bytes[14 + 1] = 0;
        bytes[14 + 2..14 + 4].copy_from_slice(&((header_len + 8) as u16).to_be_bytes());
        bytes[14 + 6] = 0x40; // Don't fragment.
        bytes[14 + 9] = protocol;
        bytes[14 + 12..14 + 16].copy_from_slice(&[127, 0, 0, 1]);
        bytes[14 + 16..14 + 20].copy_from_slice(&[127, 0, 0, 1]);
        let udp = 14 + header_len;
        bytes[udp..udp + 2].copy_from_slice(&source.to_be_bytes());
        bytes[udp + 2..udp + 4].copy_from_slice(&destination.to_be_bytes());
        bytes
    }

    #[test]
    fn accepts_udp_on_either_game_port() {
        let (low, high) = PORT_RANGE;
        assert!(is_game_udp_frame(&frame(17, low, 443, 0)));
        assert!(is_game_udp_frame(&frame(17, 443, high, 0)));
        assert!(is_game_udp_frame(&frame(17, high, low, 0)));
    }

    #[test]
    fn rejects_other_ports_and_protocols() {
        let (low, _) = PORT_RANGE;
        assert!(!is_game_udp_frame(&frame(17, 443, 8080, 0)));
        assert!(!is_game_udp_frame(&frame(17, low - 1, 443, 0)));
        // TCP on a game port is not game traffic.
        assert!(!is_game_udp_frame(&frame(6, low, 443, 0)));
    }

    #[test]
    fn accepts_ipv4_header_options() {
        assert!(is_game_udp_frame(&frame(17, PORT_RANGE.0, 443, 4)));
    }

    #[test]
    fn rejects_non_ipv4_and_truncated_frames() {
        let mut tagged = frame(17, PORT_RANGE.0, 443, 0);
        tagged[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
        assert!(!is_game_udp_frame(&tagged));

        let mut short = frame(17, PORT_RANGE.0, 443, 0);
        short.truncate(30);
        assert!(!is_game_udp_frame(&short));

        assert!(!is_game_udp_frame(&[]));
    }

    #[test]
    fn rejects_non_initial_fragments_without_ports() {
        let mut fragment = frame(17, PORT_RANGE.0, 443, 0);
        // Fragment offset = 1 (later fragment): the UDP header is absent.
        fragment[14 + 6..14 + 8].copy_from_slice(&0x2001u16.to_be_bytes());
        assert!(!is_game_udp_frame(&fragment));
    }
}
