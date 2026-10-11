//! Windows Core Audio endpoint notifications. Callbacks only post a bounded
//! event; enumeration, stream destruction and playback decisions stay on the UI.
use crate::audio::{OutputSignals, endpoint_hash};
use std::sync::Arc;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, PKEY_AudioEngine_DeviceFormat, eConsole,
    eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::core::{PCWSTR, Result, implement};

pub(crate) fn default_device_id() -> Result<String> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let id = device.GetId()?;
        let text = id.to_string();
        CoTaskMemFree(Some(id.0.cast()));
        Ok(text?)
    }
}

pub(crate) struct DeviceNotifications {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl DeviceNotifications {
    pub(crate) fn new(signals: Arc<OutputSignals>) -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let client: IMMNotificationClient = Notifications { signals }.into();
            enumerator.RegisterEndpointNotificationCallback(&client)?;
            Ok(Self { enumerator, client })
        }
    }
}

impl Drop for DeviceNotifications {
    fn drop(&mut self) {
        // The client must remain alive until after unregistration completes.
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.client);
        }
    }
}

#[implement(IMMNotificationClient)]
struct Notifications {
    signals: Arc<OutputSignals>,
}

fn id_hash(id: &PCWSTR) -> u64 {
    if id.is_null() {
        return 0;
    }
    // The OS owns this null-terminated ID for the duration of the callback.
    // Hash in place without allocation or a lock on the notification thread.
    unsafe { endpoint_hash(id.as_wide().iter().copied()) }
}

impl IMMNotificationClient_Impl for Notifications_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, _state: DEVICE_STATE) -> Result<()> {
        if self.signals.is_current_endpoint(id_hash(id)) {
            self.signals.endpoint_changed();
        } else {
            self.signals.device_changed();
        }
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> Result<()> {
        self.signals.device_changed();
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> Result<()> {
        if self.signals.is_current_endpoint(id_hash(id)) {
            self.signals.endpoint_changed();
        } else {
            self.signals.device_changed();
        }
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> Result<()> {
        // CPAL WASAPI uses the eConsole render endpoint.
        if flow == eRender && role == eConsole {
            self.signals.endpoint_changed();
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, id: &PCWSTR, key: &PROPERTYKEY) -> Result<()> {
        // Volume/name changes must not restart playback. Device-format changes
        // do require a fresh configuration/pool even when the endpoint ID stays.
        if key.fmtid == PKEY_AudioEngine_DeviceFormat.fmtid
            && key.pid == PKEY_AudioEngine_DeviceFormat.pid
        {
            if self.signals.is_current_endpoint(id_hash(id)) {
                self.signals.endpoint_changed();
            } else {
                self.signals.device_changed();
            }
        } else if self.signals.is_current_endpoint(id_hash(id)) {
            // Some jack drivers report restoration as an endpoint property
            // update. Probe it if disconnected; unchanged healthy outputs are
            // ignored by AudioOut, so ordinary name/volume updates do not restart.
            self.signals.device_changed();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::Audio::{DEVICE_STATE_ACTIVE, eCapture, eMultimedia};
    use windows::core::w;

    #[test]
    fn endpoint_callbacks_filter_roles_properties_and_force_current_endpoint_recovery() {
        let signals = OutputSignals::new();
        signals.set_endpoint("render-id");
        let client: IMMNotificationClient = Notifications {
            signals: signals.clone(),
        }
        .into();
        unsafe {
            client
                .OnDefaultDeviceChanged(eCapture, eConsole, w!("microphone"))
                .unwrap();
            client
                .OnDefaultDeviceChanged(eRender, eMultimedia, w!("other-role"))
                .unwrap();
            assert_eq!(signals.take(), (false, false, false));
            client
                .OnPropertyValueChanged(w!("render-id"), PROPERTYKEY::default())
                .unwrap();
            assert_eq!(signals.take(), (true, false, false));
            client
                .OnDeviceStateChanged(w!("microphone"), DEVICE_STATE_ACTIVE)
                .unwrap();
            assert_eq!(signals.take(), (true, false, false));
            client
                .OnDeviceStateChanged(w!("render-id"), DEVICE_STATE_ACTIVE)
                .unwrap();
            assert_eq!(
                signals.take(),
                (true, false, true),
                "same-ID jack events must recover silent stalls"
            );
            client
                .OnPropertyValueChanged(w!("render-id"), PKEY_AudioEngine_DeviceFormat)
                .unwrap();
            assert_eq!(signals.take(), (true, false, true));
            client
                .OnDefaultDeviceChanged(eRender, eConsole, w!("headphones"))
                .unwrap();
            assert_eq!(signals.take(), (true, false, true));
        }
    }
}
