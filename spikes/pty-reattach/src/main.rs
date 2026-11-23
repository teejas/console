//! M0 spike (c): PTY ownership + screen snapshots with `portable-pty` and `vt100`.
//!
//! Doubles as a headless TUI driver so we can script Toad and capture what it renders.
//!
//! Usage: pty-reattach <script> -- <cmd> [args...]
//!
//! Script lines:
//!   size <cols> <rows>      (must be first; default 120x40)
//!   wait <ms>
//!   type <text>             (literal text, no newline)
//!   key <enter|esc|tab|up|down|ctrl-c|ctrl-d|ctrl-q|ctrl-p|backspace>
//!   snap <name>             write the current screen to $OUT/<name>.txt
//!   detach                  the "client" stops viewing; the owner keeps parsing output
//!   attach <name>           a new client gets a snapshot (contents_formatted) of the owner's
//!                           screen, replays it into a fresh parser, and we assert both match
//!   waitfor <text> <ms>     poll until the screen contains <text> or time out (non-fatal)
//!   # comment

use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn nz(v: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(v).expect("non-zero size")
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let sep = args
        .iter()
        .position(|a| a == "--")
        .context("usage: pty-reattach <script> -- <cmd>")?;
    let script = std::fs::read_to_string(&args[1])?;
    let cmd_args = &args[sep + 1..];
    let out = PathBuf::from(std::env::var("OUT").unwrap_or_else(|_| "pty-out".into()));
    std::fs::create_dir_all(&out)?;

    let (mut cols, mut rows) = (120u16, 40u16);
    if let Some(first) = script.lines().next() {
        if let Some(rest) = first.strip_prefix("size ") {
            let mut it = rest.split_whitespace();
            cols = it.next().unwrap().parse()?;
            rows = it.next().unwrap().parse()?;
        }
    }

    let pty = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new(&cmd_args[0]);
    cmd.args(&cmd_args[1..]);
    cmd.cwd(std::env::current_dir()?);
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let mut child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);

    // The owner (executor) side: always parses output, whether or not a client is attached.
    let parser = Arc::new(Mutex::new(vt100::Parser::new(nz(rows), nz(cols), 10_000)));
    let mut reader = pty.master.try_clone_reader()?;
    let raw_log = Arc::new(Mutex::new(std::fs::File::create(out.join("raw.log"))?));
    {
        let parser = parser.clone();
        let raw_log = raw_log.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                parser.lock().unwrap().process(&buf[..n]);
                let _ = raw_log.lock().unwrap().write_all(&buf[..n]);
            }
        });
    }
    let mut writer = pty.master.take_writer()?;
    let mut attached = true;

    for line in script.lines().skip_while(|l| l.starts_with("size ")) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (op, arg) = line.split_once(' ').unwrap_or((line, ""));
        match op {
            "wait" => std::thread::sleep(Duration::from_millis(arg.parse()?)),
            "type" => {
                for ch in arg.chars() {
                    write!(writer, "{ch}")?;
                    writer.flush()?;
                    std::thread::sleep(Duration::from_millis(8));
                }
            }
            "key" => {
                let bytes: &[u8] = match arg {
                    "enter" => b"\r",
                    "esc" => b"\x1b",
                    "tab" => b"\t",
                    "up" => b"\x1b[A",
                    "down" => b"\x1b[B",
                    "backspace" => b"\x7f",
                    "ctrl-c" => b"\x03",
                    "ctrl-d" => b"\x04",
                    "ctrl-p" => b"\x10",
                    "ctrl-q" => b"\x11",
                    "ctrl-s" => b"\x13",
                    "ctrl-n" => b"\x0e",
                    "ctrl-h" => b"\x08",
                    other => bail!("unknown key {other}"),
                };
                writer.write_all(bytes)?;
                writer.flush()?;
            }
            "snap" => {
                let text = parser.lock().unwrap().screen().contents();
                std::fs::write(out.join(format!("{arg}.txt")), &text)?;
                eprintln!("[snap {arg}] attached={attached}");
            }
            "waitfor" => {
                let (needle, ms) = arg.rsplit_once(' ').context("waitfor <text> <ms>")?;
                let deadline = Instant::now() + Duration::from_millis(ms.parse()?);
                let mut found = false;
                while Instant::now() < deadline {
                    if parser.lock().unwrap().screen().contents().contains(needle) {
                        found = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                eprintln!(
                    "[waitfor {needle:?}] {}",
                    if found { "found" } else { "TIMED OUT" }
                );
            }
            "detach" => {
                attached = false;
                eprintln!("[detach] client gone; owner keeps parsing");
            }
            "attach" => {
                // New client: receives a formatted snapshot and replays it into its own parser.
                let owner = parser.lock().unwrap();
                let snapshot = owner.screen().contents_formatted();
                let mut client = vt100::Parser::new(nz(rows), nz(cols), 0);
                client.process(snapshot.as_bytes());
                let same = client.screen().contents() == owner.screen().contents();
                let cursor_same =
                    client.screen().cursor_position() == owner.screen().cursor_position();
                std::fs::write(
                    out.join(format!("{arg}.client.txt")),
                    client.screen().contents(),
                )?;
                eprintln!(
                    "[attach {arg}] snapshot {} bytes; text match={same}; cursor match={cursor_same}",
                    snapshot.len()
                );
                attached = true;
            }
            other => bail!("unknown op {other}"),
        }
    }

    let _ = child.kill();
    Ok(())
}
