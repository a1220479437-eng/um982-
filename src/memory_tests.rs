use super::*;

#[test]
fn live_archive_stops_at_byte_budget_and_retains_existing_prefix() {
    let (records, _) = protocol::decode_log(include_bytes!("../tests/fixtures/um982.dat"));
    let mut record = records[0].clone();
    record.raw = "x".repeat(4096);
    let mut archive = Vec::new();
    let mut bytes = 0;
    while archive_live(&mut archive, &mut bytes, record.clone()) {}
    assert!(bytes <= LIVE_ARCHIVE_BYTES);
    assert!(LIVE_ARCHIVE_BYTES - bytes < record.retained_bytes());
    let len = archive.len();
    let capacity = archive.capacity();
    for _ in 0..10_000 {
        assert!(!archive_live(&mut archive, &mut bytes, record.clone()));
    }
    assert_eq!(archive.len(), len);
    assert_eq!(archive.capacity(), capacity);
    assert_eq!(archive[0].raw, record.raw);
}

#[test]
fn display_history_remains_bounded_during_long_streams() {
    let (records, _) = protocol::decode_log(include_bytes!("../tests/fixtures/um982.dat"));
    let mut model = Model::default();
    for i in 0..50_000 {
        for record in &records {
            model.apply(record, i as f64);
        }
    }
    assert!(model.logs.len() <= 300);
    assert!(model.heights.len() <= model::HISTORY);
    assert!(model.angles.len() <= model::HISTORY);
    assert!(model.speeds.len() <= model::HISTORY);
    assert!(model.satellites.len() <= model::HISTORY);
}

#[test]
fn charts_start_at_first_sample_then_scroll_over_sixty_seconds() {
    let initial = [[150.0, 1.0, 1.0], [200.0, 2.0, 1.0]];
    assert_eq!(chart_window_range(200.0, &initial), (150.0, 200.0));

    let later = [
        [100.0, 0.0, 1.0],
        [150.0, 1.0, 1.0],
        [200.0, 2.0, 1.0],
        [230.0, 3.0, 1.0],
    ];
    assert_eq!(chart_window_range(230.0, &later), (200.0, 230.0));
}
