use crate::protocol::{Heading, Message, Position, Record};
use std::collections::VecDeque;

pub const HISTORY: usize = 10000;
#[derive(Clone, Copy, Debug)]
pub struct ReceiverTimeAnchor {
    gnss_seconds: f64,
    utc_seconds: f64,
    session_seconds: f64,
}
#[derive(Default)]
pub struct Model {
    pub position: Option<(Position, f64)>,
    pub heading: Option<(Heading, f64)>,
    pub time: Option<(Option<chrono::NaiveDateTime>, f64)>,
    pub smooth: Option<f64>,
    pub heights: VecDeque<[f64; 3]>,
    pub angles: VecDeque<[f64; 3]>,
    pub pitches: VecDeque<[f64; 3]>,
    pub speeds: VecDeque<[f64; 3]>,
    pub satellites: VecDeque<[f64; 3]>,
    pub logs: VecDeque<String>,
    pub count: usize,
    pub first_frame_at: Option<f64>,
    pub valid_after: [Option<f64>; 3],
    pub receiver_time_anchor: Option<ReceiverTimeAnchor>,
}
fn push<const N: usize>(q: &mut VecDeque<[f64; N]>, p: [f64; N]) {
    if q.len() >= HISTORY {
        q.pop_front();
    }
    q.push_back(p);
}
impl Model {
    fn gnss_seconds(record: &Record) -> Option<f64> {
        if record.week == 0 || record.tow_ms >= 604_800_000 {
            return None;
        }
        Some(f64::from(record.week) * 604_800.0 + f64::from(record.tow_ms) / 1000.0)
    }
    pub fn chart_time(&self, gnss_seconds: f64) -> Option<f64> {
        self.receiver_time_anchor
            .map(|anchor| anchor.utc_seconds + (gnss_seconds - anchor.gnss_seconds))
    }
    pub fn chart_now(&self, session_seconds: f64) -> Option<f64> {
        self.receiver_time_anchor
            .map(|anchor| anchor.utc_seconds + (session_seconds - anchor.session_seconds))
    }
    pub fn acquisition_delay(&self, index: usize) -> Option<f64> {
        Some(self.valid_after.get(index).copied().flatten()? - self.first_frame_at?)
    }
    /// Invalid measurements are still drawn, without substituting an old valid angle.
    pub fn compass_angle(&self) -> Option<f64> {
        self.heading.as_ref().map(|(h, _)| {
            if h.valid() {
                self.smooth.unwrap_or(h.angle)
            } else {
                h.angle
            }
        })
    }
    pub fn apply(&mut self, r: &Record, t: f64) {
        self.first_frame_at.get_or_insert(t);
        self.count += 1;
        if self.logs.len() >= 300 {
            self.logs.pop_front();
        }
        self.logs.push_back(r.raw.clone());
        match &r.message {
            Message::Position(p) => {
                let valid = p.valid();
                if valid {
                    self.valid_after[1].get_or_insert(t);
                }
                if let Some(gnss_time) = Self::gnss_seconds(r) {
                    let valid = if valid { 1.0 } else { 0.0 };
                    push(&mut self.heights, [gnss_time, p.height, valid]);
                    push(&mut self.speeds, [gnss_time, p.speed, valid]);
                    push(&mut self.satellites, [gnss_time, f64::from(p.used), valid]);
                }
                self.position = Some((p.clone(), t));
            }
            Message::Heading(h) => {
                let valid = h.valid();
                if valid {
                    self.valid_after[2].get_or_insert(t);
                    let prev = self.smooth.filter(|_| {
                        self.heading
                            .as_ref()
                            .is_some_and(|(_, last)| t - last <= 2.0)
                    });
                    self.smooth = Some(match prev {
                        Some(a) => (a + 0.3 * ((h.angle - a + 180.0).rem_euclid(360.0) - 180.0))
                            .rem_euclid(360.0),
                        None => h.angle,
                    });
                }
                if let Some(gnss_time) = Self::gnss_seconds(r) {
                    let valid = if valid { 1.0 } else { 0.0 };
                    push(&mut self.angles, [gnss_time, h.angle, valid]);
                    push(&mut self.pitches, [gnss_time, h.pitch, valid]);
                }
                self.heading = Some((h.clone(), t));
            }
            Message::Time(v) => {
                if let Some(utc) = v {
                    self.valid_after[0].get_or_insert(t);
                    if let Some(gnss_time) = Self::gnss_seconds(r) {
                        self.receiver_time_anchor = Some(ReceiverTimeAnchor {
                            gnss_seconds: gnss_time,
                            utc_seconds: utc.and_utc().timestamp_millis() as f64 / 1000.0,
                            session_seconds: t,
                        });
                    }
                }
                self.time = Some((*v, t));
            }
        }
    }
}

pub fn timeline(records: &[Record]) -> Vec<f64> {
    let mut t = 0.0;
    let mut last = None;
    records
        .iter()
        .map(|r| {
            let stamp = r.week as i64 * 604800000 + r.tow_ms as i64;
            if let Some(prev) = last {
                let delta = stamp - prev;
                // Receiver startup can jump from week 1 to the true week; don't wait decades.
                t += if (0..=10000).contains(&delta) {
                    delta as f64 / 1000.0
                } else {
                    0.2
                };
            }
            last = Some(stamp);
            t
        })
        .collect()
}

pub fn export(path: &std::path::Path, records: &[Record]) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(
        out,
        "elapsed_s,gps_week,gps_tow_ms,message,status,solution,latitude_deg,longitude_deg,height_m,satellites_used,satellites_tracked,speed_mps,heading_deg,heading_std_deg,dual_satellites,utc"
    )?;
    for (r, t) in records.iter().zip(timeline(records)) {
        let mut fields = vec![format!("{t:.3}"), r.week.to_string(), r.tow_ms.to_string()];
        let mut data = vec![String::new(); 13];
        match &r.message {
            Message::Position(p) => {
                data[0] = "BESTNAVA".into();
                data[1] = p.status.clone();
                data[2] = p.kind.clone();
                data[3] = format!("{:.11}", p.lat);
                data[4] = format!("{:.11}", p.lon);
                data[5] = p.height.to_string();
                data[6] = p.used.to_string();
                data[7] = p.tracked.to_string();
                data[8] = p.speed.to_string();
            }
            Message::Heading(h) => {
                data[0] = "UNIHEADINGA".into();
                data[1] = h.status.clone();
                data[2] = h.kind.clone();
                data[9] = h.angle.to_string();
                data[10] = h.std.to_string();
                data[11] = h.dual.to_string();
            }
            Message::Time(v) => {
                data[0] = "RECTIMEA".into();
                data[1] = if v.is_some() { "VALID" } else { "INVALID" }.into();
                data[12] = v
                    .map(|v| v.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
                    .unwrap_or_default();
            }
        }
        fields.extend(
            data.into_iter()
                .map(|s| format!("\"{}\"", s.replace('"', "\"\""))),
        );
        writeln!(out, "{}", fields.join(","))?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_heading_is_drawn_even_without_valid_history() {
        let mut model = Model::default();
        assert_eq!(model.compass_angle(), None);
        for (i, angle) in [123.0, 0.0, 278.0].into_iter().enumerate() {
            let h = Heading {
                status: "INSUFFICIENT_OBS".into(),
                kind: "NONE".into(),
                baseline: 0.0,
                angle,
                pitch: 0.0,
                std: 2.0,
                dual: 0,
            };
            if i == 1 {
                model.smooth = Some(90.0);
            } // Must not reuse the last valid EMA.
            model.apply(
                &Record {
                    week: 0,
                    tow_ms: 0,
                    raw: String::new(),
                    message: Message::Heading(h),
                },
                i as f64,
            );
            assert_eq!(model.compass_angle(), Some(angle));
        }
    }
    #[test]
    fn circular_ema() {
        let mut m = Model::default();
        for angle in [359.0, 1.0] {
            m.apply(
                &Record {
                    week: 0,
                    tow_ms: 0,
                    raw: String::new(),
                    message: Message::Heading(Heading {
                        status: "SOL_COMPUTED".into(),
                        kind: "NARROW_INT".into(),
                        baseline: 1.0,
                        angle,
                        pitch: 0.0,
                        std: 0.2,
                        dual: 20,
                    }),
                },
                0.0,
            );
        }
        assert!((m.smooth.unwrap() - 359.6).abs() < 1e-8);
    }

    #[test]
    fn pitch_history_tracks_valid_heading_measurements() {
        let mut model = Model::default();
        for (t, status, pitch) in [
            (1.0, "SOL_COMPUTED", -1.25),
            (2.0, "INSUFFICIENT_OBS", 8.0),
            (3.0, "SOL_COMPUTED", 2.5),
        ] {
            model.apply(
                &Record {
                    week: 2400,
                    tow_ms: (t * 1000.0) as u32,
                    raw: String::new(),
                    message: Message::Heading(Heading {
                        status: status.into(),
                        kind: "NARROW_INT".into(),
                        baseline: 1.0,
                        angle: 0.0,
                        pitch,
                        std: 0.2,
                        dual: 20,
                    }),
                },
                t,
            );
        }

        assert_eq!(
            model.pitches.iter().copied().collect::<Vec<_>>(),
            [
                [2400.0 * 604800.0 + 1.0, -1.25, 1.0],
                [2400.0 * 604800.0 + 2.0, 8.0, 0.0],
                [2400.0 * 604800.0 + 3.0, 2.5, 1.0]
            ]
        );
    }

    #[test]
    fn trend_histories_preserve_measurement_validity() {
        let mut model = Model::default();
        for (t, status, used, kind) in [
            (1.0, "SOL_COMPUTED", 12, "NARROW_INT"),
            (2.0, "INSUFFICIENT_OBS", 0, "NONE"),
        ] {
            model.apply(
                &Record {
                    week: 2400,
                    tow_ms: (t * 1000.0) as u32,
                    raw: String::new(),
                    message: Message::Position(Position {
                        status: status.into(),
                        kind: kind.into(),
                        lat: 1.0,
                        lon: 2.0,
                        height: 100.0 + t,
                        std: [0.1; 3],
                        tracked: 15,
                        used,
                        speed: 5.0 + t,
                        course: 0.0,
                        up: 0.0,
                    }),
                },
                t,
            );
            model.apply(
                &Record {
                    week: 2400,
                    tow_ms: (t * 1000.0) as u32,
                    raw: String::new(),
                    message: Message::Heading(Heading {
                        status: status.into(),
                        kind: kind.into(),
                        baseline: 1.0,
                        angle: 10.0 * t,
                        pitch: t,
                        std: 0.2,
                        dual: used,
                    }),
                },
                t,
            );
        }

        assert_eq!(
            model.heights.iter().copied().collect::<Vec<_>>(),
            [
                [2400.0 * 604800.0 + 1.0, 101.0, 1.0],
                [2400.0 * 604800.0 + 2.0, 102.0, 0.0]
            ]
        );
        assert_eq!(
            model.speeds.iter().copied().collect::<Vec<_>>(),
            [
                [2400.0 * 604800.0 + 1.0, 6.0, 1.0],
                [2400.0 * 604800.0 + 2.0, 7.0, 0.0]
            ]
        );
        assert_eq!(
            model.satellites.iter().copied().collect::<Vec<_>>(),
            [
                [2400.0 * 604800.0 + 1.0, 12.0, 1.0],
                [2400.0 * 604800.0 + 2.0, 0.0, 0.0]
            ]
        );
        assert_eq!(
            model.angles.iter().copied().collect::<Vec<_>>(),
            [
                [2400.0 * 604800.0 + 1.0, 10.0, 1.0],
                [2400.0 * 604800.0 + 2.0, 20.0, 0.0]
            ]
        );
    }

    #[test]
    fn receiver_time_aligns_charts_and_tracks_first_valid_delays() {
        let mut model = Model::default();
        let gnss_week = 2400;
        let gnss_tow = 1_000;
        model.apply(
            &Record {
                week: gnss_week,
                tow_ms: 0,
                raw: String::new(),
                message: Message::Position(Position {
                    status: "INSUFFICIENT_OBS".into(),
                    kind: "NONE".into(),
                    lat: 0.0,
                    lon: 0.0,
                    height: 0.0,
                    std: [1.0; 3],
                    tracked: 0,
                    used: 0,
                    speed: 0.0,
                    course: 0.0,
                    up: 0.0,
                }),
            },
            2.0,
        );
        model.apply(
            &Record {
                week: gnss_week,
                tow_ms: 500,
                raw: String::new(),
                message: Message::Position(Position {
                    status: "SOL_COMPUTED".into(),
                    kind: "NARROW_INT".into(),
                    lat: 0.0,
                    lon: 0.0,
                    height: 0.0,
                    std: [0.1; 3],
                    tracked: 20,
                    used: 12,
                    speed: 0.0,
                    course: 0.0,
                    up: 0.0,
                }),
            },
            3.0,
        );
        model.apply(
            &Record {
                week: gnss_week,
                tow_ms: 800,
                raw: String::new(),
                message: Message::Heading(Heading {
                    status: "SOL_COMPUTED".into(),
                    kind: "NARROW_INT".into(),
                    baseline: 1.0,
                    angle: 10.0,
                    pitch: 1.0,
                    std: 0.2,
                    dual: 20,
                }),
            },
            4.0,
        );
        let gnss_time = f64::from(gnss_week) * 604_800.0 + 1.0;
        assert_eq!(model.chart_time(gnss_time), None);

        let utc = chrono::NaiveDate::from_ymd_opt(2026, 10, 31)
            .unwrap()
            .and_hms_milli_opt(3, 0, 0, 0)
            .unwrap();
        model.apply(
            &Record {
                week: gnss_week,
                tow_ms: gnss_tow,
                raw: String::new(),
                message: Message::Time(Some(utc)),
            },
            5.0,
        );

        assert_eq!(model.acquisition_delay(0), Some(3.0));
        assert_eq!(model.acquisition_delay(1), Some(1.0));
        assert_eq!(model.acquisition_delay(2), Some(2.0));
        assert_eq!(
            model.chart_time(gnss_time),
            Some(utc.and_utc().timestamp_millis() as f64 / 1000.0)
        );
        assert_eq!(
            model.chart_now(7.0),
            model.chart_time(gnss_time).map(|t| t + 2.0)
        );
    }
}
