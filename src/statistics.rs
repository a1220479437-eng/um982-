//! Independent session statistics: replay seeks never mutate this accumulator.
use crate::protocol::{Heading, Message, Record};
use chrono::{Duration, NaiveDate, NaiveDateTime};
use std::collections::VecDeque;

pub const LIVE_DETAILS: usize = 2000;
const LIVE_CHAIN: usize = 32;

pub const NAMES: [&str; 3] = ["定位 BESTNAVA", "航向 UNIHEADINGA", "时间 RECTIMEA"];

pub struct Interval {
    pub channel: usize,
    pub start: f64,
    pub end: f64,
    pub reason: String,
    pub start_time: Option<NaiveDateTime>,
    pub end_time: Option<NaiveDateTime>,
}

/// Consecutive invalid heading measurements, independent of reason changes.
/// End is the last invalid frame, not the first recovered frame.
pub struct HeadingSegment {
    pub start: f64,
    pub end: f64,
    pub start_time: Option<NaiveDateTime>,
    pub end_time: Option<NaiveDateTime>,
    pub frames: usize,
    pub first: Heading,
    pub last: Heading,
    pub before: Option<Heading>,
    pub after: Option<Heading>,
    pub kinds: Vec<String>,
    pub reasons: std::collections::BTreeSet<String>,
    pub std_range: [f64; 2],
    pub pitch_range: [f64; 2],
    pub satellite_range: [u32; 2],
}

impl HeadingSegment {
    pub fn solution_chain(&self) -> String {
        let describe = |h: &Heading| format!("{} (dev={:.3}°)", h.kind, h.std);
        format!(
            "{} → {} (dev={:.3}–{:.3}°) → {}",
            self.before
                .as_ref()
                .map(describe)
                .unwrap_or_else(|| "无前序有效帧".into()),
            self.kinds.join(" → "),
            self.std_range[0],
            self.std_range[1],
            self.after
                .as_ref()
                .map(describe)
                .unwrap_or_else(|| "尚无恢复帧".into())
        )
    }
}

#[derive(Default)]
struct Channel {
    last: Option<f64>,
    active: Option<usize>,
    reason: Option<String>,
    total: usize,
    invalid: usize,
    archived_intervals: usize,
    archived_duration: f64,
}

#[derive(Default)]
pub struct Statistics {
    channels: [Channel; 3],
    pub intervals: VecDeque<Interval>,
    pub end: f64,
    clock_anchor: Option<(f64, NaiveDateTime)>,
    gps_utc_offset: Option<Duration>,
    pub heading_segments: VecDeque<HeadingSegment>,
    detail_limit: Option<usize>,
    pub discarded_segments: usize,
    active_heading: Option<usize>,
    previous_heading: Option<Heading>,
    // [valid, invalid] measurements; independent of the bounded record archive.
    baseline_sums: [f64; 2],
    baseline_counts: [usize; 2],
}

fn gps_time(record: &Record) -> Option<NaiveDateTime> {
    // Week zero is also used before the receiver has acquired time.
    if record.week == 0 || record.tow_ms >= 604_800_000 {
        return None;
    }
    NaiveDate::from_ymd_opt(1980, 1, 6)?
        .and_hms_opt(0, 0, 0)?
        .checked_add_signed(Duration::milliseconds(
            i64::from(record.week) * 604_800_000 + i64::from(record.tow_ms),
        ))
}

impl Statistics {
    pub fn live() -> Self {
        Self {
            detail_limit: Some(LIVE_DETAILS),
            ..Self::default()
        }
    }

    pub fn interval_summary(&self, channel: usize) -> (usize, f64) {
        let state = &self.channels[channel];
        self.intervals.iter().filter(|i| i.channel == channel).fold(
            (state.archived_intervals, state.archived_duration),
            |(count, duration), i| (count + 1, duration + i.end - i.start),
        )
    }

    fn reserve_interval(&mut self) {
        if !self
            .detail_limit
            .is_some_and(|limit| self.intervals.len() >= limit)
        {
            return;
        }
        // Never evict an ongoing interval; at most three can be active.
        let index = (0..self.intervals.len())
            .find(|&i| !self.ongoing(i))
            .unwrap();
        let old = self.intervals.remove(index).unwrap();
        let state = &mut self.channels[old.channel];
        state.archived_intervals += 1;
        state.archived_duration += old.end - old.start;
        for state in &mut self.channels {
            if let Some(i) = &mut state.active
                && *i > index
            {
                *i -= 1;
            }
        }
    }

    pub fn from_records(records: &[Record], times: &[f64]) -> Self {
        let mut stats = Self::default();
        for (record, &t) in records.iter().zip(times) {
            stats.apply(record, t);
        }
        stats
    }

    fn transition(&mut self, channel: usize, t: f64, reason: Option<String>) {
        let time = self.time_at(t);
        if self.channels[channel].reason == reason {
            return;
        }
        if let Some(i) = self.channels[channel].active.take() {
            self.intervals[i].end = t;
            self.intervals[i].end_time = time;
        }
        self.channels[channel].reason = reason.clone();
        if let Some(reason) = reason {
            self.reserve_interval();
            self.channels[channel].active = Some(self.intervals.len());
            self.intervals.push_back(Interval {
                channel,
                start: t,
                end: t,
                reason,
                start_time: time,
                end_time: time,
            });
        }
    }

    /// Missing streams are not inferred invalid before their first message.
    pub fn advance(&mut self, t: f64) {
        self.end = self.end.max(t);
        let time = self.time_at(self.end);
        for channel in 0..3 {
            if let Some(last) = self.channels[channel].last
                && t - last > 2.0
            {
                self.transition(channel, last + 2.0, Some("超过 2 秒未更新".into()));
            }
            if let Some(i) = self.channels[channel].active {
                self.intervals[i].end = self.end;
                self.intervals[i].end_time = time;
            }
        }
    }

    pub fn apply(&mut self, record: &Record, t: f64) {
        // First close any timeout using the preceding record's clock. Then use
        // this record's actual GPS stamp, not the compressed replay timeline.
        self.advance(t);
        let gps = gps_time(record);
        self.clock_anchor = gps.map(|time| (t, time));
        if let (Some(gps), Message::Time(Some(utc))) = (gps, &record.message) {
            self.gps_utc_offset = Some(gps - *utc);
        }
        let time = self.time_at(t);
        for state in &self.channels {
            if let Some(i) = state.active {
                self.intervals[i].end_time = time;
            }
        }
        let (channel, reason) = invalid_reason(&record.message);
        self.channels[channel].total += 1;
        self.channels[channel].invalid += usize::from(reason.is_some());
        self.transition(channel, t, reason);
        self.channels[channel].last = Some(t);
        if let Message::Heading(h) = &record.message {
            self.apply_heading(h, t, time);
        }
    }

    fn apply_heading(&mut self, h: &Heading, t: f64, time: Option<NaiveDateTime>) {
        let group = usize::from(!h.valid());
        self.baseline_sums[group] += h.baseline;
        self.baseline_counts[group] += 1;
        if h.valid() {
            if let Some(i) = self.active_heading.take() {
                self.heading_segments[i].after = Some(h.clone());
            }
        } else {
            let reason = invalid_reason(&Message::Heading(h.clone()))
                .1
                .unwrap_or_default();
            if let Some(i) = self.active_heading {
                let segment = &mut self.heading_segments[i];
                segment.end = t;
                segment.end_time = time;
                segment.frames += 1;
                segment.last = h.clone();
                if segment.kinds.last() != Some(&h.kind)
                    && (self.detail_limit.is_none() || segment.kinds.len() < LIVE_CHAIN)
                {
                    segment.kinds.push(h.kind.clone());
                }
                for reason in reason.split('；') {
                    if self.detail_limit.is_none() || segment.reasons.len() < LIVE_CHAIN {
                        segment.reasons.insert(reason.to_owned());
                    }
                }
                segment.std_range[0] = segment.std_range[0].min(h.std);
                segment.std_range[1] = segment.std_range[1].max(h.std);
                segment.pitch_range[0] = segment.pitch_range[0].min(h.pitch);
                segment.pitch_range[1] = segment.pitch_range[1].max(h.pitch);
                segment.satellite_range[0] = segment.satellite_range[0].min(h.dual);
                segment.satellite_range[1] = segment.satellite_range[1].max(h.dual);
            } else {
                if self
                    .detail_limit
                    .is_some_and(|limit| self.heading_segments.len() >= limit)
                {
                    self.heading_segments.pop_front();
                    self.discarded_segments += 1;
                }
                self.active_heading = Some(self.heading_segments.len());
                self.heading_segments.push_back(HeadingSegment {
                    start: t,
                    end: t,
                    start_time: time,
                    end_time: time,
                    frames: 1,
                    first: h.clone(),
                    last: h.clone(),
                    before: self.previous_heading.clone(),
                    after: None,
                    kinds: vec![h.kind.clone()],
                    reasons: reason.split('；').map(str::to_owned).collect(),
                    std_range: [h.std; 2],
                    pitch_range: [h.pitch; 2],
                    satellite_range: [h.dual; 2],
                });
            }
        }
        self.previous_heading = Some(h.clone());
    }

    pub fn baseline_means(&self) -> [Option<f64>; 2] {
        std::array::from_fn(|i| {
            (self.baseline_counts[i] > 0)
                .then(|| self.baseline_sums[i] / self.baseline_counts[i] as f64)
        })
    }

    fn time_at(&self, t: f64) -> Option<NaiveDateTime> {
        let (anchor, time) = self.clock_anchor?;
        time.checked_add_signed(Duration::milliseconds(
            ((t - anchor) * 1000.0).round() as i64
        ))
    }

    pub fn time_basis(&self) -> &'static str {
        if self.gps_utc_offset.is_some() {
            "北京时间（UTC+8）"
        } else {
            "GPS 时间（非 UTC）"
        }
    }

    pub fn format_time(&self, time: Option<NaiveDateTime>) -> String {
        time.and_then(|time| match self.gps_utc_offset {
            Some(offset) => time
                .checked_sub_signed(offset)?
                .checked_add_signed(Duration::hours(8)),
            None => Some(time),
        })
        .map(|time| time.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
        .unwrap_or_else(|| "时间不可用".into())
    }

    pub fn counts(&self, channel: usize) -> (usize, usize) {
        let c = &self.channels[channel];
        (c.total, c.invalid)
    }

    pub fn ongoing(&self, index: usize) -> bool {
        self.channels.iter().any(|c| c.active == Some(index))
    }
}

fn invalid_reason(message: &Message) -> (usize, Option<String>) {
    let mut reasons = Vec::new();
    let channel = match message {
        Message::Position(p) => {
            if p.status != "SOL_COMPUTED" {
                reasons.push(format!("解算状态 {}", p.status));
            }
            if p.kind == "NONE" {
                reasons.push("无定位解（NONE）".into());
            }
            if p.used == 0 {
                reasons.push("使用卫星数为 0".into());
            }
            0
        }
        Message::Heading(h) => {
            if h.status != "SOL_COMPUTED" {
                reasons.push(format!("解算状态 {}", h.status));
            }
            if !matches!(
                h.kind.as_str(),
                "NARROW_INT" | "L1_INT" | "NARROW_FLOAT" | "L1_FLOAT"
            ) {
                reasons.push(format!("不支持的航向解类型 {}", h.kind));
            }
            if h.std >= 1.0 {
                reasons.push("航向标准差 ≥ 1°".into());
            }
            if h.dual == 0 {
                reasons.push("双频卫星数为 0".into());
            }
            1
        }
        Message::Time(t) => {
            if t.is_none() {
                reasons.push("接收机时间标记无效".into());
            }
            2
        }
    };
    (channel, (!reasons.is_empty()).then(|| reasons.join("；")))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn time(valid: bool) -> Record {
        Record {
            week: 0,
            tow_ms: 0,
            raw: String::new(),
            message: Message::Time(valid.then(|| {
                chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
            })),
        }
    }
    #[test]
    fn live_details_are_bounded_without_losing_totals_or_active_intervals() {
        let mut live = Statistics::live();
        let mut full = Statistics::default();
        // Keep another channel active throughout eviction to check index remapping.
        let heading = Record {
            week: 0,
            tow_ms: 0,
            raw: String::new(),
            message: Message::Heading(Heading {
                status: "SOL_COMPUTED".into(),
                kind: "NONE".into(),
                baseline: 1.0,
                angle: 0.0,
                pitch: 0.0,
                std: 2.0,
                dual: 0,
            }),
        };
        live.apply(&heading, 0.0);
        full.apply(&heading, 0.0);
        for i in 0..LIVE_DETAILS * 6 {
            let t = i as f64 * 0.1;
            let record = time(i % 2 == 0);
            live.apply(&record, t);
            full.apply(&record, t);
            assert!(live.intervals.len() <= LIVE_DETAILS);
        }
        for channel in 0..3 {
            assert_eq!(live.counts(channel), full.counts(channel));
            let (count, duration) = live.interval_summary(channel);
            let (expected_count, expected_duration) = full.interval_summary(channel);
            assert_eq!(count, expected_count);
            assert!((duration - expected_duration).abs() < 1e-7);
        }
        for state in &live.channels {
            if let Some(i) = state.active {
                assert_eq!(live.intervals[i].end, live.end);
            }
        }
    }

    #[test]
    fn live_heading_segments_and_continuous_reason_changes_are_bounded() {
        let mut stats = Statistics::live();
        let mut h = Heading {
            status: "SOL_COMPUTED".into(),
            kind: "NARROW_INT".into(),
            baseline: 1.0,
            angle: 0.0,
            pitch: 0.0,
            std: 2.0,
            dual: 10,
        };
        for i in 0..LIVE_DETAILS * 4 {
            h.std = if i % 2 == 0 { 2.0 } else { 0.2 };
            stats.apply_heading(&h, i as f64, None);
            assert!(stats.heading_segments.len() <= LIVE_DETAILS);
        }
        assert!(stats.discarded_segments > 0);
        h.std = 2.0;
        for i in 0..LIVE_CHAIN * 10 {
            h.kind = format!("UNKNOWN_{i}");
            stats.apply_heading(&h, i as f64, None);
        }
        let segment = stats.heading_segments.back().unwrap();
        assert_eq!(segment.frames, LIVE_CHAIN * 10);
        assert_eq!(segment.kinds.len(), LIVE_CHAIN);
        assert_eq!(segment.reasons.len(), LIVE_CHAIN);
        assert_eq!(segment.last.kind, h.kind);
        h.kind = "NARROW_INT".into();
        h.std = 0.2;
        stats.apply_heading(&h, 1000.0, None);
        assert!(stats.heading_segments.back().unwrap().after.is_some());
    }

    #[test]
    fn heading_segments_merge_reason_changes_and_keep_recovery_context() {
        let heading = Heading {
            status: "SOL_COMPUTED".into(),
            kind: "NARROW_INT".into(),
            baseline: 1.678,
            angle: 0.0,
            pitch: 2.28,
            std: 0.19,
            dual: 34,
        };
        let mut s = Statistics::default();
        let mut apply = |h: Heading, t| {
            s.apply(
                &Record {
                    week: 2400,
                    tow_ms: (t * 1000.0) as u32,
                    raw: String::new(),
                    message: Message::Heading(h),
                },
                t,
            )
        };
        apply(heading.clone(), 0.0);
        let mut invalid = heading.clone();
        invalid.kind = "NARROW_FLOAT".into();
        invalid.std = 17.8;
        invalid.baseline = 1.674;
        apply(invalid.clone(), 0.2);
        invalid.std = 20.3;
        invalid.dual = 0; // Changes the reason, but not the measurement segment.
        invalid.pitch = -2.78;
        apply(invalid, 0.4);
        apply(heading.clone(), 0.6);
        let segment = &s.heading_segments[0];
        assert_eq!(s.heading_segments.len(), 1);
        assert_eq!(segment.frames, 2);
        assert_eq!((segment.start, segment.end), (0.2, 0.4));
        assert_eq!(segment.std_range, [17.8, 20.3]);
        assert_eq!(segment.satellite_range, [0, 34]);
        assert_eq!(segment.before.as_ref().unwrap().std, 0.19);
        assert!(segment.after.as_ref().unwrap().valid());
        assert_eq!(segment.reasons.len(), 2);
        assert!(segment.solution_chain().contains("NARROW_FLOAT"));
        let means = s.baseline_means();
        assert!((means[0].unwrap() - 1.678).abs() < 1e-9);
        assert!((means[1].unwrap() - 1.674).abs() < 1e-9);
        let mut invalid = heading;
        invalid.std = 2.0;
        s.apply(
            &Record {
                week: 2400,
                tow_ms: 800,
                raw: String::new(),
                message: Message::Heading(invalid),
            },
            0.8,
        );
        assert_eq!(s.heading_segments.len(), 2);
        assert!(s.heading_segments[1].after.is_none());
    }

    #[test]
    fn interval_dates_use_record_timestamps_not_compressed_replay_seconds() {
        let mut first = time(false);
        first.week = 2400;
        first.tow_ms = 1000;
        let mut recovered = time(true);
        recovered.week = 2400;
        recovered.tow_ms = 61_000;
        // A 60-second gap is compressed to 0.2 seconds by playback.
        let mut s = Statistics::default();
        s.apply(&first, 0.0);
        // No UTC anchor yet: report GPS rather than silently assuming leap seconds.
        assert_eq!(s.time_basis(), "GPS 时间（非 UTC）");
        let gps = gps_time(&recovered).unwrap();
        recovered.message = Message::Time(Some(gps - Duration::seconds(18)));
        s.apply(&recovered, 0.2);
        let interval = &s.intervals[0];
        assert_eq!(interval.start_time, gps_time(&first));
        assert_eq!(interval.end_time, Some(gps));
        assert_eq!(s.time_basis(), "北京时间（UTC+8）");
        assert_eq!(
            s.format_time(interval.start_time),
            (gps_time(&first).unwrap() - Duration::seconds(18) + Duration::hours(8))
                .format("%Y-%m-%d %H:%M:%S%.3f")
                .to_string()
        );
    }

    #[test]
    fn timeout_dates_are_extrapolated_from_last_record() {
        let mut record = time(false);
        record.week = 2400;
        record.tow_ms = 604_799_000;
        let mut s = Statistics::default();
        s.apply(&record, 0.0);
        s.advance(4.0);
        assert_eq!(
            s.intervals[1].start_time,
            gps_time(&record)
                .unwrap()
                .checked_add_signed(Duration::seconds(2))
        );
        assert_eq!(
            s.intervals[1].end_time,
            gps_time(&record)
                .unwrap()
                .checked_add_signed(Duration::seconds(4))
        );
        assert_eq!(s.format_time(None), "时间不可用");
    }

    #[test]
    fn merges_invalid_and_closes_at_recovery() {
        let mut s = Statistics::default();
        s.apply(&time(false), 0.0);
        s.apply(&time(false), 1.0);
        s.apply(&time(true), 2.0);
        assert_eq!(s.intervals.len(), 1);
        assert_eq!((s.intervals[0].start, s.intervals[0].end), (0.0, 2.0));
        assert!(!s.ongoing(0));
        assert_eq!(s.counts(2), (3, 2));
    }
    #[test]
    fn timeout_splits_reason_and_recovers() {
        let mut s = Statistics::default();
        s.apply(&time(false), 0.0);
        s.advance(2.0);
        assert_eq!(s.intervals.len(), 1);
        s.advance(3.0);
        s.advance(4.0);
        s.apply(&time(true), 5.0);
        assert_eq!(s.intervals.len(), 2);
        assert_eq!(s.intervals[0].end, 2.0);
        assert_eq!((s.intervals[1].start, s.intervals[1].end), (2.0, 5.0));
        assert_eq!(s.counts(0), (0, 0));
    }
    #[test]
    fn fixture_statistics_match_measurement_validity() {
        let (records, _) = crate::protocol::decode_log(crate::test_date::LOG);
        let times = crate::model::timeline(&records);
        let s = Statistics::from_records(&records, &times);
        let mut expected = [(0, 0); 3];
        for record in &records {
            let (channel, valid) = match &record.message {
                Message::Position(p) => (0, p.valid()),
                Message::Heading(h) => (1, h.valid()),
                Message::Time(t) => (2, t.is_some()),
            };
            expected[channel].0 += 1;
            expected[channel].1 += usize::from(!valid);
        }
        for (channel, counts) in expected.into_iter().enumerate() {
            assert_eq!(s.counts(channel), counts);
        }
        assert!(
            s.intervals
                .iter()
                .all(|i| i.end >= i.start && i.end <= s.end)
        );
    }

    #[test]
    fn unseen_channels_and_empty_files_have_no_failures() {
        let mut s = Statistics::from_records(&[], &[]);
        s.advance(10.0);
        assert!(s.intervals.is_empty());
    }
}
