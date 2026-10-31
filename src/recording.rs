use chrono::{Duration as ChronoDuration, Local};
use std::{
    fs::{self, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, RecvTimeoutError, SyncSender},
    thread::{self, JoinHandle},
    time::Duration,
};

/// A bounded queue keeps disk I/O off the serial/UI threads without unbounded memory use.
pub struct Recording {
    pub path: PathBuf,
    sender: Option<SyncSender<Vec<u8>>>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl Recording {
    pub fn start(directory: &Path, port: &str) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        // Serial devices may be paths (e.g. /dev/ttyUSB0 or \\.\COM13).
        let port = port
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("serial")
            .replace([':', '*', '?', '"', '<', '>', '|'], "_");
        let mut timestamp = Local::now();
        let (path, file) = loop {
            let path = directory.join(format!(
                "ReceivedTofile-{port}-{}.DAT",
                timestamp.format("%Y_%m_%d_%H-%M-%S")
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                // Preserve the naming format even for multiple reconnects in one second.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    timestamp += ChronoDuration::seconds(1);
                }
                Err(e) => return Err(e),
            }
        };
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(256);
        let worker = thread::Builder::new()
            .name("serial-recording".into())
            .spawn(move || {
                let mut writer = BufWriter::with_capacity(64 * 1024, file);
                loop {
                    match receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(bytes) => writer.write_all(&bytes)?,
                        Err(RecvTimeoutError::Timeout) => writer.flush()?,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                writer.flush()?;
                writer.get_ref().sync_all()
            })?;
        Ok(Self {
            path,
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    pub fn record(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.sender
            .as_ref()
            .unwrap()
            .send(bytes)
            .map_err(|_| io::Error::other("日志写入线程已停止"))
    }

    /// Close the queue, drain all queued blocks, sync and release the file.
    pub fn finish(&mut self) -> io::Result<()> {
        self.sender.take();
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| io::Error::other("日志写入线程异常退出"))?,
            None => Ok(()),
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_raw_bytes_and_creates_new_files_on_reconnect() {
        let directory = std::env::temp_dir().join(format!(
            "beidou-recording-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap()
        ));
        let mut first = Recording::start(&directory, "COM13").unwrap();
        let first_path = first.path.clone();
        for _ in 0..1000 {
            first.record(vec![0, 255, b'\r', b'\n']).unwrap();
        }
        first.finish().unwrap();
        assert_eq!(
            fs::read(&first_path).unwrap(),
            [0, 255, b'\r', b'\n'].repeat(1000)
        );
        let second = Recording::start(&directory, "COM13").unwrap();
        assert_ne!(first_path, second.path);
        let filename = second.path.file_name().unwrap().to_str().unwrap();
        let timestamp = filename
            .strip_prefix("ReceivedTofile-COM13-")
            .unwrap()
            .strip_suffix(".DAT")
            .unwrap();
        chrono::NaiveDateTime::parse_from_str(timestamp, "%Y_%m_%d_%H-%M-%S").unwrap();
        let second_path = second.path.clone();
        second.record(b"partial frame".to_vec()).unwrap();
        drop(second);
        assert_eq!(fs::read(second_path).unwrap(), b"partial frame");
        fs::remove_dir_all(directory).unwrap();
    }
}
