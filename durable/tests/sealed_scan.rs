//! The scan is the law: torn tails drop by rule, damage to acknowledged
//! bytes is fatal. Every trial here bleeds real sealed bytes.

use durable::runtime::{parse_sealed, render_sealed, scan};

fn seal(seq: u64, from: u64, to: u64, body: &str) -> String {
    let event: serde_json::Value = serde_json::from_str(body).unwrap();
    render_sealed(seq, 1000 + seq, from, to, &event).unwrap()
}

fn tape(lines: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

#[test]
fn sealed_lines_stay_greppable_and_jqable() {
    let line = seal(100, 100, 102, r#"{"kind":"x"}"#);
    assert!(line.starts_with(r#"{"seq":100,"#));
    assert!(line.ends_with('}'));
    // Suffix is always exactly ,"crc":"xxxxxxxx"} — slice, never re-render.
    let suffix = &line[line.len() - 18..];
    assert!(suffix.starts_with(r#","crc":""#));
    assert!(suffix.ends_with("\"}"));
    let hex = &suffix[8..16];
    assert_eq!(hex.len(), 8);
    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    let (env, _event) = parse_sealed(line.as_bytes()).unwrap();
    assert_eq!((env.seq, env.from, env.to), (100, 100, 102));
}

#[test]
fn clean_batches_commit_whole() {
    let bytes = tape(&[
        seal(0, 0, 0, r#"{"kind":"a"}"#),
        seal(1, 1, 2, r#"{"kind":"b"}"#),
        seal(2, 1, 2, r#"{"kind":"c"}"#),
    ]);
    let found = scan(&bytes).unwrap();
    assert_eq!(found.len, 3);
    assert_eq!(found.end, bytes.len());
}

#[test]
fn torn_tail_at_every_byte_drops_to_prefix() {
    let committed = vec![
        seal(0, 0, 0, r#"{"kind":"a"}"#),
        seal(1, 1, 1, r#"{"kind":"b"}"#),
    ];
    let base = tape(&committed);
    // In-flight batch of two; truncate at every byte of it.
    let inflight = format!("{}{}", seal(2, 2, 3, r#"{"kind":"c"}"#) + "\n", seal(3, 2, 3, r#"{"kind":"d"}"#) + "\n");
    let full = [base.clone(), inflight.into_bytes()].concat();
    // Cut 0 is the whole tape: the in-flight batch is complete, so all 4
    // commit by rule. Every cut past the final newline must drop to 2.
    assert_eq!(scan(&full).unwrap().len, 4);
    for k in 1..full.len() - base.len() {
        let cut = full.len() - k;
        let found = scan(&full[..cut]).unwrap();
        assert_eq!(found.len, 2, "cut {k} from end kept {}", found.len);
        assert_eq!(found.end, base.len());
    }
}

#[test]
fn closing_line_surviving_without_opener_still_drops() {
    // Batch [2..3]: opener destroyed, closer intact. The closer's from == 2
    // == committed len, so she reads as in-flight residue and drops.
    let committed = tape(&[seal(0, 0, 0, r#"{"kind":"a"}"#), seal(1, 1, 1, r#"{"kind":"b"}"#)]);
    let closer = seal(3, 2, 3, r#"{"kind":"d"}"#);
    let damaged = [committed.clone(), b"garbage line, not a candidate\n".to_vec(), format!("{closer}\n").into_bytes()].concat();
    let found = scan(&damaged).unwrap();
    assert_eq!(found.len, 2);
    assert_eq!(found.end, committed.len());
}

#[test]
fn valid_line_with_foreign_from_after_damage_is_fatal() {
    // Committed [0..1], damage, then a valid line from batch [7..7]:
    // a later batch was written, so the damage hit acknowledged bytes.
    let good = tape(&[seal(0, 0, 0, r#"{"kind":"a"}"#), seal(1, 1, 1, r#"{"kind":"b"}"#)]);
    let later = seal(7, 7, 7, r#"{"kind":"z"}"#);
    let damaged = [good, b"###BROKEN###\n".to_vec(), format!("{later}\n").into_bytes()].concat();
    let err = scan(&damaged).unwrap_err();
    assert!(err.reason.contains("not a candidate"), "{}", err.reason);
}

#[test]
fn crc_flip_is_not_a_candidate() {
    let mut line = seal(0, 0, 0, r#"{"kind":"a"}"#);
    let last_hex = line.pop().unwrap();
    line.pop();
    line.pop();
    line.push(if last_hex == '0' { '1' } else { '0' });
    line.push_str("\"}");
    assert!(parse_sealed(line.as_bytes()).is_err());
    // A flipped-CRC line mid-tape drops the tail by rule, not by judgment.
    let bytes = format!("{line}\n").into_bytes();
    let found = scan(&bytes).unwrap();
    assert_eq!(found.len, 0);
    assert_eq!(found.end, 0);
}

#[test]
fn bit_flip_in_committed_line_with_later_batch_is_fatal() {
    let l0 = seal(0, 0, 0, r#"{"kind":"a"}"#);
    let l1 = seal(1, 1, 1, r#"{"kind":"b"}"#);
    // Flip one body byte in l0 but keep her CRC valid is impossible without
    // the key; instead flip the CRC hex itself: line stops being a
    // candidate while a later valid batch survives -> corruption.
    let mut broken = l0.into_bytes();
    let pos = broken.len() - 10;
    broken[pos] = if broken[pos] == b'0' { b'1' } else { b'0' };
    let bytes = [broken, b"\n".to_vec(), format!("{l1}\n").into_bytes()].concat();
    assert!(scan(&bytes).is_err());
}

#[test]
fn inconsistent_to_inside_batch_never_commits() {
    // seq 1 claims to=[9] while her batch-mate claims to=[2]: incoherent.
    let bytes = tape(&[
        seal(0, 0, 0, r#"{"kind":"a"}"#),
        seal(1, 1, 9, r#"{"kind":"b"}"#),
        seal(2, 1, 2, r#"{"kind":"c"}"#),
    ]);
    let found = scan(&bytes).unwrap();
    assert_eq!(found.len, 1);
}
