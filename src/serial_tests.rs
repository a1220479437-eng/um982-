use super::*;

fn frame() -> Vec<u8> {
    let (records, _) = crate::protocol::decode_log(crate::test_date::LOG);
    format!("{}\r\n", records[0].raw).into_bytes()
}

#[test]
fn automatic_waits_for_complete_valid_frame_after_two_seconds() {
    let mut policy = ResetPolicy::new(true);
    let start = Instant::now();
    let frame = frame();
    assert!(policy.receive(&frame, start).is_none());
    let resumed = start + Duration::from_secs(2);
    assert!(policy.receive(b"noise\r\n", resumed).is_none());
    let mut corrupt = frame.clone();
    corrupt[1] ^= 1;
    assert!(policy.receive(&corrupt, resumed).is_none());
    let split = frame.len() / 2;
    assert!(policy.receive(&frame[..split], resumed).is_none());
    assert_eq!(
        policy.receive(&frame[split..], resumed),
        Some(frame.clone())
    );
    assert!(policy.receive(&frame, resumed).is_none());
}

#[test]
fn manual_and_short_gaps_never_reset() {
    let frame = frame();
    let start = Instant::now();
    let mut manual = ResetPolicy::new(false);
    assert!(manual.receive(&frame, start).is_none());
    assert!(
        manual
            .receive(&frame, start + Duration::from_secs(60))
            .is_none()
    );
    let mut automatic = ResetPolicy::new(true);
    assert!(automatic.receive(&frame, start).is_none());
    assert!(
        automatic
            .receive(&frame, start + Duration::from_millis(1999))
            .is_none()
    );
}

#[test]
fn stale_partial_frame_is_not_joined_across_silence() {
    let mut policy = ResetPolicy::new(true);
    let start = Instant::now();
    let frame = frame();
    let split = frame.len() / 2;
    assert!(policy.receive(&frame[..split], start).is_none());
    let resumed = start + Duration::from_secs(3);
    assert!(policy.receive(&frame[split..], resumed).is_none());
    assert_eq!(policy.receive(&frame, resumed), Some(frame));
}
