//! Core Audio access: list endpoints, read the current defaults, and change them.

use crate::devices::DeviceInfo;
use crate::policy_config::{IPolicyConfig, POLICY_CONFIG_CLASS};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{HWND, LPARAM, PROPERTYKEY, WPARAM};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_UNPLUGGED, EDataFlow, ERole, IMMDevice,
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
    eCapture, eCommunications, eConsole, eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{GUID, HSTRING, Interface, PCWSTR, Result, implement};

pub const WM_APP_DEVICES: u32 = WM_APP + 1;

/// Index order shared by every per-role triple in the app.
const EVERY_ROLE: [ERole; 3] = [eConsole, eMultimedia, eCommunications];

/// One endpoint id per role, indexed like `EVERY_ROLE`.
pub type RoleIds = [Option<String>; 3];

pub enum Flow {
    Capture,
    Render,
}

impl Flow {
    fn edata(self) -> EDataFlow {
        match self {
            Flow::Capture => eCapture,
            Flow::Render => eRender,
        }
    }
}

pub enum Roles {
    All,
    ConsoleMultimedia,
    Comms,
}

impl Roles {
    fn targets(&self) -> &'static [ERole] {
        match self {
            Roles::All => &EVERY_ROLE,
            Roles::ConsoleMultimedia => &[eConsole, eMultimedia],
            Roles::Comms => &[eCommunications],
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Current {
    pub render: RoleIds,
    pub capture: RoleIds,
}

// COM calls this from its own worker threads, where an HWND (not Send) cannot travel.
#[implement(IMMNotificationClient)]
struct ChangeSink {
    target: isize,
}

impl ChangeSink_Impl {
    fn wake_core(&self) -> Result<()> {
        let hwnd = HWND(self.target as *mut _);
        // SAFETY: posting only enqueues; if the window is gone the call fails harmlessly.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_DEVICES, WPARAM(0), LPARAM(0)) };
        Ok(())
    }
}

impl IMMNotificationClient_Impl for ChangeSink_Impl {
    fn OnDeviceStateChanged(&self, _endpoint: &PCWSTR, _state: DEVICE_STATE) -> Result<()> {
        self.wake_core()
    }

    fn OnDeviceAdded(&self, _endpoint: &PCWSTR) -> Result<()> {
        self.wake_core()
    }

    fn OnDeviceRemoved(&self, _endpoint: &PCWSTR) -> Result<()> {
        self.wake_core()
    }

    fn OnDefaultDeviceChanged(
        &self,
        _flow: EDataFlow,
        _role: ERole,
        _endpoint: &PCWSTR,
    ) -> Result<()> {
        self.wake_core()
    }

    fn OnPropertyValueChanged(&self, _endpoint: &PCWSTR, _key: &PROPERTYKEY) -> Result<()> {
        // Very chatty, and nothing Swara tracks depends on it.
        Ok(())
    }
}

pub struct Audio {
    devices: IMMDeviceEnumerator,
    sink: IMMNotificationClient,
}

fn endpoint_id(endpoint: &IMMDevice) -> Result<String> {
    // SAFETY: GetId allocates with CoTaskMem; the text is copied out before the single free.
    unsafe {
        let raw = endpoint.GetId()?;
        let copied = raw.to_string();
        CoTaskMemFree(Some(raw.0.cast_const().cast()));
        Ok(copied.unwrap_or_default())
    }
}

fn friendly_name(endpoint: &IMMDevice) -> String {
    // SAFETY: COM calls on a live endpoint; the returned PROPVARIANT frees itself.
    let value = unsafe {
        endpoint
            .OpenPropertyStore(STGM_READ)
            .and_then(|props| props.GetValue(&PKEY_Device_FriendlyName))
    };
    value.map(|v| v.to_string()).unwrap_or_default()
}

fn create<T: Interface>(class: &GUID) -> Result<T> {
    // SAFETY: every caller runs on a thread where COM is already initialised.
    unsafe { CoCreateInstance(class, None, CLSCTX_ALL) }
}

impl Audio {
    /// The calling thread must have COM initialised.
    pub fn new(core_window: HWND) -> Result<Self> {
        let sink: IMMNotificationClient = ChangeSink {
            target: core_window.0 as isize,
        }
        .into();
        let devices: IMMDeviceEnumerator = create(&MMDeviceEnumerator)?;
        // SAFETY: `sink` is kept in Self and unregistered in Drop.
        unsafe { devices.RegisterEndpointNotificationCallback(&sink)? };
        Ok(Self { devices, sink })
    }

    pub fn list(&self, which: Flow) -> Vec<DeviceInfo> {
        let wanted = DEVICE_STATE(DEVICE_STATE_ACTIVE.0 | DEVICE_STATE_UNPLUGGED.0);
        // SAFETY: COM calls on the enumerator and its collection; failures drop the entry.
        unsafe {
            let Ok(found) = self.devices.EnumAudioEndpoints(which.edata(), wanted) else {
                return Vec::new();
            };
            let total = found.GetCount().unwrap_or(0);
            (0..total)
                .filter_map(|n| {
                    let endpoint = found.Item(n).ok()?;
                    let id = endpoint_id(&endpoint).ok()?;
                    let state = endpoint.GetState();
                    Some(DeviceInfo {
                        id,
                        name: friendly_name(&endpoint),
                        present: state.is_ok_and(|st| st == DEVICE_STATE_ACTIVE),
                    })
                })
                .collect()
        }
    }

    /// Console output default alone: a hotkey press needs one lookup, not the six of `current`.
    pub fn default_output(&self) -> Option<String> {
        // SAFETY: COM call on the enumerator; no default reports an error, kept as None.
        let found = unsafe { self.devices.GetDefaultAudioEndpoint(eRender, eConsole) };
        found.and_then(|endpoint| endpoint_id(&endpoint)).ok()
    }

    pub fn current(&self) -> Current {
        let ids_for = |flow: EDataFlow| -> RoleIds {
            EVERY_ROLE.map(|role| {
                // SAFETY: COM call; a role with no default reports E_NOTFOUND, kept as None.
                let found = unsafe { self.devices.GetDefaultAudioEndpoint(flow, role) };
                found.and_then(|endpoint| endpoint_id(&endpoint)).ok()
            })
        };
        Current {
            render: ids_for(eRender),
            capture: ids_for(eCapture),
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        // SAFETY: the same sink that `new` registered.
        let _ = unsafe {
            self.devices
                .UnregisterEndpointNotificationCallback(&self.sink)
        };
    }
}

/// Sole writer of default endpoints. The calling thread must have COM initialised.
pub fn set_default(id: &str, roles: Roles) -> Result<()> {
    let wide = HSTRING::from(id);
    let device = PCWSTR(wide.as_ptr());
    let policy: IPolicyConfig = create(&POLICY_CONFIG_CLASS)?;
    // Every role is tried: stopping at the first failure would leave the others behind for no
    // reason. The first error is reported.
    let mut first_error = None;
    for &role in roles.targets() {
        // SAFETY: `wide` stays alive until this function returns, so `device` is valid.
        if let Err(e) = unsafe { policy.SetDefaultEndpoint(device, role) }.ok() {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}
