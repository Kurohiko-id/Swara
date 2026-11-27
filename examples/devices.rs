use swara::audio::{Audio, Flow, Roles, set_default};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

fn main() {
    // SAFETY: first COM call on this thread.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .expect("CoInitializeEx");
    let audio = Audio::new(HWND::default()).expect("Audio::new");
    for (label, flow) in [("render", Flow::Render), ("capture", Flow::Capture)] {
        println!("{label}:");
        for d in audio.list(flow) {
            println!("  {} {} {}", d.present, d.id, d.name);
        }
    }
    println!("{:#?}", audio.current());

    if let Some(id) = std::env::args().nth(1) {
        set_default(&id, Roles::All).expect("set_default");
        println!("set_default ok: {id}");
        println!("{:#?}", audio.current());
    }
}
