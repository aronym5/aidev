//! Tool-Ausführung auf dem Kanal: run_tool_live, ToolOut, tool_activity.

use std::path::Path;

use serde_json::Value;

use super::helpers::truncate;
use super::tools_def::{
    ToolInvocation, GLOB_RESULT_CAP, GREP_FILE_CAP, MAX_GREP_CONTEXT, MAX_READ_LINES,
    MAX_READ_LINE_CHARS, RUN_CONSOLE_CAP, TOOL_RESULT_CAP,
};
use super::{ReadInfo, RunInfo, ToolActivity};
use crate::channel::{Channel, RunOut};
use serde_json::Map;

/// Führt einen Tool-Call über den Kanal aus und liefert das Ergebnis an das
/// Modell plus die Anzeige-Details. Fehler (unbekanntes Tool, ungültige
/// Argumente, Kanal-Fehler, Exit-Code ≠ 0) werden als `FEHLER: …` an das
/// Modell zurückgegeben, damit es reagieren kann.
#[derive(Default)]
pub(crate) struct ToolOut {
    /// Text, der dem Modell als `tool`-Nachricht zurückgegeben wird.
    pub(crate) text: String,
    /// Abgesetzte Konsolen-Box für `run`-Aufrufe mit Ausgabe.
    pub(crate) run: Option<RunInfo>,
    /// Diff-Daten für `edit`-Aufrufe (zweispaltige Chat-Anzeige).
    pub(crate) diff: Option<crate::diff::DiffInfo>,
    /// Bei einer Fenster-Lesung (`read` ohne ganze Datei): die gelesenen
    /// Zeilennummern – der Worker hängt sie an den Log-Einzeiler.
    pub(crate) read: Option<ReadInfo>,
}

/// Führt ein Werkzeug aus und reicht die Ausgabe eines `run`-Werkzeugs bereits
/// während des Laufs (stdout+stderr, Ankunftsreihenfolge) über `live` durch –
/// für die Live-Vorschau der Konsolen-Box in der UI. Ein gesetztes `cancel`
/// (z. B. `Esc`) bricht einen laufenden `run` **sofort** ab (Prozessgruppe wird
/// beendet); die anderen Datei-Werkzeuge laufen zu schnell, um sinnvoll
/// unterbrechbar zu sein.
pub(crate) fn run_tool_live(
    name: &str,
    args: &str,
    ch: &dyn Channel,
    live: &mut dyn FnMut(&str),
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> ToolOut {
    let v: Value = if args.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(args).unwrap_or_else(|_| Value::Object(Map::new()))
    };

    let result: Result<ToolOut, String> = (|| match name {
        "grep" => {
            let pattern = arg_str(&v, "pattern")?;
            let path = arg_str_opt(&v, "path").unwrap_or_else(|| ".".to_string());
            let include = arg_str_opt(&v, "include");
            let content = v
                .get("content")
                .and_then(|x| x.as_i64())
                .unwrap_or(1)
                .clamp(0, MAX_GREP_CONTEXT as i64) as usize;
            let result = ch.grep(&pattern, Path::new(&path), include.as_deref(), content)?;
            let mut text = String::new();
            if let Some(note) = &result.note {
                text.push_str(note);
                text.push('\n');
            }
            if content > 0 {
                // Roh-Passthrough: Kontextblöcke von rg/grep inkl. „--“-Gruppen.
                match &result.raw {
                    Some(raw) if !raw.trim().is_empty() => {
                        text.push_str(raw.trim_end());
                        text.push('\n');
                    }
                    _ => text.push_str("No matches.\n"),
                }
            } else {
                // Zwitter aus count und files_with_matches: pro Datei die
                // Anzahl ihrer Funde, in Fundreihenfolge.
                let mut counts: Vec<(&str, usize)> = Vec::new();
                for m in &result.matches {
                    match counts.last_mut() {
                        Some((p, c)) if *p == m.path.as_str() => *c += 1,
                        _ => counts.push((m.path.as_str(), 1)),
                    }
                }
                if counts.is_empty() {
                    text.push_str("No matches.\n");
                } else {
                    for (i, (p, c)) in counts.iter().enumerate() {
                        if i >= GREP_FILE_CAP {
                            text.push_str(&format!(
                                "... {} more files\n",
                                counts.len() - GREP_FILE_CAP
                            ));
                            break;
                        }
                        text.push_str(&format!("{p}:{c}\n"));
                    }
                }
            }
            Ok(ToolOut {
                text: truncate(&text, TOOL_RESULT_CAP),
                ..Default::default()
            })
        }
        "glob" => {
            let pattern = arg_str(&v, "pattern")?;
            let paths = ch.glob(&pattern, Path::new("."))?;
            let mut text = String::new();
            if paths.is_empty() {
                text.push_str("No matches.\n");
            }
            for (i, p) in paths.iter().enumerate() {
                if i >= GLOB_RESULT_CAP {
                    text.push_str(&format!(
                        "... {} more paths\n",
                        paths.len() - GLOB_RESULT_CAP
                    ));
                    break;
                }
                text.push_str(p);
                text.push('\n');
            }
            Ok(ToolOut {
                text: truncate(&text, TOOL_RESULT_CAP),
                ..Default::default()
            })
        }
        "webfetch" => {
            let url = arg_str(&v, "url")?;
            // Der Prompt geht nicht an die Seite – er dokumentiert die
            // Absicht und erscheint über das Label im Verlauf (tool_label).
            arg_str(&v, "prompt")?;
            let out = super::webfetch::fetch(&url)?;
            let mut text = format!(
                "[HTTP {} / {:.1} kB / {}]\n",
                out.status,
                out.bytes as f64 / 1024.0,
                out.final_url
            );
            text.push_str(out.text.trim_end());
            text.push('\n');
            if out.truncated {
                text.push_str("...[Body truncated at size cap]\n");
            }
            Ok(ToolOut {
                text: truncate(&text, TOOL_RESULT_CAP),
                ..Default::default()
            })
        }
        "read" => {
            let path = arg_str(&v, "path")?;
            let content = ch.read(Path::new(&path))?;
            let lines: Vec<&str> = content.lines().collect();
            // Leere Datei: keinen Nummernblock erzeugen, nur den Kopf.
            if lines.is_empty() {
                return Ok(ToolOut {
                    text: format!("// {path}: (empty)\n"),
                    ..Default::default()
                });
            }
            // 1-basierte Adressierung: `offset` ist die Zeilennummer des
            // Fensteranfangs (Standard 1 = Dateianfang), `limit` die Anzahl
            // Zeilen (Standard bis zum Dateiende).
            let start = v
                .get("offset")
                .and_then(|x| x.as_u64())
                .map(|n| n as usize)
                .unwrap_or(1)
                .max(1);
            let limit = v
                .get("limit")
                .and_then(|x| x.as_u64())
                .map(|n| n as usize)
                .unwrap_or(usize::MAX)
                .max(1);
            if start > lines.len() {
                return Err(format!(
                    "offset {start} is outside the file ({} lines).",
                    lines.len()
                ));
            }
            let from = start - 1;
            // Die 2000-Zeilen-Decke: offen angefragte Fenster (auch das volle
            // Lesen ohne limit) werden an dieser Grenze gestoppt; das Modell
            // paginiert dann mit offset/limit weiter. Ein explizit kleines
            // limit bleibt dagegen unangetastet.
            let window = limit.min(MAX_READ_LINES);
            let end = (from + window).min(lines.len());
            let mut text = format!("// {path}: Lines {start}-{end} of {}\n", lines.len());
            // Nummernspalte an der Stellenzahl der Gesamtzahl ausrichten, damit
            // die führenden Nummern sauber untereinander stehen.
            let width = lines.len().to_string().len().max(1);
            for (n, l) in lines[from..end].iter().enumerate() {
                let n = start + n; // 1-basierte Zeilennummer
                let line = if l.chars().count() > MAX_READ_LINE_CHARS {
                    let cut: String = l.chars().take(MAX_READ_LINE_CHARS).collect();
                    format!("{cut}...[line {n} truncated]")
                } else {
                    l.to_string()
                };
                text.push_str(&format!("{n:>width$} | {line}\n"));
            }
            // Wurde das Fenster an der 2000-Zeilen-Decke gestoppt und die Datei
            // geht weiter, den Fortsetzungspunkt nennen – zeilenbasiert,
            // passend zur 1-basierten Adressierung des Tools.
            if window == MAX_READ_LINES && end < lines.len() {
                text.push_str(&format!(
                    "...[continue from line {} - use read with offset/limit]\n",
                    end + 1
                ));
            }
            // Nur bei FENSTER-Lesungen (nicht die ganze Datei) merkt sich das
            // Ergebnis die gelesenen Zeilennummern; der Worker hängt sie an den
            // Log-Einzeiler. Ein komplettes Lesen bleibt ein kompakter
            // Log-Einzeiler ohne Zusatz. Der Dateiinhalt geht nur an das Modell,
            // nicht in die Chat-Anzeige.
            let partial = start > 1 || end < lines.len();
            let read = if partial {
                Some(ReadInfo {
                    range: if start == end {
                        start.to_string()
                    } else {
                        format!("{start}-{end}")
                    },
                })
            } else {
                None
            };
            Ok(ToolOut {
                text,
                read,
                ..Default::default()
            })
        }
        "edit" => {
            let path = arg_str(&v, "path")?;
            let old = arg_str(&v, "old")?;
            let new = arg_str(&v, "new")?;
            let replace_all = v
                .get("replace_all")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            let content = ch.read(Path::new(&path))?;
            let out = crate::diff::edit(&content, &old, &new, replace_all)?;
            ch.write(Path::new(&path), &out.text)?;
            let mut info = out.diff;
            info.path = path.clone();
            Ok(ToolOut {
                text: format!("Changed: {path}"),
                diff: Some(info),
                ..Default::default()
            })
        }
        "write" => {
            let path = arg_str(&v, "path")?;
            let content = arg_str(&v, "content")?;
            ch.write(Path::new(&path), &content)?;
            Ok(ToolOut {
                text: format!("Written: {path}"),
                ..Default::default()
            })
        }
        "run" => {
            // Das Modell liefert den kompletten Shell-Ausdruck als `command` –
            // mit voller Shell-Auswertung (Pipes, Umleitungen, Variablen,
            // Verkettungen, Logik).
            let command = arg_str(&v, "command")?;
            let shell = ch.shell()?;
            // Live-Ausgabe während des Laufs (stdout+stderr) über `live` an die
            // UI durchreichen, damit die Konsolen-Box schon läuft und sich
            // fortschreibt, statt erst am Ende zu erscheinen. `cancel` bricht
            // den laufenden Befehl sofort ab (Esc). Der Ausdruck geht als EIN
            // Argument an `-c` der gewählten Shell (`sh`/`bash`).
            let out = ch.run_live(
                &shell,
                &["-c".to_string(), command.clone()],
                Path::new("."),
                live,
                cancel,
            )?;
            // In der Box/Anzeige erscheint nur der Ausdruck selbst (kein
            // `sh -c …` / `bash -c …`), damit die Zeile wie getippt aussieht.
            let run = run_console(&command, &[], &out, ch.root());
            let text = render_run(&command, &[], &out);
            Ok(ToolOut {
                text,
                run,
                ..Default::default()
            })
        }
        other => Err(format!("unknown tool \"{other}\"")),
    })();

    match result {
        Ok(t) => t,
        Err(err) => ToolOut {
            text: format!("ERROR: {err}"),
            ..Default::default()
        },
    }
}

/// Erzeugt die `ToolActivity`-Abschlussdaten eines beendeten Werkzeugs: den
/// vollen Ergebnistext (für Modell + API-Projektion) und die UI-Render-Zusätze
/// (Konsolen-Box, Diff, read-Range). Label/Status der Anzeige kommen aus dem
/// Chat-Event bzw. dem `ToolStart`-Label, nicht aus diesem Event.
pub(crate) fn tool_activity(result: &ToolOut) -> ToolActivity {
    ToolActivity {
        output_full: result.text.clone(),
        run: result.run.clone(),
        diff: result.diff.clone(),
        read: result.read.clone(),
    }
}

/// Baut die Konsolen-Box für einen `run`-Aufruf – nur wenn es etwas zu zeigen
/// gibt (stdout/stderr vorhanden oder Exit-Code ≠ 0).
fn run_console(cmd: &str, args: &[String], out: &RunOut, cwd: String) -> Option<RunInfo> {
    let mut body = String::new();
    if !out.stdout.is_empty() {
        body.push_str(&out.stdout);
    }
    if !out.stderr.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&out.stderr);
    }
    if body.trim().is_empty() && out.exit_code == Some(0) {
        return None;
    }
    let command = if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{cmd} {}", args.join(" "))
    };
    Some(RunInfo {
        cwd,
        command,
        output: truncate(body.trim_end(), RUN_CONSOLE_CAP),
        exit_code: out.exit_code,
    })
}

fn arg_str(v: &Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("Argument \"{key}\" missing or not a string."))
}

fn arg_str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

fn render_run(cmd: &str, args: &[String], out: &RunOut) -> String {
    let mut s = String::new();
    s.push_str(cmd);
    if !args.is_empty() {
        s.push(' ');
        s.push_str(&args.join(" "));
    }
    match out.exit_code {
        Some(0) => {}
        Some(code) => s.push_str(&format!("\n[exit {code}]")),
        None => s.push_str("\n[timeout/cancelled]"),
    }
    if !out.stdout.is_empty() {
        s.push('\n');
        s.push_str(&out.stdout);
    }
    if !out.stderr.is_empty() {
        s.push('\n');
        s.push_str(&out.stderr);
    }
    trim_end_whitespace(s)
}

fn trim_end_whitespace(s: String) -> String {
    s.trim_end().to_string()
}

/// Kurzbeschreibung eines Tool-Calls für Status/Log (z. B. `run cargo test`).
pub(crate) fn tool_label(t: &ToolInvocation) -> String {
    let v: Value = serde_json::from_str(&t.arguments).unwrap_or(Value::Null);
    let brief = |key: &str| v.get(key).and_then(|x| x.as_str()).map(str::to_string);
    let args: Vec<String> = match t.name.as_str() {
        "grep" | "glob" => brief("pattern").map(|q| vec![q]).unwrap_or_default(),
        "read" | "write" | "edit" => brief("path").map(|p| vec![p]).unwrap_or_default(),
        "run" => brief("command").map(|c| vec![c]).unwrap_or_default(),
        "webfetch" => {
            // Host (+ Pfadanfang) der URL plus gekürzter Prompt – so sieht
            // der User im Verlauf, WOHIN und WOZU abgerufen wurde.
            let url = brief("url").unwrap_or_default();
            let prompt = brief("prompt").unwrap_or_default();
            let without_scheme = url
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(&url)
                .to_string();
            let target: String = without_scheme.chars().take(60).collect();
            let short_prompt: String = prompt.chars().take(60).collect();
            vec![format!("{target} · {short_prompt}")]
        }
        _ => Vec::new(),
    };
    let mut s = t.name.clone();
    if !args.is_empty() {
        s.push(' ');
        s.push_str(&args.join(" "));
    }
    s
}

/// Rohform eines `run`-Aufrufs (der komplette Shell-Ausdruck) für den
/// Bestätigungsdialog – ungekürzt, damit der User den echten Befehl sieht.
pub(crate) fn run_command_display(t: &ToolInvocation) -> String {
    let v: Value = serde_json::from_str(&t.arguments).unwrap_or(Value::Null);
    let parts: Vec<String> = v
        .get("command")
        .and_then(|x| x.as_str())
        .map(|c| vec![c.to_string()])
        .unwrap_or_default();
    if parts.is_empty() {
        "(empty command)".to_string()
    } else {
        parts.join(" ")
    }
}
