# Genshin capture debug dumps

Enable packet dumping before starting a capture. Each attempt creates a separate
`debug_capture/gi_<session>/` folder beside the executable. Earlier attempts are
preserved, including failures and runs with no decoded commands. Existing flat
dump files are left in place.

- `packets.pcap` contains every raw pktmon Ethernet frame passed to the decoder,
  in arrival order with capture timestamps. Frames are flushed to disk before
  decoding, including frames that fail parsing, reassembly, or decryption. The
  file uses standard PCAP format and can be opened in Wireshark or replayed by
  passing its frame records to `GameSniffer::receive_packet` in order.
- `NNNNNN_cmdID.bin` preserves decoded command data in the previous dump format
  (PacketHead plus protobuf body). Command numbering starts at zero per session.
- `diagnostics.jsonl` records state after each raw frame and at capture end.
  `rawPacket` identifies the 1-based PCAP record, and `commands` gives the number
  of decoded command files so far. Rejected command counts, key exchange state,
  transport errors, and category counts distinguish decoding failures from
  unrecognized account data. A raw frame saved before a decoder stall or panic
  may have no corresponding diagnostics line.

The PCAP timestamps describe arrival at the monitor, rather than kernel capture
time. Raw frames include the login traffic required to investigate session-key
failures. Keep the entire session folder for debugging. Dumps are only created
when packet dumping is enabled. A disk write failure is logged and disables
further dumping for that session while capture continues.
