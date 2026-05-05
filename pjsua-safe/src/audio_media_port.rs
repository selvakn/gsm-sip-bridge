use crate::call::SlotId;
use crate::endpoint::Endpoint;
use crate::error::PjsipError;

pub trait AudioMediaPort: Send {
    fn read_frame(&mut self, buf: &mut [i16]);
    fn write_frame(&mut self, buf: &[i16]);
}

pub struct MediaPortHandle {
    slot_id: SlotId,
}

impl MediaPortHandle {
    pub fn register_to_conf_bridge(
        _endpoint: &Endpoint,
        _port: Box<dyn AudioMediaPort>,
    ) -> Result<Self, PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            // SAFETY: We allocate the pjmedia_port on the heap with the custom callbacks.
            // The port lifetime is tied to this MediaPortHandle.
            // Full implementation would create a pjmedia_port with get_frame/put_frame callbacks
            // that delegate to the AudioMediaPort trait methods.
            return Err(PjsipError::MediaPort(
                "full pjmedia_port registration requires runtime PJSIP".into(),
            ));
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            tracing::debug!("media port registered to conference bridge (stub mode)");
            Ok(Self { slot_id: 1 })
        }
    }

    pub fn slot_id(&self) -> SlotId {
        self.slot_id
    }

    pub fn connect_to(&self, _dest_slot: SlotId) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            unsafe // SAFETY: PJSIP initialized; slot IDs valid conference bridge ports for connect
            {
                let status = pjsua_sys::pjsua_conf_connect(self.slot_id, _dest_slot);
                if status != crate::error::PJ_SUCCESS {
                    return Err(PjsipError::MediaPort(format!(
                        "pjsua_conf_connect returned {status}"
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn disconnect_from(&self, _dest_slot: SlotId) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            unsafe // SAFETY: PJSIP initialized; slot IDs valid conference bridge ports for disconnect
            {
                let status = pjsua_sys::pjsua_conf_disconnect(self.slot_id, _dest_slot);
                if status != crate::error::PJ_SUCCESS {
                    return Err(PjsipError::MediaPort(format!(
                        "pjsua_conf_disconnect returned {status}"
                    )));
                }
            }
        }
        Ok(())
    }
}
