//! `Glass.exe`: the window you double-click. A Windows-subsystem program, so
//! no console window opens with it.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    glass_win::gui_main();
    #[cfg(not(windows))]
    eprintln!("The Glass window is Windows-only; use glass-cli.");
}
