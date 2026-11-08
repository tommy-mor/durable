//! Strict envelope: a line with an unknown envelope key and a valid CRC is
//! refused everywhere (scan, parse, open, record decode) and never treated as
//! residue. Also: the streaming scan agrees with itself at every buffer size.

mod common;

use std::io::BufReader;

use common::*;
use durable::runtime::{parse_sealed, render_sealed, scan, scan_reader, seal_crc};
use durable::{Error, Record, Runtime};
use proptest::prelude::*;

/// A sealed line with one extra envelope key, CRC computed over it.
fn with_extra_key(
    seq: u64,
    ts: u64,
    from: u64,
    to: u64,
    raw: &str,
    key: &str,
    value: &str,
) -> String {
    let prefix = format!(
        "{{\"seq\":{seq},\"ts_ms\":{ts},\"from\":{from},\"to\":{to},\"{key}\":{value},\"event\":{raw}"
    );
    let crc = seal_crc(prefix.as_bytes());
    format!("{prefix},\"crc\":\"{crc}\"}}")
}

fn joined(lines: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in lines {
        out.extend_from_slice(l.as_bytes());
        out.push(b'\n');
    }
    out
}

#[test]
fn extra_key_with_valid_crc_is_refused_by_every_reader() {
    let good = sealed_raw(0, 1, 0, 0, r#"{"type":"reset"}"#);
    let extra = with_extra_key(1, 2, 1, 1, r#"{"type":"reset"}"#, "v", "2");
    let tape = joined(&[good.clone(), extra.clone()]);

    let err = parse_sealed(extra.as_bytes()).unwrap_err();
    assert!(matches!(err, Error::Deserialize(_)), "{err:?}");
    let corruption = scan(&tape).unwrap_err();
    assert_eq!(
        corruption.offset,
        good.len() + 1,
        "points at the foreign line"
    );
    assert!(
        corruption.reason.contains("crc-valid"),
        "{}",
        corruption.reason
    );
}

#[test]
fn extra_key_in_the_final_batch_is_fatal_not_residue() {
    // A torn write can never produce a valid CRC, so a CRC-valid foreign line
    // at the tail is not residue: it must stop the scan and the open.
    let good = sealed_raw(0, 1, 0, 0, r#"{"type":"reset"}"#);
    let extra = with_extra_key(1, 2, 1, 1, r#"{"type":"reset"}"#, "id", "\"op-7\"");
    let tape = joined(&[good, extra]);
    assert!(scan(&tape).is_err());
}

#[test]
fn open_refuses_extra_key_and_leaves_the_file_byte_identical() {
    let dir = tmp_dir("strict-open");
    let log = dir.join("tape.jsonl");
    let good = sealed_raw(0, 1, 0, 0, r#"{"type":"reset"}"#);
    let extra = with_extra_key(1, 2, 1, 1, r#"{"type":"reset"}"#, "v", "2");
    let tape = joined(&[good, extra]);
    std::fs::write(&log, &tape).unwrap();
    let err = match Runtime::<Ext>::open(dir.join("proj"), &log, schema(), None, reduce_ext) {
        Err(e) => e,
        Ok(_) => panic!("open must refuse a newer envelope"),
    };
    assert!(matches!(err, Error::Corruption(_)), "{err:?}");
    assert_eq!(
        read_bytes(&log),
        tape,
        "refusal must not truncate or rewrite"
    );
}

#[test]
fn record_decoder_refuses_unknown_keys() {
    let ok = r#"{"seq":0,"ts_ms":1,"from":0,"to":0,"event":{"type":"reset"},"crc":"00000000"}"#;
    assert!(serde_json::from_str::<Record<Int>>(ok).is_ok());
    let extra =
        r#"{"seq":0,"ts_ms":1,"from":0,"to":0,"event":{"type":"reset"},"crc":"00000000","v":2}"#;
    assert!(serde_json::from_str::<Record<Int>>(extra).is_err());
}

#[test]
fn render_and_parse_round_trip_with_the_strict_wire_format() {
    let line = render_sealed(3, 9, 2, 3, &Int::Reset).unwrap();
    let (env, event) = parse_sealed(line.as_bytes()).unwrap();
    assert_eq!((env.seq, env.from, env.to, env.ts_ms), (3, 2, 3, 9));
    assert_eq!(event, br#"{"type":"reset"}"#);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Any unknown envelope key on any line of a valid tape, with a valid CRC,
    /// makes the scan fail and leaves the bytes alone.
    #[test]
    fn any_extra_envelope_key_anywhere_is_refused(
        n in 1usize..12,
        at in any::<usize>(),
        key in "[a-z_]{1,8}",
    ) {
        prop_assume!(!matches!(key.as_str(), "seq" | "ts_ms" | "from" | "to" | "event" | "crc"));
        let mut lines = Vec::new();
        for seq in 0..n as u64 {
            lines.push(sealed_raw(seq, seq, seq, seq, r#"{"type":"reset"}"#));
        }
        let victim = at % n;
        let prefix = format!(
            "{{\"seq\":{victim},\"ts_ms\":{victim},\"from\":{victim},\"to\":{victim},\"{key}\":1,\"event\":{{\"type\":\"reset\"}}"
        );
        lines[victim] = format!("{prefix},\"crc\":\"{}\"}}", seal_crc(prefix.as_bytes()));
        let tape = joined(&lines);
        prop_assert!(scan(&tape).is_err());
        // Its own prefix is still fine: the damage is only the foreign key.
        prop_assert!(parse_sealed(lines[victim].as_bytes()).is_err());
    }

    /// The streaming scan gives the same answer whatever the reader's buffer
    /// size: lines split across refills are read whole.
    #[test]
    fn streaming_scan_is_buffer_size_independent(
        n in 1usize..16,
        cap in 1usize..40,
        garble in any::<Option<(usize, u8)>>(),
    ) {
        let mut tape = {
            let mut lines = Vec::new();
            let mut seq = 0u64;
            while (seq as usize) < n {
                let from = seq;
                let to = (seq + 1).min(n as u64 - 1);
                for s in from..=to {
                    lines.push(sealed_raw(s, s, from, to, r#"{"type":"note","text":"héllo\n"}"#));
                }
                seq = to + 1;
            }
            joined(&lines)
        };
        if let Some((at, byte)) = garble {
            let i = at % tape.len();
            tape[i] = byte;
        }
        let whole = scan(&tape).map_err(|c| (c.offset, c.reason));
        let streamed = scan_reader(BufReader::with_capacity(cap, &tape[..]))
            .map_err(|c| (c.offset, c.reason));
        prop_assert_eq!(whole, streamed);
    }
}
