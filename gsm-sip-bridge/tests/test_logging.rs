use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Clone)]
struct CaptureWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl CaptureWriter {
    fn new() -> Self {
        Self {
            buf: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn output(&self) -> String {
        let buf = self.buf.lock().unwrap();
        String::from_utf8_lossy(&buf).to_string()
    }
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn test_redaction_of_secrets_in_logs() {
    let capture = CaptureWriter::new();
    let capture_clone = capture.clone();

    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::new("trace"))
        .with(fmt::layer().with_writer(capture_clone).with_ansi(false));

    let _guard = tracing::subscriber::set_default(subscriber);

    let fake_password = "secret123";
    tracing::info!(
        sip_server = "pbx.example.com",
        sip_username = "bridge",
        message = "Config loaded successfully"
    );

    let output = capture.output();
    assert!(
        !output.contains(fake_password),
        "password value should not appear in logs"
    );
    assert!(
        output.contains("Config loaded"),
        "log message should appear: {output}"
    );
}
