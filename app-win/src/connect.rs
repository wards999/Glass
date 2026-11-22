//! `glass connect`: pair with a host if needed, then view and control it.
//!
//! `glass connect [HOST[:PORT]] [--pin PIN] [--seconds S] [--windowed]
//!                [--keys pc|labelled] [--view mac|fit|space|larger] [--colour 444|420]
//!                [--log FILE]`
//!
//! View modes (M3c): Ctrl+Alt+Shift+R while viewing cycles the host's own
//! screen and three "fit to this PC" sizes; the last one is remembered per
//! host address (`view-modes.txt`) and asked for when connecting.
//!
//! Keys: with a Mac host the PC's Ctrl acts as ⌘ (so Ctrl+C copies)
//! and Win as ⌃; with a PC host they stay as labelled. `--keys pc|labelled` overrides.

use crate::win::viewer::{self, Options};
use glass_core::identity::{default_dir, fingerprint_short};
use glass_core::keymap::PcModifierRule;
use glass_core::net::{pair_begin, Client, ConnectError};
use glass_core::protocol::DEFAULT_PORT;
use std::error::Error;
use std::io::{BufRead, Write};

type Res<T> = Result<T, Box<dyn Error>>;

fn ask(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().to_string()
}

/// "host" or "host:port".
fn parse_address(s: &str) -> Res<(String, u16)> {
    let s = s.trim();
    match s.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !h.contains(':') => Ok((h.to_string(), p.parse()?)),
        _ => Ok((s.to_string(), DEFAULT_PORT)),
    }
}

pub fn connect(args: &[String]) -> Res<()> {
    let (mut addr, mut pin, mut seconds, mut log) = (None, None, None, None);
    let mut view: Option<u8> = None;
    let mut opts = Options {
        windowed: false,
        seconds: None,
        rule: None, // from the host (Mac: Ctrl acts as ⌘; PC: as labelled)
        // 4:2:0 by default: in m3c-test4 4:4:4 cost ~25 ms on the Mac.
        full_colour: false,
        clipboard: true,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--pin" => pin = Some(val()?.clone()),
            "--seconds" => seconds = Some(val()?.parse::<f64>()?),
            "--log" => log = Some(val()?.clone()),
            "--windowed" => opts.windowed = true,
            "--no-clipboard" => opts.clipboard = false,
            "--colour" | "--color" => {
                opts.full_colour = match val()?.as_str() {
                    "444" | "4:4:4" => true,
                    "420" | "4:2:0" => false,
                    v => return Err(format!("--colour {v}: use 444 or 420").into()),
                }
            }
            "--view" => {
                view = Some(match val()?.as_str() {
                    "mac" | "host" => 0,
                    "fit" => 1,
                    "space" => 2,
                    "larger" => 3,
                    v => return Err(format!("--view {v}: use mac, fit, space or larger").into()),
                })
            }
            "--keys" => {
                opts.rule = Some(match val()?.as_str() {
                    "pc" => PcModifierRule::PcStyle,
                    "labelled" | "labeled" => PcModifierRule::MacStyle,
                    k => return Err(format!("--keys {k}: use pc or labelled").into()),
                })
            }
            s if addr.is_none() && !s.starts_with("--") => addr = Some(s.to_string()),
            s => return Err(format!("unknown argument {s}").into()),
        }
    }
    opts.seconds = seconds;

    let device = crate::device()?;
    println!(
        "{}\nThis PC: \"{}\", id {}",
        crate::banner(),
        device.name,
        fingerprint_short(&device.fingerprint())
    );
    let last_file = default_dir().unwrap_or_default().join("last-host.txt");
    let last = std::fs::read_to_string(&last_file).unwrap_or_default();
    let addr = match addr {
        Some(a) => a,
        None => {
            let prompt = if last.trim().is_empty() {
                "Address of the Mac (Glass on the Mac shows it under Share This Mac): ".to_string()
            } else {
                format!("Address of the Mac [{}]: ", last.trim())
            };
            let a = ask(&prompt);
            if a.is_empty() {
                last.trim().to_string()
            } else {
                a
            }
        }
    };
    if addr.is_empty() {
        return Err("no address given".into());
    }
    let (host, port) = parse_address(&addr)?;
    let err = |e: ConnectError| -> Box<dyn Error> { format!("{host}:{port}: {e}").into() };
    let modes_file = default_dir().unwrap_or_default().join("view-modes.txt");
    // --view wins; otherwise the view last used with this host.
    let mode = view.unwrap_or_else(|| saved_mode(&modes_file, &addr));
    let prep = viewer::prepare(mode, opts.full_colour)?;
    let hello = prep.hello;

    let mut fixed_pin = pin;
    let client = loop {
        match Client::connect_with(&device, &host, port, hello) {
            Ok(c) => break c,
            Err(ConnectError::NotPaired) => {
                println!("This PC isn't paired with {host} yet. Asking it to show a PIN ...");
                let p = pair_begin(&device, &host, port).map_err(err)?;
                let pin = match fixed_pin.take() {
                    Some(p) => p,
                    None => {
                        println!(
                            "\"{}\" is showing a 6-digit PIN. Pair only with your own devices: \
                             a paired host sees what you type while you view it.",
                            p.host_name()
                        );
                        ask("PIN (Enter to cancel): ")
                            .chars()
                            .filter(char::is_ascii_digit)
                            .collect()
                    }
                };
                if pin.is_empty() {
                    return Err("pairing cancelled".into());
                }
                let name = p.host_name().to_string();
                match p.finish(&pin) {
                    Ok(_) => println!("Paired with \"{name}\"."),
                    Err(ConnectError::WrongPin) if args.iter().all(|a| a != "--pin") => {
                        println!("That PIN wasn't right (or it expired). Try again.");
                    }
                    Err(e) => return Err(err(e)),
                }
            }
            Err(e) => return Err(err(e)),
        }
    };
    let _ = std::fs::write(&last_file, &addr);
    let info = client.info();
    println!(
        "Connected to \"{}\" ({}x{} @ {} Hz, encrypted). Ctrl+Alt+Shift+F: window / full screen, \
         Ctrl+Alt+Shift+R: view ({}), Ctrl+Alt+Shift+Q: disconnect.{}",
        client.peer().name,
        info.width,
        info.height,
        info.fps,
        viewer::mode_name(mode),
        if hello.caps & glass_core::protocol::CAP_HEVC_444 == 0 {
            ""
        } else if opts.full_colour {
            " Full colour (4:4:4) is on when the host can send it (Ctrl+Alt+Shift+C: 4:2:0)."
        } else {
            " Ctrl+Alt+Shift+C: full colour (4:4:4), sharper coloured detail but slower."
        }
    );
    let (report, last_mode) = viewer::run(client, prep, opts, &crate::banner())?;
    save_mode(&modes_file, &addr, last_mode);
    println!("\n{report}");
    if let Some(path) = log {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{report}")?;
        println!("(appended to {path})");
    }
    Ok(())
}

/// The view mode last used with `addr`; new hosts start in "fit" (fills
/// this screen, no black bars).
fn saved_mode(file: &std::path::Path, addr: &str) -> u8 {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.rsplit_once(' '))
        .find(|(a, _)| *a == addr)
        .and_then(|(_, m)| m.parse().ok())
        .unwrap_or(glass_core::protocol::MODE_FIT)
}

fn save_mode(file: &std::path::Path, addr: &str, mode: u8) {
    let old = std::fs::read_to_string(file).unwrap_or_default();
    let mut lines: Vec<String> = old
        .lines()
        .filter(|l| l.rsplit_once(' ').is_some_and(|(a, _)| a != addr))
        .map(str::to_string)
        .collect();
    lines.push(format!("{addr} {mode}"));
    let _ = std::fs::write(file, lines.join("\n") + "\n");
}

/// `glass-cli debug-keys ADDR[:PORT]`: connects without a window and sends a
/// few harmless F15 presses and pointer moves, to check a host's input path
/// (the device must already be paired). For local checks only.
pub fn debug_keys(args: &[String]) -> Res<()> {
    let target = args
        .first()
        .ok_or("usage: glass-cli debug-keys ADDR[:PORT]")?;
    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse()?),
        _ => (target.clone(), DEFAULT_PORT),
    };
    let device = crate::device()?;
    let prep = viewer::prepare(glass_core::protocol::MODE_FIT, false)?;
    let c = Client::connect_with(&device, &host, port, prep.hello)?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    for i in 0..5u16 {
        c.input_move(30000 + i * 100, 30000);
        c.input_key(0x6A, true); // F15
        std::thread::sleep(std::time::Duration::from_millis(60));
        c.input_key(0x6A, false);
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    println!("sent 5 F15 presses");
    drop(c);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_address, save_mode, saved_mode};

    #[test]
    fn modes_are_remembered_per_address() {
        let f = std::env::temp_dir().join(format!("glass-modes-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&f);
        assert_eq!(saved_mode(&f, "192.168.4.28"), 1); // new host: fit
        save_mode(&f, "192.168.4.28", 2);
        save_mode(&f, "mac.local:47811", 1);
        save_mode(&f, "192.168.4.28", 3);
        assert_eq!(saved_mode(&f, "192.168.4.28"), 3);
        assert_eq!(saved_mode(&f, "mac.local:47811"), 1);
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn addresses() {
        assert_eq!(
            parse_address("192.168.4.20").unwrap(),
            ("192.168.4.20".into(), 47800)
        );
        assert_eq!(
            parse_address(" mac.local:47811 ").unwrap(),
            ("mac.local".into(), 47811)
        );
        assert!(parse_address("host:notaport").is_err());
    }
}
