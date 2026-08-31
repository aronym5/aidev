//! Prozessausführung mit Timeout und nonblocking-Pipes.
//!
//! Enthält `run_with_timeout`, `run_with_timeout_live` (Unix/non-Unix),
//! Prozessgruppen-Kill, Pipe-Drain, Ausgabe-Begrenzung und Hilfsfunktionen
//! für Container-Identität (`host_uid_gid`) und Namen (`sanitize`).

use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use super::RunOut;

/// Führt ein Kommando mit Timeout aus und kapselt Stdout/Stderr.
///
/// Robust gegen hängende Ausgabepipes: Es wird **nie auf EOF gewartet**. Nach
/// Kind-Ende bzw. Timeout wird der Puffer nur noch kurz (Grace-Fenster)
/// nachgelesen – auch wenn ein Enkelprozess die Pipes geerbt hat und offen hält.
/// Bei Timeout wird das Kind samt seiner Prozessgruppe beendet; vor dem
/// Gruppen-Kill wird die Gruppenzugehörigkeit gegen `/proc` verifiziert, damit
/// nie eine fremde Prozessgruppe getroffen werden kann.
///
/// (Auf Nicht-Unix-Systemen fällt die Funktion auf die einfachere
/// Thread-basierte Variante zurück, die an geerbten Pipes hängen kann.)
#[cfg(unix)]
pub(super) fn run_with_timeout(
    cmd: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<RunOut, String> {
    run_with_timeout_live(cmd, args, cwd, timeout, None, None)
}

/// Wie `run_with_timeout`, meldet aber bereits während des Laufs anfallende
/// Ausgabe (stdout+stderr) abschnittsweise über `live` – für die Live-Vorschau
/// der Run-Konsolen-Box. Die Zwischenausgabe kommt unabhängig von stdout/stderr
/// in Ankunftsreihenfolge; für den Endwert gelten die gleichen Grenzen wie in
/// `run_with_timeout`.
///
/// Ist `cancel` gesetzt (z. B. durch `Esc`), wird der laufende Kindprozess samt
/// Prozessgruppe **sofort** beendet und der Lauf als abgebrochen gemeldet
/// (`exit_code == None`) – statt bis zum natürlichen Ende bzw. Timeout zu
/// warten.
#[cfg(unix)]
pub(super) fn run_with_timeout_live(
    cmd: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    mut live: Option<&mut dyn FnMut(&str)>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<RunOut, String> {
    use std::os::unix::process::CommandExt;
    let mut builder = Command::new(cmd);
    builder
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0); // eigenes PGID → Gruppen-Kill bei Timeout möglich

    let mut child = builder
        .spawn()
        .map_err(|e| format!("{cmd} nicht startbar: {e}"))?;
    let pid = child.id();

    // Pipes non-blocking anlegen – gelesen wird in einer einzigen Schleife.
    let mut stdout = child.stdout.take().expect("stdout-Pipe");
    let mut stderr = child.stderr.take().expect("stderr-Pipe");
    use std::os::fd::AsRawFd;
    set_nonblocking(stdout.as_raw_fd()).map_err(|e| format!("stdout nonblocking: {e}"))?;
    set_nonblocking(stderr.as_raw_fd()).map_err(|e| format!("stderr nonblocking: {e}"))?;

    let deadline = Instant::now() + timeout;
    let mut out = String::new();
    let mut err = String::new();
    let mut out_eof = false;
    let mut err_eof = false;
    let mut timed_out = false;
    let mut exit_code: Option<i32> = None;
    let mut done = false; // Kind beendet (try_wait) bzw. Timeout abgeschlossen

    // Rest nachlesen: kurzes Fester nach Kind-Ende/Timeout statt EOF-Warten.
    let mut drain_until: Option<Instant> = None;
    const GRACE: Duration = Duration::from_millis(250);

    loop {
        // Sofortiger Abbruch bei gesetztem Cancel-Flag: die Prozessgruppe
        // beenden und – wie beim Timeout – nur noch ein kurzes Restfenster
        // nachlesen, damit der Endwert als "abgebrochen" gemeldet wird.
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            kill_process_group(pid);
            let _ = child.kill();
            let _ = child.wait();
            exit_code = None; // abgebrochen → als abgebrochen markieren
            done = true;
            drain_until = Some(Instant::now() + GRACE);
        }

        let (out_eof_new, added_out) = drain_pipe(&mut stdout, &mut out);
        out_eof |= out_eof_new;
        trim_front(&mut out, RUN_OUTPUT_MEM_CAP);
        let (err_eof_new, added_err) = drain_pipe(&mut stderr, &mut err);
        err_eof |= err_eof_new;
        trim_front(&mut err, RUN_OUTPUT_MEM_CAP);
        // Neu angekommene Ausgabe sofort an den Live-Sink durchreichen: das sind
        // die zuletzt angehängten Bytes am Ende des (ggf. von vorn gekürzten)
        // Schwanz-Speichers – das echte Ende geht so nie verloren.
        if let Some(cb) = live.as_mut() {
            if added_out > 0 {
                cb(&out[out.len() - added_out..]);
            }
            if added_err > 0 {
                cb(&err[err.len() - added_err..]);
            }
        }

        if !done && !timed_out {
            if let Some(status) = child.try_wait().map_err(|e| format!("wait-Fehler: {e}"))? {
                exit_code = status.code();
                done = true;
                drain_until = Some(Instant::now() + GRACE);
            }
        }

        if !timed_out && !done && Instant::now() >= deadline {
            timed_out = true;
            kill_process_group(pid);
            let _ = child.kill();
            let _ = child.wait();
            exit_code = None; // Timeout → als abgebrochen markieren
            done = true;
            drain_until = Some(Instant::now() + GRACE);
        }

        if done && drain_until.is_some_and(|end| Instant::now() >= end || (out_eof && err_eof)) {
            break;
        }

        thread::sleep(Duration::from_millis(5));
    }

    Ok(RunOut {
        exit_code,
        stdout: truncate_output(&out),
        stderr: truncate_output(&err),
    })
}

/// Liest alles gerade Verfügbare aus einer non-blocking Pipe und hängt es an
/// `out` (vorerst unbegrenzt; die Speicher-Kürzung übernimmt `trim_front` im
/// Aufrufer). Liefert `(eof, added)`: `eof`, wenn alle Schreiber die Pipe
/// geschlossen haben, und `added` als Anzahl der angehängten Bytes (für den
/// Live-Sink).
#[cfg(unix)]
pub(super) fn drain_pipe<R: Read>(pipe: &mut R, out: &mut String) -> (bool, usize) {
    let start = out.len();
    let mut buf = [0u8; 8192];
    let mut eof = false;
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break, // Lesefehler (z. B. Pipe zu) wie Ende behandeln
        }
    }
    (eof, out.len() - start)
}

/// Speichergrenze der Run-Ausgabe während des Laufs (roh nach Bytes): fällt
/// die Ausgabe darüber, wird von VORN verworfen (nie vom Ende).
pub(super) const RUN_OUTPUT_MEM_CAP: usize = 8 * 1024 * 1024;

/// Verwirft von VORN, bis `out` wieder unter `cap` Bytes liegt – möglichst an
/// ganzen Zeilen und nie mitten in UTF-8. Dadurch bleibt stets das ENDE der
/// Ausgabe erhalten. Liefert `true`, wenn gekürzt wurde.
pub(super) fn trim_front(out: &mut String, cap: usize) -> bool {
    if out.len() <= cap {
        return false;
    }
    // Char-Grenze für die Abtrennung, ohne UTF-8-Sequenzen zu zerschneiden.
    let mut cut = out.len() - cap;
    while cut > 0 && !out.is_char_boundary(cut) {
        cut -= 1;
    }
    out.drain(..cut);
    // Nicht mitten in einer Zeile beginnen: die erste (meist halbe) Zeile des
    // Restbestandes gleich mit verwerfen, solange danach noch etwas bleibt.
    if let Some(nl) = out.find('\n') {
        let drop_to = nl + 1;
        if drop_to < out.len() {
            out.drain(..drop_to);
        }
    }
    true
}

/// Beendet bei Timeout die Prozessgruppe des Kindes (`kill(-pgid, SIGKILL)`),
/// damit auch Nachkommen sterben. Erst wenn auf Linux verifiziert wurde, dass
/// das Kind noch in ihrer eigenen Gruppe liegt (`pgrp == pid`), kann der
/// Gruppen-Kill ausschließlich unsere eigene Gruppe treffen – andernfalls
/// bleibt es beim direkten Kill des Kindes (durch den Aufrufer).
#[cfg(unix)]
pub(super) fn kill_process_group(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        if proc_pgrp(pid) == Some(pid as i32) {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

/// Liest die Prozessgruppen-ID (`pgrp`, Feld 5) des Prozesses `pid` aus
/// `/proc/<pid>/stat`. Robust gegen Leerzeichen/Klammern im Komma: hinter der
/// letzten `)` beginnen die restlichen Felder, `pgrp` steht an Position 3.
#[cfg(target_os = "linux")]
pub(super) fn proc_pgrp(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rfind(')')?;
    let fields: Vec<&str> = stat[after + 1..].split_whitespace().collect();
    // Felder hinter `comm`: state(3) ppid(4) pgrp(5) → Index 2.
    fields.get(2).and_then(|f| f.parse().ok())
}

/// Setzt den `O_NONBLOCK`-Pipe-Flag auf einem Deskriptor (Unix).
#[cfg(unix)]
pub(super) fn set_nonblocking(fd: i32) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Einfache Fallback-Variante für Nicht-Unix-Systeme: Thread-basierter
/// EOF-Lesezugriff. Kann an geerbten Pipes hängen – auf Unix ist die
/// Non-blocking-Variante oben aktiv und kommt auf jedem unix-Verwandten
/// (Linux, macOS, BSD) zum Einsatz.
#[cfg(not(unix))]
pub(super) fn run_with_timeout(
    cmd: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<RunOut, String> {
    let mut child = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{cmd} nicht startbar: {e}"))?;

    let stdout_handle = {
        let mut out = child.stdout.take().expect("stdout-Pipe");
        thread::spawn(move || {
            let mut text = String::new();
            let _ = out.read_to_string(&mut text);
            text
        })
    };
    let stderr_handle = {
        let mut err = child.stderr.take().expect("stderr-Pipe");
        thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            text
        })
    };

    let deadline = Instant::now() + timeout;
    let exit_code = loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("wait-Fehler: {e}"))? {
            break status.code();
        }
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };

    let stdout = truncate_output(&stdout_handle.join().unwrap_or_default());
    let stderr = truncate_output(&stderr_handle.join().unwrap_or_default());
    Ok(RunOut {
        exit_code,
        stdout,
        stderr,
    })
}

/// Nicht-Unix-Fallback der Live-Variante: kein echtes Zwischenfeedback, die
/// komplette Ausgabe wird erst am Ende gemeldet.
#[cfg(not(unix))]
pub(super) fn run_with_timeout_live(
    cmd: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    live: Option<&mut dyn FnMut(&str)>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<RunOut, String> {
    let out = run_with_timeout(cmd, args, cwd, timeout, cancel)?;
    if let Some(cb) = live {
        if !out.stdout.is_empty() {
            cb(&out.stdout);
        }
        if !out.stderr.is_empty() {
            cb(&out.stderr);
        }
    }
    Ok(out)
}

/// Begrenzt übermäßig lange Ausgabe (z. B. bei Werkzeug-Spam). Abgeschnitten
/// wird von VORN und immer an ganzen Zeilen: alles bis zum Schluss bleibt
/// erhalten, die Lücke ist mit „…“ markiert. Das Ende (Fehler/Ergebnis) geht
/// dadurch nie verloren. Der Algorithmus liegt geteilt in `crate::text`.
pub(super) fn truncate_output(s: &str) -> String {
    const CAP: usize = 64 * 1024;
    crate::text::truncate_tail(s, CAP, "…\n[vorne gekürzt]\n")
}

/// Ermittelt die numerische Identität des Host-Aufrufers (`id -u` / `id -g`).
/// Run-Container laufen als diese UID/GID im Container (`--userns=keep-id`).
pub(crate) fn host_uid_gid() -> Result<(u32, u32), String> {
    let uid = run_with_timeout(
        "id",
        &["-u".into()],
        Path::new("."),
        Duration::from_secs(10),
    )
    .and_then(|out| parse_id(&out, "id -u"))?;
    let gid = run_with_timeout(
        "id",
        &["-g".into()],
        Path::new("."),
        Duration::from_secs(10),
    )
    .and_then(|out| parse_id(&out, "id -g"))?;
    Ok((uid, gid))
}

/// Liest eine numerische UID/GID aus der Ausgabe von `id`.
pub(super) fn parse_id(out: &RunOut, what: &str) -> Result<u32, String> {
    if out.exit_code != Some(0) {
        return Err(format!("{what} failed:\n{}", out.stderr));
    }
    out.stdout
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("{what} lieferte keine numerische ID: {}", out.stdout.trim()))
}

/// Pflegt Namen in Nur-Wort-Namen für Container-/Worktree-Namen um.
pub(super) fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.starts_with('-') {
        out.remove(0);
    }
    if out.is_empty() {
        out.push_str("channel");
    }
    out
}
