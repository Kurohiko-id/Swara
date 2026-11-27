//! `IPolicyConfig` has no public header, yet it is the sole route to setting a default endpoint.
//! Swara only calls `SetDefaultEndpoint`; every other method exists to keep the vtable aligned.
#![allow(non_snake_case)]
use std::ffi::c_void;
use windows::Win32::Media::Audio::ERole;
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR, interface};

pub const POLICY_CONFIG_CLASS: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

#[interface("F8679F50-850A-41CF-9C72-430F290290C8")]
pub unsafe trait IPolicyConfig: IUnknown {
    fn GetMixFormat(&self, device: PCWSTR, format: *mut c_void) -> HRESULT;
    fn GetDeviceFormat(&self, device: PCWSTR, use_default: i32, format: *mut c_void) -> HRESULT;
    fn ResetDeviceFormat(&self, device: PCWSTR) -> HRESULT;
    fn SetDeviceFormat(
        &self,
        device: PCWSTR,
        endpoint_format: *mut c_void,
        mix_format: *mut c_void,
    ) -> HRESULT;
    fn GetProcessingPeriod(
        &self,
        device: PCWSTR,
        use_default: i32,
        default_period: *mut i64,
        min_period: *mut i64,
    ) -> HRESULT;
    fn SetProcessingPeriod(&self, device: PCWSTR, period: *mut i64) -> HRESULT;
    fn GetShareMode(&self, device: PCWSTR, share: *mut c_void) -> HRESULT;
    fn SetShareMode(&self, device: PCWSTR, share: *mut c_void) -> HRESULT;
    fn GetPropertyValue(&self, device: PCWSTR, prop: *const c_void, value: *mut c_void) -> HRESULT;
    fn SetPropertyValue(&self, device: PCWSTR, prop: *const c_void, value: *mut c_void) -> HRESULT;
    pub fn SetDefaultEndpoint(&self, device: PCWSTR, role: ERole) -> HRESULT;
    fn SetEndpointVisibility(&self, device: PCWSTR, shown: i32) -> HRESULT;
}
