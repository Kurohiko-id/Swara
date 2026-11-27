use swara::guard::Side;
use swara::i18n::{Key, ResolvedLang};
use swara::popup::{Choice, PromptRow, WM_APP_PROMPT_CHOICE, hide_all, show_osd, show_prompt};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow, GetMessageW,
    HWND_MESSAGE, KillTimer, MSG, SetTimer, WINDOW_EX_STYLE, WINDOW_STYLE, WM_TIMER,
};
use windows::core::w;

fn main() {
    // SAFETY: first COM call on this thread.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .expect("CoInitializeEx");
    // SAFETY: message-only window of a system class; destroyed before exit.
    let owner = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            None,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
    .expect("owner window");

    // SAFETY: read-only query.
    let before = unsafe { GetForegroundWindow() };
    show_osd(owner, "Headset\tNum /");
    let rows = [PromptRow {
        now_label: Key::PopupNow,
        now: "Speaker".into(),
        pick: "Headset".into(),
    }];
    show_prompt(owner, ResolvedLang::Id, Side::Out, &rows, 15).expect("show_prompt");
    println!("shown; click a button within 15 s");

    // SAFETY: thread timers, killed below.
    let (check, timeout) = unsafe {
        (
            SetTimer(None, 0, 500, None),
            SetTimer(None, 0, 15_000, None),
        )
    };
    let mut queued = MSG::default();
    // SAFETY: standard message loop on this thread.
    while unsafe { GetMessageW(&mut queued, None, 0, 0) }.as_bool() {
        if queued.message == WM_APP_PROMPT_CHOICE {
            let side = if queued.lParam.0 == 0 { "Out" } else { "In" };
            println!(
                "choice: {:?}, side: {side}",
                Choice::from_index(queued.wParam.0)
            );
            break;
        }
        if queued.message == WM_TIMER && queued.hwnd.is_invalid() && queued.wParam.0 == check {
            // SAFETY: read-only query; timer id came from SetTimer.
            let after = unsafe {
                let _ = KillTimer(None, check);
                GetForegroundWindow()
            };
            println!(
                "foreground before={before:?} after_500ms={after:?} changed={}",
                before != after
            );
            continue;
        }
        if queued.message == WM_TIMER && queued.hwnd.is_invalid() && queued.wParam.0 == timeout {
            println!("no choice after 15 s");
            break;
        }
        // SAFETY: dispatching a message this loop just received.
        unsafe { DispatchMessageW(&queued) };
    }
    hide_all();
    // SAFETY: timer ids and window created above.
    unsafe {
        let _ = KillTimer(None, timeout);
        let _ = DestroyWindow(owner);
    }
}
