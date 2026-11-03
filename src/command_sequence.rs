use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(2);
const COMMANDS: [&str; 5] = [
    "UNLOG",
    "RECTIMEA 0.5",
    "UNIHEADINGA 0.5",
    "BESTNAVA 0.2",
    "SAVECONFIG",
];

/// One outstanding command at a time. Receive framing is independent of the
/// navigation decoder: command replies need not be navigation records.
#[derive(Default)]
pub struct Sequence {
    commands: Vec<String>,
    manual: bool,
    index: usize,
    active: bool,
    sent: Option<Instant>,
    line: Vec<u8>,
    result: Option<bool>,
    command_result: Option<bool>,
}

impl Sequence {
    pub fn start(&mut self) {
        *self = Self {
            commands: COMMANDS.iter().map(|command| (*command).into()).collect(),
            active: true,
            ..Self::default()
        };
    }

    pub fn start_manual(&mut self, command: &str) {
        *self = Self {
            commands: vec![command.split_whitespace().collect::<Vec<_>>().join(" ")],
            manual: true,
            active: true,
            ..Self::default()
        };
    }

    pub fn is_manual(&self) -> bool {
        self.manual
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn next(&mut self, now: Instant) -> Option<&str> {
        if !self.active {
            return None;
        }
        if let Some(sent) = self.sent {
            if now.duration_since(sent) >= TIMEOUT {
                self.finish(false);
            }
            return None;
        }
        self.sent = Some(now);
        // Never join an old partial reply to a reply for the next command.
        self.line.clear();
        Some(&self.commands[self.index])
    }

    pub fn status(&self) -> String {
        if self.active {
            format!(
                "下发 {}/{}：{}，等待对应 OK",
                self.index + 1,
                self.commands.len(),
                self.commands[self.index]
            )
        } else if self.result == Some(false) {
            "一键下发失败".into()
        } else {
            "一键下发完成：全部指令已收到对应 OK".into()
        }
    }

    fn finish(&mut self, success: bool) {
        if !success {
            self.command_result = Some(false);
        }
        self.active = false;
        self.sent = None;
        self.line.clear();
        self.result = Some(success);
    }

    pub fn take_command_result(&mut self) -> Option<bool> {
        self.command_result.take()
    }

    pub fn take_result(&mut self) -> Option<bool> {
        self.result.take()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if !self.active || self.sent.is_none() {
            return;
        }
        for &byte in bytes {
            if byte == b'\n' {
                let line = String::from_utf8_lossy(&self.line);
                let reply = reply(&line, &self.commands[self.index]);
                self.line.clear();
                if let Some(ok) = reply {
                    self.command_result = Some(ok);
                    self.sent = None;
                    if ok {
                        self.index += 1;
                        if self.index == self.commands.len() {
                            self.finish(true);
                        }
                    } else {
                        self.finish(false);
                    }
                    // Remaining bytes predate the next send and cannot ack it.
                    return;
                }
            } else if self.line.len() < 4096 {
                self.line.push(byte);
            } else {
                // Discard oversized/noisy lines rather than growing without bound.
                self.line.clear();
            }
        }
    }
}

/// UM982 reply: $command,RECTIMEA 0.5,response: OK*xx\r\n.
/// A bare OK or another command's reply must never advance the sequence.
fn reply(line: &str, command: &str) -> Option<bool> {
    let line = line.trim();
    let body = line.split_once('*').map_or(line, |(body, _)| body);
    let mut fields = body.splitn(3, ',');
    if !fields.next()?.eq_ignore_ascii_case("$command") {
        return None;
    }
    let received = fields
        .next()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !received.eq_ignore_ascii_case(command) {
        return None;
    }
    let (label, result) = fields.next()?.split_once(':')?;
    if !label.trim().eq_ignore_ascii_case("response") {
        return None;
    }
    Some(result.trim().eq_ignore_ascii_case("OK"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_in_order_only_after_matching_ok() {
        let mut sequence = Sequence::default();
        sequence.start();
        let now = Instant::now();
        for command in COMMANDS {
            assert_eq!(sequence.next(now), Some(command));
            assert_eq!(sequence.next(now), None);
            sequence.feed(b"#BESTNAVA,OK\r\nOK\r\n$command,OTHER,response: OK*00\r\n");
            assert_eq!(sequence.next(now), None);
            let response = format!("$command,{},response: OK*00\r\n", command.to_lowercase());
            for byte in response.bytes() {
                sequence.feed(&[byte]);
            }
        }
        assert!(!sequence.active());
        assert_eq!(sequence.next(now), None);
        assert_eq!(sequence.take_result(), Some(true));
        assert_eq!(sequence.take_result(), None);
    }

    #[test]
    fn timeout_stops_sequence_and_reports_failure_once() {
        let mut sequence = Sequence::default();
        sequence.start();
        let now = Instant::now();
        assert_eq!(sequence.next(now), Some("UNLOG"));
        assert_eq!(
            sequence.next(now + TIMEOUT - Duration::from_millis(1)),
            None
        );
        assert!(sequence.active());
        assert_eq!(sequence.next(now + TIMEOUT), None);
        assert!(!sequence.active());
        assert_eq!(sequence.take_result(), Some(false));
        assert_eq!(sequence.take_result(), None);
        sequence.feed(b"$command,UNLOG,response: OK\r\n");
        assert_eq!(sequence.next(now + TIMEOUT), None);
    }

    #[test]
    fn negative_reply_stops_sequence_and_restart_clears_failure() {
        let mut sequence = Sequence::default();
        let now = Instant::now();
        for response in ["ERROR", "OKAY"] {
            sequence.start();
            assert_eq!(sequence.take_result(), None);
            assert_eq!(sequence.next(now), Some("UNLOG"));
            sequence.feed(format!("$command,UNLOG,response: {response}*00\r\n").as_bytes());
            assert!(!sequence.active());
            assert_eq!(sequence.next(now), None);
            assert_eq!(sequence.take_result(), Some(false));
        }
    }

    #[test]
    fn manual_command_waits_for_its_own_reply() {
        let mut sequence = Sequence::default();
        sequence.start_manual("  BESTNAVA   0.2  ");
        let now = Instant::now();
        assert!(sequence.is_manual());
        assert_eq!(sequence.next(now), Some("BESTNAVA 0.2"));
        sequence.feed(b"$command,BESTNAVA 0.5,response: OK\r\n");
        assert!(sequence.active());
        sequence.feed(b"$command,BESTNAVA 0.2,response: OK\r\n");
        assert_eq!(sequence.take_result(), Some(true));
        assert_eq!(sequence.next(now), None);
    }

    #[test]
    fn manual_timeout_and_failure_stop_without_retry() {
        let now = Instant::now();
        let mut sequence = Sequence::default();
        sequence.start_manual("UNLOG");
        sequence.next(now);
        sequence.next(now + TIMEOUT);
        assert_eq!(sequence.take_result(), Some(false));
        sequence.start_manual("SAVECONFIG");
        sequence.next(now);
        sequence.feed(b"$command,SAVECONFIG,response: ERROR\r\n");
        assert_eq!(sequence.take_result(), Some(false));
        assert_eq!(sequence.next(now), None);
    }

    #[test]
    fn stale_replies_in_same_read_cannot_ack_next_command() {
        let mut sequence = Sequence::default();
        sequence.start();
        let now = Instant::now();
        sequence.next(now);
        sequence.feed(b"$command,UNLOG,response: OK\r\n$command,RECTIMEA 0.5,response: OK\r\n");
        assert_eq!(sequence.next(now), Some("RECTIMEA 0.5"));
        assert_eq!(sequence.next(now), None);
    }
}
