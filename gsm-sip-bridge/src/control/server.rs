use crate::control::protocol::{read_cmd, write_resp, ControlCmd, ControlResp};
use std::io::BufReader;
use std::path::Path;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

pub type CmdSender = mpsc::Sender<(ControlCmd, oneshot::Sender<ControlResp>)>;

pub async fn start_control_server(
    socket_path: &str,
    cmd_tx: CmdSender,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let socket_path = socket_path.to_string();

    // Remove stale socket file
    let _ = std::fs::remove_file(&socket_path);

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, path = %socket_path, "failed to bind control socket");
            return tokio::spawn(async {});
        }
    };

    tracing::info!(path = %socket_path, "control socket listening");

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let cmd_tx = cmd_tx.clone();
                            tokio::spawn(async move {
                                handle_connection(stream, cmd_tx).await;
                            });
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "control socket accept error");
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("control server shutting down");
                        break;
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&socket_path);
    })
}

async fn handle_connection(stream: tokio::net::UnixStream, cmd_tx: CmdSender) {
    let std_stream = match stream.into_std() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to convert Unix stream");
            return;
        }
    };

    let read_stream = match std_stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to clone Unix stream");
            return;
        }
    };

    let mut reader = BufReader::new(read_stream);
    let mut writer = std_stream;

    let cmd = match read_cmd(&mut reader) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read control command");
            let _ = write_resp(&mut writer, &ControlResp::err(e));
            return;
        }
    };

    let (resp_tx, resp_rx) = oneshot::channel();
    if cmd_tx.send((cmd, resp_tx)).await.is_err() {
        tracing::warn!("control command channel closed");
        let _ = write_resp(&mut writer, &ControlResp::err("daemon shutting down"));
        return;
    }

    match resp_rx.await {
        Ok(resp) => {
            let _ = write_resp(&mut writer, &resp);
        }
        Err(_) => {
            let _ = write_resp(&mut writer, &ControlResp::err("no response from daemon"));
        }
    }
}

pub fn socket_path_exists(path: &str) -> bool {
    Path::new(path).exists()
}
