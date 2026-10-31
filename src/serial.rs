use std::{
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub enum Command {
    Send(String),
}
pub enum Event {
    Bytes(Vec<u8>),
    Error(String),
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
pub fn connect(name: &str, baud: u32) -> Result<Connection, String> {
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
    let worker = thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let mut buffer = [0u8; 4096];
            while !worker_stop.load(Ordering::Relaxed) {
                while let Ok(Command::Send(s)) = rx.try_recv() {
                    port.write_all(format!("{}\r\n", s.trim()).as_bytes())
                        .map_err(|e| e.to_string())?;
                }
                match port.read(&mut buffer) {
                    Ok(0) => (),
                    Ok(n) => {
                        let bytes = buffer[..n].to_vec();
                        recording
                            .record(bytes.clone())
                            .map_err(|e| format!("写入串口日志失败：{e}"))?;
                        if !deliver(&tx, Event::Bytes(bytes), &worker_stop) {
                            break;
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
