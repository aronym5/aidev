//! Modell-Werkzeug `webfetch`: eine URL abrufen und als Text zurückgeben.
//!
//! Bewusst **host-seitig** statt über den Kanal: Die Kanal-Abstraktion ist
//! die FS/Shell-Sandbox, Netzwerk ist orthogonal – und Podman-Images haben
//! selten einen HTTP-Client. Der geteilte reqwest-Client (rustls,
//! System-Proxy) aus [`super::http`] wird wiederverwendet.
//!
//! Sicherheitsprofil (absichtlich schlank):
//! - Nur `http`/`https`; schemelose URLs bekommen `https://` vorangestellt,
//!   `http://` wird auf `https://` angehoben.
//! - Keine IP-/Host-Filterung (bewusste Entscheidung): webfetch liegt auf
//!   Berechtigungsstufe `read`, wo der User es bewusst freigibt.
//! - Decken: 30 s pro Abruf, 1 MB Rohbody; der konvertierte Text wird vom
//!   Aufrufer zusätzlich auf das übliche Tool-Ergebnislimit gekürzt.

use std::io::Read;
use std::time::Duration;

use super::http::shared_client;
use super::tools_def::USER_AGENT;

/// Maximale Rohbody-Größe in Bytes (über dieser Grenze wird abgebrochen und
/// markiert – schützt vor Endlos-Streams und Riesen-Downloads).
const BODY_CAP: usize = 1024 * 1024;
/// Timeout für einen einzelnen Abruf.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Ergebnis eines erfolgreichen Abrufs (Status 2xx).
#[derive(Debug, Clone)]
pub(crate) struct FetchOut {
    pub(crate) status: u16,
    /// Finale URL nach Redirects.
    pub(crate) final_url: String,
    /// Bei HTML: zu lesbarem Text gewandelt; sonst der Rohbody (UTF-8, mit
    /// Verlustzeichen-Ersatz).
    pub(crate) text: String,
    /// Gelesene Rohbytes (vor Konversion).
    pub(crate) bytes: u64,
    /// True, wenn der Body an [`BODY_CAP`] gekürzt wurde.
    pub(crate) truncated: bool,
}

/// Normalisiert eine Modelleingabe zu einer abrufbaren HTTPS-URL:
/// schemelose Eingaben erhalten `https://`, `http://` wird angehoben,
/// andere Schemata werden abgewiesen.
pub(crate) fn normalize_url(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("URL is empty.".to_string());
    }
    // Ein „:“ VOR dem ersten „/“ ist ein Schemata-Trenner (javascript:,
    // file:, ftp: …) – nur http(s) darf durch; schemelose Hosts bekommen
    // https:// vorangestellt.
    if let Some(colon) = s.find(':') {
        if !s[..colon].contains('/') {
            match s[..colon].to_ascii_lowercase().as_str() {
                "http" | "https" => {}
                other => {
                    return Err(format!(
                        "Unsupported scheme \"{other}:\" - only http/https."
                    ))
                }
            }
        }
    }
    if let Some((scheme, rest)) = s.split_once("://") {
        match scheme.to_ascii_lowercase().as_str() {
            "https" => Ok(format!("https://{rest}")),
            "http" => Ok(format!("https://{rest}")),
            other => Err(format!(
                "Unsupported scheme \"{other}:\" - only http/https."
            )),
        }
    } else {
        Ok(format!("https://{s}"))
    }
}

/// Ruft die URL ab und liefert Status, finale URL und Text.
/// Nicht-2xx-Antworten sind ein Fehler (mit Statuszeile im Text).
pub(crate) fn fetch(raw_url: &str) -> Result<FetchOut, String> {
    let url = normalize_url(raw_url)?;

    let resp = shared_client()
        .get(&url)
        .timeout(FETCH_TIMEOUT)
        .header("User-Agent", USER_AGENT)
        .send()
        .map_err(|e| format!("Fetch failed: {}", reqwest_one_line(&e)))?;

    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    if !resp.status().is_success() {
        return Err(format!("HTTP {status} from {final_url}"));
    }

    // Rohbody mit Decke lesen; über dem Limit brechen wir ab und merken es.
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    resp.take(BODY_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Read failed: {e}"))?;
    if bytes.len() > BODY_CAP {
        bytes.truncate(BODY_CAP);
        truncated = true;
    }
    let byte_count = bytes.len() as u64;

    let is_html = bytes
        .get(..16.min(bytes.len()))
        .map(|head| {
            head.to_ascii_lowercase()
                .windows(4)
                .any(|w| w == b"<htm" || w == b"<!do")
        })
        .unwrap_or(false);
    let raw = String::from_utf8_lossy(&bytes).to_string();
    let text = if is_html { html_to_text(&raw) } else { raw };

    Ok(FetchOut {
        status,
        final_url,
        text,
        bytes: byte_count,
        truncated,
    })
}

/// Kürzt eine reqwest-Fehlerkette auf das Wesentliche (einzeilig).
fn reqwest_one_line(err: &reqwest::Error) -> String {
    let msg = err.to_string();
    msg.lines().next().unwrap_or("unknown error").to_string()
}

/// Wandelt HTML in lesbaren Text: Skripte/Stile/Kommentare fliegen raus,
/// Block-Tags erzeugen Zeilenumbrüche, Überschriften Markdown-„#“,
/// Listeneinträge „- “, Tabellenzellen werden mit „ | “ getrennt,
/// Entities werden dekodiert, Leerraum zusammengeklappt.
pub(crate) fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0usize;
    let mut skip_until_close: Option<&str> = None; // script/style

    while i < html.len() {
        if let Some(tag) = skip_until_close {
            // Bis zum schließenden </tag> überspringen.
            let close = format!("</{tag}");
            match lower[i..].find(&close) {
                Some(pos) => {
                    i += pos;
                    skip_until_close = None;
                    continue;
                }
                None => break, // nie geschlossen → alles weg
            }
        }
        match html[i..].find('<') {
            None => {
                out.push_str(&html[i..]);
                break;
            }
            Some(rel) if rel > 0 => {
                out.push_str(&html[i..i + rel]);
                i += rel;
            }
            _ => {}
        }
        // html[i] == '<': Tag bzw. Kommentar lesen.
        if lower[i..].starts_with("<!--") {
            match lower[i..].find("-->") {
                Some(end) => i += end + 3,
                None => break,
            }
            continue;
        }
        let Some(gt_rel) = lower[i..].find('>') else {
            break;
        };
        let tag_raw = &lower[i + 1..i + gt_rel];
        i += gt_rel + 1;
        let tag_body = tag_raw.trim();
        if tag_body.starts_with('/') || tag_body.ends_with('/') {
            // Schließende/selbstschließende Tags: Struktur-Marker je Typ.
            let name = tag_body.trim_start_matches('/').trim_end_matches('/');
            out.push_str(close_marker(name));
            continue;
        }
        let name: String = tag_body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        match name.as_str() {
            "script" | "style" => {
                skip_until_close = Some(match name.as_str() {
                    "script" => "script",
                    _ => "style",
                });
            }
            "br" => out.push('\n'),
            "hr" => out.push_str("\n---\n"),
            "li" => out.push_str("\n- "),
            n if n.len() == 2 && n.starts_with('h') && n.as_bytes()[1].is_ascii_digit() => {
                let level = n.as_bytes()[1] - b'0';
                out.push_str(&format!("\n{} ", "#".repeat(level as usize)));
            }
            "td" | "th" => out.push_str(" | "),
            "tr" | "p" | "div" | "table" | "blockquote" | "pre" | "ul" | "ol" => {
                out.push('\n');
            }
            _ => {}
        }
    }

    let decoded = decode_entities(&out);
    collapse_blank_lines(&decoded)
}

/// Struktur-Umbruch beim SCHLIESSENDEN Tag (Absätze enden als Zeile).
fn close_marker(name: &str) -> &'static str {
    match name {
        "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "tr" | "blockquote"
        | "pre" | "title" => "\n",
        _ => "",
    }
}

/// Dekodiert die gängigen Entities inkl. numerischer Formen; `&amp;` zuletzt,
/// damit keine Doppel-Dekodierung entsteht.
fn decode_entities(s: &str) -> String {
    // &#NN; und &#xHH; in einem Durchgang.
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("&#") {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos + 2..];
        let consumed = match tail.find(';') {
            Some(e) => match decode_codepoint(&tail[..e]) {
                Some(c) => {
                    out.push(c);
                    e + 1
                }
                None => {
                    out.push_str("&#");
                    2
                }
            },
            None => {
                out.push_str("&#");
                2
            }
        };
        rest = &tail[consumed..];
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Eine numerische Entity-Basis (`65` dezimal, `x42` hex) in ein Zeichen –
/// NUL wird bewusst nicht erzeugt.
fn decode_codepoint(body: &str) -> Option<char> {
    let parse = |s: &str| -> Option<char> {
        s.parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .filter(|c| *c != '\0')
    };
    if let Some(hex) = body.strip_prefix('x').or_else(|| body.strip_prefix('X')) {
        u32::from_str_radix(hex, 16)
            .ok()
            .and_then(char::from_u32)
            .filter(|c| *c != '\0')
    } else {
        parse(body)
    }
}

/// Mehr als zwei aufeinanderfolgende Newlines → genau zwei.
fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut runs = 0usize;
    for ch in s.chars() {
        if ch == '\n' {
            runs += 1;
            if runs <= 2 {
                out.push(ch);
            }
        } else {
            runs = 0;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_werden_normalisiert() {
        assert_eq!(
            normalize_url("example.com/a").unwrap(),
            "https://example.com/a"
        );
        assert_eq!(
            normalize_url("http://example.com").unwrap(),
            "https://example.com"
        );
        assert_eq!(normalize_url(" https://x.y ").unwrap(), "https://x.y");
        assert!(normalize_url("ftp://f.x").is_err());
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert!(normalize_url("javascript:alert(1)").is_err());
        assert!(normalize_url("").is_err());
    }

    #[test]
    fn html_wird_zu_lesbarem_text() {
        let src = r#"<html><head><style>p{color:red}</style></head>
<body><!-- Kommentar --><h1>Titel</h1><p>Erster &amp; zweiter &lt;Tag&gt;</p>
<ul><li>Eins</li><li>Zwei</li></ul><table><tr><td>a</td><td>b</td></tr></table>
<script>var x = "<p>kein markup</p>";</script><br>Weiter</body></html>"#;
        let t = html_to_text(src);
        assert!(t.contains("# Titel"), "{t}");
        assert!(t.contains("Erster & zweiter <Tag>"), "{t}");
        assert!(t.contains("- Eins"), "{t}");
        assert!(t.contains("a | b"), "{t}");
        assert!(!t.contains("color:red"), "{t}");
        assert!(!t.contains("kein markup"), "{t}");
        assert!(t.contains("Weiter"), "{t}");
    }

    #[test]
    fn numerische_entities_werken_dekodiert() {
        assert_eq!(decode_entities("&#65;&#x42;&amp;#66;"), "AB&#66;");
    }

    #[test]
    fn leerraum_wird_geklappt() {
        assert_eq!(collapse_blank_lines("a\n\n\n\nb"), "a\n\nb");
    }
}
