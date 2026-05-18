pub mod calls;
pub mod schema;
pub mod slots;
pub mod sms;

use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::NetworkMode;
use crossbeam_channel::{Receiver, Sender};
use rusqlite::Connection;
use std::path::Path;
use std::thread;

pub enum StoreCommand {
    InsertCall(calls::CallRecord),
    InsertSms(sms::SmsRecord),
    UpdateSmsForwarding(sms::SmsForwardingUpdate),
    UpdateSmsForwardingByTime(sms::SmsForwardingByTimeUpdate),
    UpsertSlot { imei: String, usb_serial: String },
    SetModePref { slot: u32, mode: NetworkMode },
    Shutdown,
}

pub struct StoreHandle {
    tx: Sender<StoreCommand>,
    // Read-only connection for synchronous queries (called before async context is available).
    read_conn: Connection,
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

        let read_conn = Connection::open(&path)
            .map_err(|e| BridgeError::Store(format!("failed to open read connection: {e}")))?;

        let (tx, rx): (Sender<StoreCommand>, Receiver<StoreCommand>) =
            crossbeam_channel::unbounded();

        let thread = thread::Builder::new()
            .name("store-writer".into())
            .spawn(move || writer_loop(conn, rx))
            .map_err(|e| BridgeError::Store(format!("failed to spawn writer thread: {e}")))?;

        Ok(Self {
            tx,
            read_conn,
            thread: Some(thread),
        })
    }

    pub fn sender(&self) -> Sender<StoreCommand> {
        self.tx.clone()
    }

    pub fn lookup_slot(&self, imei: &str) -> BridgeResult<Option<u32>> {
        slots::lookup_slot(&self.read_conn, imei)
    }

    pub fn assign_slot_sync(&self, imei: &str, usb_serial: &str) -> BridgeResult<u32> {
        slots::assign_slot(&self.read_conn, imei, usb_serial)
    }

    pub fn get_mode_pref(&self, slot: u32) -> BridgeResult<Option<NetworkMode>> {
        slots::get_mode_pref(&self.read_conn, slot)
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
            StoreCommand::UpsertSlot { imei, usb_serial } => {
                if let Err(e) = slots::assign_slot(&conn, &imei, &usb_serial) {
                    tracing::error!(error = %e, imei = %imei, "failed to upsert card slot");
                }
            }
            StoreCommand::SetModePref { slot, mode } => {
                if let Err(e) = slots::set_mode_pref(&conn, slot, mode) {
                    tracing::error!(error = %e, slot = slot, "failed to set mode pref");
                }
            }
            StoreCommand::Shutdown => {
                tracing::info!("store writer shutting down");
                break;
            }
        }
    }
}
