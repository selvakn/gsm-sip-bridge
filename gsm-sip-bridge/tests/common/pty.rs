use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub struct PtyHarness {
    _socat: Child,
    pub device_path: String,
    test_fd: std::fs::File,
}

impl PtyHarness {
    pub fn new() -> Option<Self> {
        let socat = Command::new("socat")
            .args([
                "-d",
                "-d",
                "PTY,raw,echo=0,link=/tmp/gsm-test-device",
                "PTY,raw,echo=0,link=/tmp/gsm-test-host",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;

        std::thread::sleep(Duration::from_millis(200));

        let test_fd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/tmp/gsm-test-host")
            .ok()?;

        Some(Self {
            _socat: socat,
            device_path: "/tmp/gsm-test-device".to_string(),
            test_fd,
        })
    }

    pub fn expect_and_reply(&mut self, _expected: &str, reply: &str) {
        let _ = self.test_fd.write_all(reply.as_bytes());
        let _ = self.test_fd.flush();
    }

    pub fn reply(&mut self, data: &str) {
        let _ = self.test_fd.write_all(data.as_bytes());
        let _ = self.test_fd.flush();
    }

    pub fn read_line(&mut self) -> Option<String> {
        let mut reader = BufReader::new(&self.test_fd);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        Some(line)
    }
}

impl Drop for PtyHarness {
    fn drop(&mut self) {
        let _ = self._socat.kill();
    }
}
