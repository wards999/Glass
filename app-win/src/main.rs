//! Glass for Windows: host and client roles in one app.
//!
//! LATER(M1): host capture (Windows.Graphics.Capture) -> encode (ffmpeg hevc_*) to file.
//! LATER(M3b): client decode (D3D11VA) -> flip-model present, Raw Input.
//! LATER(M6): tray shell, device list, pairing UI.

fn main() {
    println!(
        "Glass (Windows) {} | core {} | protocol v{}",
        env!("CARGO_PKG_VERSION"),
        glass_core::VERSION,
        glass_core::protocol::PROTOCOL_VERSION
    );
}
