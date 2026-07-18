//! Property tests: every packet we can serialize parses back identically,
//! and the parser never panics on arbitrary input (it faces the internet).

use jam_protocol::packet::{
    parse_datagram, AudioPacket, Codec, Control, DenyReason, Header, PacketType, Payload,
    PlayerInfo,
};
use jam_protocol::{MAX_DATAGRAM, MAX_NAME_LEN, ROOM_CODE_LEN};
use proptest::prelude::*;

fn arb_codec() -> impl Strategy<Value = Codec> {
    prop_oneof![Just(Codec::Opus), Just(Codec::PcmS16), Just(Codec::PcmF32)]
}

fn arb_deny() -> impl Strategy<Value = DenyReason> {
    prop_oneof![
        Just(DenyReason::BadRoomCode),
        Just(DenyReason::RoomFull),
        Just(DenyReason::VersionMismatch),
    ]
}

fn arb_name() -> impl Strategy<Value = String> {
    // Names are length-limited in *bytes*; multibyte chars exercise the
    // char-boundary truncation path.
    proptest::string::string_regex("[a-zA-Z0-9é✓ ]{0,10}").unwrap()
}

fn arb_player() -> impl Strategy<Value = PlayerInfo> {
    (any::<u16>(), arb_name()).prop_map(|(id, name)| PlayerInfo { id, name })
}

fn arb_control() -> impl Strategy<Value = Control> {
    prop_oneof![
        (
            any::<u8>(),
            proptest::array::uniform6(any::<u8>()),
            arb_name(),
            any::<u8>(),
            any::<u8>()
        )
            .prop_map(|(v, code, name, cm, fm)| Control::Hello {
                proto_version: v,
                room_code: code,
                name,
                codec_mask: cm,
                frame_mask: fm,
            }),
        (
            any::<u16>(),
            any::<u16>(),
            any::<u32>(),
            any::<u16>(),
            arb_codec(),
            proptest::collection::vec(arb_player(), 0..5)
        )
            .prop_map(|(id, sid, rate, fs, codec, roster)| Control::Welcome {
                assigned_id: id,
                session_id: sid,
                sample_rate: rate,
                frame_samples: fs,
                codec,
                roster,
            }),
        arb_deny().prop_map(|reason| Control::Deny { reason }),
        any::<u64>().prop_map(|t1_us| Control::Ping { t1_us }),
        (any::<u64>(), any::<u64>(), any::<u64>()).prop_map(|(t1_us, t2_us, t3_us)| {
            Control::Pong {
                t1_us,
                t2_us,
                t3_us,
            }
        }),
        Just(Control::Keepalive),
        proptest::collection::vec(arb_player(), 0..5)
            .prop_map(|players| Control::Roster { players }),
        Just(Control::Bye),
        (any::<u16>(), any::<u8>()).prop_map(|(acked_seq, acked_type)| Control::Ack {
            acked_seq,
            acked_type,
        }),
    ]
}

fn arb_header_for(ptype: PacketType) -> impl Strategy<Value = Header> {
    (any::<u16>(), any::<u16>(), any::<u16>()).prop_map(move |(session_id, sender_id, seq)| {
        Header {
            ptype,
            session_id,
            sender_id,
            seq,
        }
    })
}

proptest! {
    #[test]
    fn control_roundtrip(control in arb_control(), session_id: u16, sender_id: u16, seq: u16) {
        let header = Header { ptype: control.packet_type(), session_id, sender_id, seq };
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = control.write(&header, &mut buf);
        let (parsed_header, payload) = parse_datagram(&buf[..n]).expect("roundtrip parse");
        prop_assert_eq!(parsed_header, header);
        match payload {
            Payload::Control(parsed) => {
                // Serialization truncates over-long names; reserialize the
                // parsed value and check it is a fixed point.
                let mut buf2 = [0u8; MAX_DATAGRAM];
                let n2 = parsed.write(&header, &mut buf2);
                prop_assert_eq!(&buf[..n], &buf2[..n2]);
                if control_names_within_limit(&control) {
                    prop_assert_eq!(parsed, control);
                }
            }
            Payload::Audio(_) => prop_assert!(false, "control parsed as audio"),
        }
    }

    #[test]
    fn audio_roundtrip(
        header in arb_header_for(PacketType::Audio),
        codec in arb_codec(),
        timestamp: u32,
        payload in proptest::collection::vec(any::<u8>(), 0..600),
        redundant in proptest::option::of(proptest::collection::vec(any::<u8>(), 0..600)),
    ) {
        let pkt = AudioPacket {
            timestamp,
            codec,
            payload: &payload,
            redundant: redundant.as_deref(),
        };
        let mut buf = [0u8; MAX_DATAGRAM * 2];
        let n = pkt.write(&header, &mut buf);
        let (parsed_header, parsed) = parse_datagram(&buf[..n]).expect("roundtrip parse");
        prop_assert_eq!(parsed_header, header);
        match parsed {
            Payload::Audio(a) => {
                prop_assert_eq!(a.timestamp, timestamp);
                prop_assert_eq!(a.codec, codec);
                prop_assert_eq!(a.payload, &payload[..]);
                prop_assert_eq!(a.redundant, redundant.as_deref());
            }
            Payload::Control(_) => prop_assert!(false, "audio parsed as control"),
        }
    }

    /// The parser must reject or accept — never panic — on arbitrary bytes.
    #[test]
    fn parser_never_panics(data in proptest::collection::vec(any::<u8>(), 0..1500)) {
        let _ = parse_datagram(&data);
    }

    /// Any truncation of a valid packet must not panic either.
    #[test]
    fn truncated_packets_never_panic(control in arb_control(), cut in 0usize..64) {
        let header = Header { ptype: control.packet_type(), session_id: 1, sender_id: 2, seq: 3 };
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = control.write(&header, &mut buf);
        let cut = cut.min(n);
        let _ = parse_datagram(&buf[..n - cut]);
    }
}

fn control_names_within_limit(c: &Control) -> bool {
    let name_ok = |s: &str| s.len() <= MAX_NAME_LEN;
    match c {
        Control::Hello {
            name, room_code, ..
        } => name_ok(name) && room_code.len() == ROOM_CODE_LEN,
        Control::Welcome { roster, .. } => roster.iter().all(|p| name_ok(&p.name)),
        Control::Roster { players } => players.iter().all(|p| name_ok(&p.name)),
        _ => true,
    }
}
