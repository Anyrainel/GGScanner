//! Opt-in capture evidence, including frames that cannot be decrypted.
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::monitor::CaptureState;

pub(super) struct PacketDump {
    directory: PathBuf,
    packets: BufWriter<File>,
    diagnostics: BufWriter<File>,
    packet_count: u64,
    command_count: u64,
}

impl PacketDump {
    pub fn new(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let directory = root.join(format!("gi_{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory)?;
        let mut packets = BufWriter::new(create_file(&directory.join("packets.pcap"))?);
        // Classic PCAP, little endian, microsecond timestamps, Ethernet frames.
        for value in [0xa1b2c3d4u32, 0x00040002, 0, 0, 262_144, 1] {
            packets.write_all(&value.to_le_bytes())?;
        }
        packets.flush()?;
        let diagnostics = BufWriter::new(create_file(&directory.join("diagnostics.jsonl"))?);
        fs::write(directory.join("README.txt"),
            "Genshin capture debug session\npackets.pcap: all raw pktmon Ethernet frames in arrival order, saved before decoding (including failed packets).\nNNNNNN_cmdID.bin: decoded command data, including its PacketHead, as in older dumps.\ndiagnostics.jsonl: state after each raw packet and at capture end; rawPacket is the 1-based PCAP record number, commands is the number of decoded command files.\n")?;
        Ok(Self {
            directory,
            packets,
            diagnostics,
            packet_count: 0,
            command_count: 0,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn write_packet(&mut self, bytes: &[u8]) -> io::Result<()> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        let seconds = u32::try_from(timestamp.as_secs()).map_err(io::Error::other)?;
        let length = u32::try_from(bytes.len()).map_err(io::Error::other)?;
        for value in [seconds, timestamp.subsec_micros(), length, length] {
            self.packets.write_all(&value.to_le_bytes())?;
        }
        self.packets.write_all(bytes)?;
        // Persist before the decoder runs, even if it stalls or panics.
        self.packets.flush()?;
        self.packet_count += 1;
        Ok(())
    }

    pub fn write_command(&mut self, command_id: u16, bytes: &[u8]) -> io::Result<()> {
        let path = self
            .directory
            .join(format!("{:06}_cmd{command_id}.bin", self.command_count));
        self.command_count += 1;
        create_file(&path)?.write_all(bytes)
    }

    pub fn write_state(&mut self, event: &str, state: &CaptureState) -> io::Result<()> {
        serde_json::to_writer(
            &mut self.diagnostics,
            &serde_json::json!({
                "event": event,
                "rawPacket": self.packet_count,
                "commands": self.command_count,
                "state": state,
            }),
        )?;
        self.diagnostics.write_all(b"\n")?;
        self.diagnostics.flush()
    }
}

fn create_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_preserve_ordered_frames_commands_and_diagnostics_without_overwriting() {
        let root = std::env::temp_dir().join(format!("gi-dump-test-{}", uuid::Uuid::new_v4()));
        let mut first = PacketDump::new(&root).unwrap();
        let mut second = PacketDump::new(&root).unwrap();
        assert_ne!(first.directory(), second.directory());
        first.write_packet(&[0, 128, 255]).unwrap();
        first.write_packet(&[9, 8]).unwrap();
        first.write_command(42, &[3, 2, 1]).unwrap();
        first.write_state("end", &CaptureState::default()).unwrap();
        second.write_command(42, &[7]).unwrap();

        let pcap = fs::read(first.directory().join("packets.pcap")).unwrap();
        assert_eq!(&pcap[..4], &[0xd4, 0xc3, 0xb2, 0xa1]);
        assert_eq!(&pcap[4..8], &[2, 0, 4, 0]);
        assert_eq!(u32::from_le_bytes(pcap[20..24].try_into().unwrap()), 1);
        let mut offset = 24;
        for expected in [&[0, 128, 255][..], &[9, 8][..]] {
            let captured =
                u32::from_le_bytes(pcap[offset + 8..offset + 12].try_into().unwrap()) as usize;
            let original =
                u32::from_le_bytes(pcap[offset + 12..offset + 16].try_into().unwrap()) as usize;
            assert_eq!(captured, expected.len());
            assert_eq!(original, captured);
            offset += 16;
            assert_eq!(&pcap[offset..offset + captured], expected);
            offset += captured;
        }
        assert_eq!(offset, pcap.len());
        assert_eq!(
            fs::read(first.directory().join("000000_cmd42.bin")).unwrap(),
            [3, 2, 1]
        );
        assert_eq!(
            fs::read(second.directory().join("000000_cmd42.bin")).unwrap(),
            [7]
        );
        let record: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(first.directory().join("diagnostics.jsonl")).unwrap(),
        )
        .unwrap();
        assert_eq!(record["rawPacket"], 2);
        assert_eq!(record["commands"], 1);
        assert_eq!(record["event"], "end");
        assert_eq!(
            fs::read(second.directory().join("packets.pcap"))
                .unwrap()
                .len(),
            24
        );
        drop(first);
        drop(second);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_dump_root_reports_failure() {
        let blocker =
            std::env::temp_dir().join(format!("gi-dump-blocked-{}", uuid::Uuid::new_v4()));
        fs::write(&blocker, b"file").unwrap();
        assert!(PacketDump::new(&blocker.join("invalid")).is_err());
        fs::remove_file(blocker).unwrap();
    }
}
