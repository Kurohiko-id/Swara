const ICON: &str = "assets/swara.ico";
const MANIFEST: &str = "assets/swara.manifest";
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

// FileDescription doubles as the process name in Task Manager.
const VERSION_FIELDS: [(&str, &str); 7] = [
    ("ProductName", "Swara"),
    ("FileDescription", "Swara - Audio Output Switcher"),
    ("Comments", env!("CARGO_PKG_DESCRIPTION")),
    ("CompanyName", "Kurohiko"),
    ("LegalCopyright", "Copyright (c) 2026 Kurohiko"),
    ("ProductVersion", PKG_VERSION),
    ("FileVersion", PKG_VERSION),
];

fn main() {
    for input in [ICON, MANIFEST] {
        println!("cargo:rerun-if-changed={input}");
    }

    let mut resources = winresource::WindowsResource::new();
    resources.set_icon(ICON).set_manifest_file(MANIFEST);
    for (field, value) in VERSION_FIELDS {
        resources.set(field, value);
    }
    if let Err(e) = resources.compile() {
        panic!("embedding icon/manifest/version info: {e}");
    }

    // Fixed light style so the settings window ignores the OS theme and keeps its own colours.
    let slint_config = slint_build::CompilerConfiguration::new().with_style("fluent-light".into());
    if let Err(e) = slint_build::compile_with_config("ui/settings.slint", slint_config) {
        panic!("ui/settings.slint: {e}");
    }
}
