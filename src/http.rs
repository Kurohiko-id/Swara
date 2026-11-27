//! Small synchronous HTTPS client on WinHTTP. The update check reads with GET and usage
//! stats send with POST; nothing else talks to the network.

use std::ffi::c_void;
use std::iter::once;
use std::ptr::{null, null_mut};
use windows::Win32::Networking::WinHttp::*;
use windows::core::PCWSTR;

const USER_AGENT: &str = "Swara";
const HTTPS_PORT: u16 = 443;

/// Owns one WinHTTP handle; null means the open call failed and there is nothing to close.
struct Owned(*mut c_void);

impl Owned {
    fn opened(self, step: &str) -> Result<Self, String> {
        if self.0.is_null() {
            Err(last_error(step))
        } else {
            Ok(self)
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: a live handle from WinHttpOpen, WinHttpConnect or WinHttpOpenRequest,
        // released exactly once here.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn nul_terminated(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(once(0)).collect()
}

fn last_error(step: &str) -> String {
    format!("http {step}: {}", windows::core::Error::from_thread())
}

/// Request whose response headers have arrived. Declaration order makes the request handle
/// close before its connection and session.
struct Exchange {
    request: Owned,
    _connection: Owned,
    _session: Owned,
    status: u32,
}

/// One request to `https://host/path`; redirects are returned, never followed.
/// `timeout_ms` is applied to every WinHTTP stage separately.
fn exchange(
    host: &str,
    path: &str,
    method: &str,
    headers: &str,
    body: &[u8],
    timeout_ms: i32,
) -> Result<Exchange, String> {
    let agent_w = nul_terminated(USER_AGENT);
    let host_w = nul_terminated(host);
    let path_w = nul_terminated(path);
    let method_w = nul_terminated(method);
    let headers_w: Vec<u16> = headers.encode_utf16().collect();
    let body_len = u32::try_from(body.len()).map_err(|_| String::from("http body over 4 GiB"))?;

    // SAFETY: each pointer handed to WinHTTP refers to one of the buffers above, which live
    // until the function returns; every handle is wrapped in `Owned` so all exits close it.
    unsafe {
        let session = Owned(WinHttpOpen(
            PCWSTR(agent_w.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))
        .opened("open")?;
        let t = timeout_ms;
        WinHttpSetTimeouts(session.0, t, t, t, t).map_err(|_| last_error("set timeouts"))?;

        let connection = Owned(WinHttpConnect(
            session.0,
            PCWSTR(host_w.as_ptr()),
            HTTPS_PORT,
            0,
        ))
        .opened("connect")?;

        let request = Owned(WinHttpOpenRequest(
            connection.0,
            PCWSTR(method_w.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ))
        .opened("open request")?;

        let no_redirects = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
        WinHttpSetOption(
            Some(request.0),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(&no_redirects),
        )
        .map_err(|_| last_error("disable redirects"))?;

        let payload = match body {
            [] => None,
            bytes => Some(bytes.as_ptr().cast::<c_void>()),
        };
        WinHttpSendRequest(request.0, Some(&headers_w), payload, body_len, body_len, 0)
            .map_err(|_| last_error("send request"))?;
        WinHttpReceiveResponse(request.0, null_mut()).map_err(|_| last_error("await response"))?;

        let mut status: u32 = 0;
        let mut status_len = std::mem::size_of_val(&status) as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_FLAG_NUMBER | WINHTTP_QUERY_STATUS_CODE,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut status_len,
            null_mut(),
        )
        .map_err(|_| last_error("read status"))?;

        Ok(Exchange {
            request,
            _connection: connection,
            _session: session,
            status,
        })
    }
}

pub struct Reply {
    pub status: u32,
    /// Redirect target, left for the caller to vet before it follows anything.
    pub location: Option<String>,
}

/// GET without automatic redirects, so trust decisions stay with the caller. Only a 200 body
/// is passed to `sink`, and the call fails once the body exceeds `max` bytes.
pub fn get(
    host: &str,
    path: &str,
    headers: &str,
    timeout_ms: i32,
    max: u64,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<Reply, String> {
    let done = exchange(host, path, "GET", headers, &[], timeout_ms)?;
    let request = done.request.0;
    let status = done.status;

    if status != 200 {
        let mut text = [0u16; 8192];
        let mut text_bytes = std::mem::size_of_val(&text) as u32;
        // SAFETY: `request` is open while `done` is alive and `text` outlives the call.
        let found = unsafe {
            WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_LOCATION,
                PCWSTR::null(),
                Some(text.as_mut_ptr().cast()),
                &mut text_bytes,
                null_mut(),
            )
        };
        let units = text_bytes as usize / 2;
        let location = found
            .ok()
            .and_then(|_| String::from_utf16(&text[..units]).ok());
        return Ok(Reply { status, location });
    }

    let mut chunk = [0u8; 16384];
    let mut received: u64 = 0;
    loop {
        let mut got = 0u32;
        // SAFETY: `request` is open while `done` is alive; `chunk` bounds the write.
        unsafe {
            WinHttpReadData(
                request,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut got,
            )
        }
        .map_err(|_| last_error("read body"))?;
        if got == 0 {
            break;
        }
        received += u64::from(got);
        if received > max {
            return Err(format!("http body over {max} bytes"));
        }
        sink(&chunk[..got as usize])?;
    }
    Ok(Reply {
        status,
        location: None,
    })
}

/// POST JSON and report just the status; the server's reply body is ignored.
pub fn post_json(host: &str, path: &str, body: &[u8], timeout_ms: i32) -> Result<u32, String> {
    const JSON_HEADER: &str = "Content-Type: application/json\r\n";
    let done = exchange(host, path, "POST", JSON_HEADER, body, timeout_ms)?;
    Ok(done.status)
}
