//! M1 acceptance: byte-for-byte round-trip of every packet in the spec's
//! `vectors/` golden captures (CC0 protocol facts from link-wire-spec).
//!
//! The spec checkout is located via `LINK_WIRE_SPEC_DIR`, falling back to a
//! sibling `link-wire-spec` checkout. Without it the tests skip (CI sets
//! `REQUIRE_VECTORS=1` to make absence a failure).

use std::path::PathBuf;

use tactus_wire::{audio, discovery, sync};

mod pcap {
    //! Minimal classic-pcap reader: just enough to pull UDP payloads out of
    //! the vector captures (Ethernet and LINUX_SLL2 link types, IPv4).

    pub struct Packet {
        /// UDP payload bytes.
        pub payload: Vec<u8>,
    }

    fn u16be(b: &[u8]) -> u16 {
        u16::from_be_bytes([b[0], b[1]])
    }

    pub fn udp_payloads(data: &[u8]) -> Vec<Packet> {
        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let (le, _nanos) = match magic {
            0xa1b2_c3d4 => (true, false),
            0xa1b2_3c4d => (true, true),
            0xd4c3_b2a1 => (false, false),
            0x4d3c_b2a1 => (false, true),
            _ => panic!("not a classic pcap file"),
        };
        let u32f = |b: &[u8]| -> u32 {
            let arr: [u8; 4] = b.try_into().unwrap();
            if le {
                u32::from_le_bytes(arr)
            } else {
                u32::from_be_bytes(arr)
            }
        };
        let linktype = u32f(&data[20..24]);
        let link_header = match linktype {
            1 => 14,   // Ethernet II
            276 => 20, // LINUX_SLL2
            other => panic!("unsupported pcap link type {other}"),
        };

        let mut packets = Vec::new();
        let mut at = 24;
        while at + 16 <= data.len() {
            let incl_len = u32f(&data[at + 8..at + 12]) as usize;
            let frame = &data[at + 16..at + 16 + incl_len];
            at += 16 + incl_len;

            // Only IPv4 frames carry protocol traffic in these captures.
            let ethertype_ok = match linktype {
                1 => u16be(&frame[12..14]) == 0x0800,
                _ => u16be(&frame[0..2]) == 0x0800,
            };
            if !ethertype_ok {
                continue;
            }
            let ip = &frame[link_header..];
            assert_eq!(ip[0] >> 4, 4, "IPv4 expected");
            let ihl = ((ip[0] & 0x0f) as usize) * 4;
            assert_eq!(
                u16be(&ip[6..8]) & 0x3fff,
                0,
                "fragmented datagrams not expected in vectors"
            );
            if ip[9] != 17 {
                continue; // not UDP
            }
            let udp = &ip[ihl..];
            let udp_len = u16be(&udp[4..6]) as usize;
            packets.push(Packet {
                payload: udp[8..udp_len].to_vec(),
            });
        }
        packets
    }
}

fn spec_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("LINK_WIRE_SPEC_DIR") {
        return Some(PathBuf::from(dir));
    }
    let sibling = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../link-wire-spec");
    sibling.is_dir().then_some(sibling)
}

fn vectors() -> Option<Vec<PathBuf>> {
    let Some(dir) = spec_dir() else {
        assert!(
            std::env::var("REQUIRE_VECTORS").is_err(),
            "REQUIRE_VECTORS set but no spec checkout found \
             (set LINK_WIRE_SPEC_DIR)"
        );
        eprintln!("skipping vector tests: no link-wire-spec checkout found");
        return None;
    };
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir.join("vectors"))
        .expect("vectors/ directory")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "pcap"))
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no .pcap files under vectors/");
    Some(out)
}

#[derive(Default)]
struct Counts {
    discovery: usize,
    sync: usize,
    audio: usize,
}

/// Decode a captured datagram and re-encode it; the result must be
/// byte-identical.
fn roundtrip(datagram: &[u8], counts: &mut Counts) {
    let magic: [u8; 8] = datagram[..8].try_into().unwrap();
    let reencoded = match magic {
        discovery::MAGIC => {
            counts.discovery += 1;
            let frame = discovery::decode(datagram).expect("discovery decode");
            assert_eq!(frame.group_id, 0, "vectors carry groupId 0 throughout");
            discovery::encode(&frame)
        }
        sync::MAGIC => {
            counts.sync += 1;
            match sync::decode(datagram).expect("sync decode") {
                sync::Message::Ping(p) => sync::encode_ping(
                    p.host_time.expect("captured pings carry __ht"),
                    p.prev_ghost_time,
                ),
                sync::Message::Pong(p) => sync::encode_pong(p.session, p.ghost_time, &p.echo),
            }
        }
        audio::MAGIC => {
            counts.audio += 1;
            let frame = audio::decode(datagram).expect("audio decode");
            assert_eq!(frame.group_id, 0, "vectors carry groupId 0 throughout");
            audio::encode(&frame)
        }
        _ => panic!("datagram with unknown frame magic in vector"),
    };
    assert_eq!(
        reencoded, datagram,
        "re-encoded bytes differ from captured bytes"
    );
}

#[test]
fn every_vector_packet_roundtrips_byte_for_byte() {
    let Some(files) = vectors() else { return };
    let mut grand_total = 0;
    for file in files {
        let data = std::fs::read(&file).unwrap();
        let packets = pcap::udp_payloads(&data);
        let mut counts = Counts::default();
        for p in &packets {
            roundtrip(&p.payload, &mut counts);
        }
        eprintln!(
            "{}: {} packets (discovery {}, sync {}, audio {})",
            file.file_name().unwrap().to_string_lossy(),
            packets.len(),
            counts.discovery,
            counts.sync,
            counts.audio,
        );
        assert!(counts.discovery > 0, "every vector contains discovery");
        assert!(counts.sync > 0, "every vector contains sync measurement");
        grand_total += packets.len();
    }
    // The manifests pin the per-capture totals; together the five released
    // captures decode 4346 protocol messages.
    assert_eq!(grand_total, 4346);
}

#[test]
fn audio_vector_exercises_every_audio_message_type() {
    let Some(files) = vectors() else { return };
    let file = files
        .iter()
        .find(|f| f.to_string_lossy().contains("audio-channel-lifecycle"))
        .expect("audio-channel-lifecycle.pcap present");
    let data = std::fs::read(file).unwrap();

    let mut announcements = 0;
    let mut byes = 0;
    let mut pongs = 0;
    let mut requests = 0;
    let mut stops = 0;
    let mut buffers = 0;
    for p in pcap::udp_payloads(&data) {
        if p.payload[..8] != audio::MAGIC {
            continue;
        }
        match audio::decode(&p.payload).unwrap().message {
            audio::Message::PeerAnnouncement { .. } => announcements += 1,
            audio::Message::ChannelByes { .. } => byes += 1,
            audio::Message::Pong { .. } => pongs += 1,
            audio::Message::ChannelRequest { .. } => requests += 1,
            audio::Message::StopChannelRequest { .. } => stops += 1,
            audio::Message::AudioBuffer(buf) => {
                buffers += 1;
                assert_eq!(buf.codec, audio::CODEC_PCM_I16);
                assert!(buf.sample_data.len() <= audio::SAMPLE_BYTE_CAP);
                assert!(!buf.chunks.is_empty());
            }
        }
    }
    // Manifest-pinned counts for this capture.
    assert_eq!(
        (announcements, byes, pongs, requests, stops, buffers),
        (132, 1, 132, 3, 1, 2098)
    );
}
