pub mod calls;
pub mod schema;
pub mod sms;

use crate::error::{BridgeError, BridgeResult};
use crossbeam_channel::{Receiver, Sender};
use rusqlite::Connection;
use std::path::Path;
use std::thread;

pub enum StoreCommand {
    InsertCall(calls::CallRecord),
    InsertSms(sms::SmsRecord),
    UpdateSmsForwarding(sms::SmsForwardingUpdate),
    UpdateSmsForwardingByTime(sms::SmsForwardingByTimeUpdate),
    Shutdown,
}

pub struct StoreHandle {
    tx: Sender<StoreCommand>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StoreHandle {
    pub fn open(db_path: &Path) -> BridgeResult<Self> {
        let path = db_path.to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BridgeError::Store(format!("failed to create store directory: {e}"))
            })?;
        }

        let conn = Connection::open(&path)
            .map_err(|e| BridgeError::Store(format!("failed to open store: {e}")))?;
        schema::init_schema(&conn)?;

        let (tx, rx): (Sender<StoreCommand>, Receiver<StoreCommand>) =
            crossbeam_channel::unbounded();

        let thread = thread::Builder::new()
            .name("store-writer".into())
            .spawn(move || writer_loop(conn, rx))
            .map_err(|e| BridgeError::Store(format!("failed to spawn writer thread: {e}")))?;

        Ok(Self {
            tx,
            thread: Some(thread),
        })
    }

    pub fn sender(&self) -> Sender<StoreCommand> {
        self.tx.clone()
    }

    pub fn shutdown(mut self) {
        let _ = self.tx.send(StoreCommand::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn writer_loop(conn: Connection, rx: Receiver<StoreCommand>) {
    for cmd in rx {
        match cmd {
            StoreCommand::InsertCall(record) => {
                if let Err(e) = calls::insert_call(&conn, &record) {
                    tracing::error!(error = %e, "failed to insert call record");
                }
            }
            StoreCommand::InsertSms(record) => {
                if let Err(e) = sms::insert_sms(&conn, &record) {
                    tracing::error!(error = %e, "failed to insert SMS record");
                }
            }
            StoreCommand::UpdateSmsForwarding(update) => {
                if let Err(e) = sms::update_sms_forwarding(&conn, &update) {
                    tracing::error!(error = %e, "failed to update SMS forwarding status");
                }
            }
            StoreCommand::UpdateSmsForwardingByTime(update) => {
                if let Err(e) = sms::update_sms_forwarding_by_time(&conn, &update) {
                    tracing::error!(error = %e, "failed to update SMS forwarding status");
                }
            }
            StoreCommand::Shutdown => {
                tracing::info!("store writer shutting down");
                break;
            }
        }
    }
}
