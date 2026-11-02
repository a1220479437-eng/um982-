use std::{
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub enum Command {
    Send(String),
    SetAutomatic(bool),
    Reset,
}
pub enum Event {
    Bytes(Vec<u8>),
    Error(String),
    Reset(std::path::PathBuf),
}
pub struct Connection {
    pub commands: mpsc::Sender<Command>,
    pub events: Receiver<Event>,
    pub log_path: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wait for all received bytes to be written and the file to be released.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn deliver(tx: &SyncSender<Event>, mut event: Event, stop: &AtomicBool) -> bool {
    while !stop.load(Ordering::Relaxed) {
        match tx.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(e)) => event = e,
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}
/// Detect silence using serial receive time, not the UI's polling cadence.
struct ResetPolicy {
    automatic: bool,
    last_bytes: Option<Instant>,
    pending: bool,
    decoder: crate::protocol::Decoder,
}
impl ResetPolicy {
    fn new(automatic: bool) -> Self {
        Self {
            automatic,
            last_bytes: None,
            pending: false,
            decoder: crate::protocol::Decoder::default(),
        }
    }

    fn receive(&mut self, bytes: &[u8], now: Instant) -> Option<Vec<u8>> {
        if self.automatic
            && self
                .last_bytes
                .is_some_and(|last| now.duration_since(last) >= Duration::from_secs(2))
        {
            self.pending = true;
            self.decoder = crate::protocol::Decoder::default();
        }
        self.last_bytes = Some(now);
        if self.pending
            && let Some(record) = self.decoder.feed(bytes).into_iter().next()
        {
            self.pending = false;
            // Include the entire triggering frame even when split across reads.
            return Some(format!("{}\r\n", record.raw).into_bytes());
        }
        None
    }
}

fn rotate(
    recording: &mut crate::recording::Recording,
    port: &str,
) -> Result<std::path::PathBuf, String> {
    let next = crate::recording::Recording::start(std::path::Path::new("logs"), port)
        .map_err(|e| format!("滚动日志失败：{e}"))?;
    recording
        .finish()
        .map_err(|e| format!("保存串口日志失败：{e}"))?;
    *recording = next;
    Ok(recording.path.clone())
}

#[cfg(test)]
#[path = "serial_tests.rs"]
mod tests;

pub fn connect(name: &str, baud: u32, automatic: bool) -> Result<Connection, String> {
    let mut port = serialport::new(name, baud)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(Duration::from_millis(50))
        .open()
        .map_err(|e| e.to_string())?;
    let mut recording = crate::recording::Recording::start(std::path::Path::new("logs"), name)
        .map_err(|e| format!("创建串口日志失败：{e}"))?;
    let log_path = recording.path.clone();
    let (tx, events) = mpsc::sync_channel(256);
    let (commands, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let name = name.to_owned();
    let worker = thread::spawn(move || {
        let mut policy = ResetPolicy::new(automatic);
        let result = (|| -> Result<(), String> {
            let mut buffer = [0u8; 4096];
            while !worker_stop.load(Ordering::Relaxed) {
                while let Ok(command) = rx.try_recv() {
                    match command {
                        Command::Send(s) => port
                            .write_all(format!("{}\r\n", s.trim()).as_bytes())
                            .map_err(|e| e.to_string())?,
                        Command::SetAutomatic(automatic) => policy = ResetPolicy::new(automatic),
                        Command::Reset => {
                            let path = rotate(&mut recording, &name)?;
                            policy = ResetPolicy::new(policy.automatic);
                            if !deliver(&tx, Event::Reset(path), &worker_stop) {
                                return Ok(());
                            }
                        }
                    }
                }
                match port.read(&mut buffer) {
                    Ok(0) => (),
                    Ok(n) => {
                        let now = Instant::now();
                        // A segment contains at most one complete frame, so rotation
                        // precedes that frame and all subsequent bytes in this read.
                        for segment in buffer[..n].split_inclusive(|b| *b == b'\n') {
                            let bytes = if let Some(frame) = policy.receive(segment, now) {
                                let path = rotate(&mut recording, &name)?;
                                if !deliver(&tx, Event::Reset(path), &worker_stop) {
                                    return Ok(());
                                }
                                frame
                            } else {
                                segment.to_vec()
                            };
                            recording
                                .record(bytes.clone())
                                .map_err(|e| format!("写入串口日志失败：{e}"))?;
                            if !deliver(&tx, Event::Bytes(bytes), &worker_stop) {
                                return Ok(());
                            }
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) => {}
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok(())
        })();
        drop(port);
        let finish = recording
            .finish()
            .map_err(|e| format!("保存串口日志失败：{e}"));
        if let Err(e) = finish.and(result) {
            let _ = deliver(&tx, Event::Error(e), &worker_stop);
        }
    });
    Ok(Connection {
        commands,
        events,
        log_path,
        stop,
        worker: Some(worker),
    })
}
