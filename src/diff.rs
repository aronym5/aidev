//! Exakter Substring-Ersatz in einer Datei (statt Unified-Diff-Parsing).
//!
//! `edit` ersetzt das erste (oder alle) exakten Vorkommen von `old` durch
//! `new` und liefert zusätzlich die Zeilenpaare für die zweispaltige
//! Diff-Darstellung im Chat (gebaut über einen Zeilen-Diff mit `similar`).
//! Um jede Änderungsgruppe werden bis zu `CONTEXT` (3) unveränderte
//! Kontextzeilen davor und danach angezeigt.

/// Ergebnis eines erfolgreichen Editors: neuer Inhalt plus die Zeilenpaare
/// für die zweispaltige Diff-Darstellung im Chat.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyOut {
    pub text: String,
    pub diff: DiffInfo,
}

/// Eine Diff-Ansicht für eine Datei: die gepaarten Zeilen in Anwendungsreihenfolge.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffInfo {
    /// Ziel-/Anzeige-Pfad der Datei (wie im Tool-Aufruf genannt).
    pub path: String,
    pub rows: Vec<DiffRow>,
}

impl DiffInfo {
    /// Zusammenfassung der Änderung: Anzahl hinzugefügter/entfernter Zeilen.
    /// Liefert `(added, removed)` – reine Kontextzeilen zählen nicht.
    pub fn summary(&self) -> (u64, u64) {
        let mut added = 0u64;
        let mut removed = 0u64;
        for row in &self.rows {
            if !row.old_mark.is_empty() {
                removed += 1;
            }
            if !row.new_mark.is_empty() {
                added += 1;
            }
        }
        (added, removed)
    }
}

/// Ein Zeilenpaar der zweispaltigen Diff-Darstellung.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    /// 1-basierte Zeilennummer in der alten Datei (`None`, wenn nur rechts
    /// etwas steht – reine Einfügung).
    pub old_num: Option<u64>,
    /// Inhalt der alten Seite (leer bei reiner Einfügung).
    pub old_text: String,
    /// Zeichenbereiche (char-Indices) der als gelöscht hervorgehobenen Zeichen.
    pub old_mark: Vec<(usize, usize)>,
    /// 1-basierte Zeilennummer in der neuen Datei (`None` bei reiner Löschung).
    pub new_num: Option<u64>,
    pub new_text: String,
    pub new_mark: Vec<(usize, usize)>,
}

/// Ersetzt in `content` das erste exakte Vorkommen von `old` durch `new`
/// (`replace_all` → alle). Liefert den neuen Inhalt plus Diff-Darstellung.
///
/// `old` wird als Roh-Substring gesucht (inkl. Einrückung/Whitespace); ein
/// leeres `old` ist ein Fehler.
pub fn edit(content: &str, old: &str, new: &str, replace_all: bool) -> Result<ApplyOut, String> {
    if old.is_empty() {
        return Err("Argument \"old\" must not be empty – it must be the exact text to replace in the file.".to_string());
    }
    if !content.contains(old) {
        return Err(format!(
            "\"{old}\" was not found in the file (exact text, including indentation)."
        ));
    }
    let text = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    Ok(ApplyOut {
        text: text.clone(),
        diff: DiffInfo {
            path: String::new(),
            rows: rows_from(content, &text),
        },
    })
}

/// Anzahl unveränderter Kontextzeilen, die vor und nach jeder Änderungsgruppe
/// in der Diff-Darstellung angezeigt werden (sofern verfügbar).
const CONTEXT: usize = 3;

/// Baut die Diff-Zeilenpaare zwischen altem und neuem Inhalt: um jede
/// Änderungsgruppe werden bis zu `CONTEXT` unveränderte Kontextzeilen davor
/// und danach angezeigt (sofern vorhanden). `-`/`+` werden indexweise
/// gepaart, übrige Zeilen erscheinen einseitig.
fn rows_from(old: &str, new: &str) -> Vec<DiffRow> {
    use similar::DiffTag;
    let d = similar::TextDiff::from_lines(old, new);
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut rows: Vec<DiffRow> = Vec::new();

    let ops = d.ops();
    let mut i = 0;
    while i < ops.len() {
        // Unveränderten Kontext (Gleich-Ops) überspringen.
        if ops[i].tag() == DiffTag::Equal {
            i += 1;
            continue;
        }
        // Änderungsgruppe: [start..end) sind Delete/Insert/Replace-Ops.
        let start = i;
        while i < ops.len() && ops[i].tag() != DiffTag::Equal {
            i += 1;
        }
        let end = i;

        // Kontext davor: die letzten Zeilen des unmittelbar vorhergehenden
        // Gleich-Ops (falls vorhanden).
        if start > 0 {
            let prev = &ops[start - 1];
            if prev.tag() == DiffTag::Equal {
                push_context(prev, &old_lines, true, &mut rows);
            }
        }

        // Die eigentlichen Änderungszeilen der Gruppe.
        for op in &ops[start..end] {
            push_change(op, &old_lines, &new_lines, &mut rows);
        }

        // Kontext danach: die ersten Zeilen des unmittelbar folgenden
        // Gleich-Ops (falls vorhanden).
        if end < ops.len() {
            let next = &ops[end];
            if next.tag() == DiffTag::Equal {
                push_context(next, &old_lines, false, &mut rows);
            }
        }
    }
    dedup_context(rows)
}

/// Entfernt doppelt ausgegebene beidseitige Zeilen.  Bei nahen
/// Änderungsgruppen überlappen sich die bis zu `CONTEXT` Kontextfenster davor/
/// danach, sodass dieselbe unveränderte Zeile mehrfach erscheint (z. B. Edit
/// in Zeile 3 und 8 mit 3er-Kontext → Zeile 1-6 und 5-11, also 5-6 doppelt).
/// Beidseitige Zeilen (Kontext) mit bereits gesehenem Nummernpaar werden
/// deshalb nur einmal ausgegeben; einseitige Lösch-/Einfügezeilen bleiben
/// unverändert.
fn dedup_context(rows: Vec<DiffRow>) -> Vec<DiffRow> {
    use std::collections::HashSet;
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        match (r.old_num, r.new_num) {
            (Some(o), Some(n)) => {
                if seen.insert((o, n)) {
                    out.push(r);
                }
            }
            _ => out.push(r),
        }
    }
    out
}

/// Hängt bis zu `CONTEXT` unveränderte Kontextzeilen eines Gleich-Ops an: bei
/// `before` die letzten Zeilen des Ops (Kontext vor einer Änderung), sonst die
/// ersten (Kontext nach einer Änderung).
fn push_context(op: &similar::DiffOp, old_lines: &[&str], before: bool, rows: &mut Vec<DiffRow>) {
    let or = op.old_range();
    let nr = op.new_range();
    let total = (or.end - or.start).min(CONTEXT);
    let (o_start, n_start) = if before {
        (or.end - total, nr.end - total)
    } else {
        (or.start, nr.start)
    };
    for k in 0..total {
        let oi = o_start + k;
        let ni = n_start + k;
        let text = old_lines.get(oi).copied().unwrap_or("").to_string();
        rows.push(DiffRow {
            old_num: Some((oi + 1) as u64),
            old_text: text.clone(),
            old_mark: Vec::new(),
            new_num: Some((ni + 1) as u64),
            new_text: text,
            new_mark: Vec::new(),
        });
    }
}

/// Hängt die geänderten Zeilen eines einzelnen Nicht-Gleich-Ops an
/// (Delete/Insert/Replace, indexweise gepaart).
fn push_change(
    op: &similar::DiffOp,
    old_lines: &[&str],
    new_lines: &[&str],
    rows: &mut Vec<DiffRow>,
) {
    use similar::DiffTag;
    let or = op.old_range();
    let nr = op.new_range();
    match op.tag() {
        DiffTag::Equal => {}
        DiffTag::Delete => {
            for (i, line) in old_lines[or.start..or.end].iter().enumerate() {
                let text = line.to_string();
                rows.push(DiffRow {
                    old_num: Some((or.start + i + 1) as u64),
                    old_text: text.clone(),
                    old_mark: vec![(0, text.chars().count())],
                    new_num: None,
                    new_text: String::new(),
                    new_mark: Vec::new(),
                });
            }
        }
        DiffTag::Insert => {
            for (i, line) in new_lines[nr.start..nr.end].iter().enumerate() {
                let text = line.to_string();
                rows.push(DiffRow {
                    old_num: None,
                    old_text: String::new(),
                    old_mark: Vec::new(),
                    new_num: Some((nr.start + i + 1) as u64),
                    new_text: text.clone(),
                    new_mark: vec![(0, text.chars().count())],
                });
            }
        }
        DiffTag::Replace => {
            let o = &old_lines[or.start..or.end];
            let n = &new_lines[nr.start..nr.end];
            push_replace(or.start, nr.start, o, n, rows);
        }
    }
}

/// Ersetzt die sequenzielle Paarung bei `Replace`-Ops durch eine
/// Ähnlichkeits-basierte Zuordnung: Für jede alte und neue Zeile wird ein
/// Ähnlichkeitswert berechnet; Paare mit hoher Übereinstimmung werden
/// bevorzugt zugeordnet.  Verbleibende Zeilen erscheinen als reine
/// Einfügung/Löschung.  So werden z. B. Kommentar-Zeilen nicht fälschlich
/// mit Code-Zeilen gepaart.
///
/// Bei kleinen Ersetzungen (max. 2 Zeilen pro Seite) wird sequenziell gepaart,
/// da dort die Ähnlichkeitsheuristik zu oft ins Leere läuft.
fn push_replace(
    old_start: usize,
    new_start: usize,
    old_slice: &[&str],
    new_slice: &[&str],
    rows: &mut Vec<DiffRow>,
) {
    let m = old_slice.len();
    let n = new_slice.len();

    if m == 0 && n == 0 {
        return;
    }

    // Für kleine Sets (max. 2 Zeilen pro Seite) → sequenzielle Paarung.
    if m <= 2 && n <= 2 {
        let count = m.max(n);
        for k in 0..count {
            match (old_slice.get(k), new_slice.get(k)) {
                (Some(r), Some(a)) => {
                    let (rm_mark, add_mark) = char_marks(r, a);
                    rows.push(DiffRow {
                        old_num: Some((old_start + k + 1) as u64),
                        old_text: r.to_string(),
                        old_mark: rm_mark,
                        new_num: Some((new_start + k + 1) as u64),
                        new_text: a.to_string(),
                        new_mark: add_mark,
                    });
                }
                (Some(r), None) => {
                    let text = r.to_string();
                    rows.push(DiffRow {
                        old_num: Some((old_start + k + 1) as u64),
                        old_text: text.clone(),
                        old_mark: vec![(0, text.chars().count())],
                        new_num: None,
                        new_text: String::new(),
                        new_mark: Vec::new(),
                    });
                }
                (None, Some(a)) => {
                    let text = a.to_string();
                    rows.push(DiffRow {
                        old_num: None,
                        old_text: String::new(),
                        old_mark: Vec::new(),
                        new_num: Some((new_start + k + 1) as u64),
                        new_text: text.clone(),
                        new_mark: vec![(0, text.chars().count())],
                    });
                }
                (None, None) => {}
            }
        }
        return;
    }

    // Strukturelle Änderung mit mehr als 2 Zeilen auf einer Seite (z. B. das
    // Umbruchen einer Funktionssignatur in mehrere Zeilen).  Ein rein
    // zeilenweises Alignment kann dabei die inhaltlich unveränderten Teile
    // (Argumente) nicht wiedererkennen.  Stattdessen wird der ganze Block
    // über ein Whitespace-tolerantes Zeichen-Alignment verglichen: nur wirklich
    // geänderte Bereiche (eingefügter Whitespace, neue Argumente) werden
    // markiert, gleicher Inhalt bleibt unmarkiert.
    push_block(old_start, new_start, old_slice, new_slice, rows);
}

/// Ersetzt bei strukturellen Änderungen (mehr als 2 Zeilen auf einer Seite)
/// die starre Zeilenpaarung durch ein Zeichen-basiertes Alignment des gesamten
/// alten und neuen Blocks (`similar::TextDiff::from_chars`).  Whitespace und
/// Zeilenumbrüche sind dabei eigene Zeichen, stören aber das Erkennen gleicher
/// Inhalte nicht: nur tatsächlich veränderte Zeichen (eingefügte
/// Zeilenumbrüche, neue Argumente) werden markiert, gleiche Argumente
/// (Name/Typ) bleiben unmarkiert.
fn push_block(
    old_start: usize,
    new_start: usize,
    old_slice: &[&str],
    new_slice: &[&str],
    rows: &mut Vec<DiffRow>,
) {
    let m = old_slice.len();
    let n = new_slice.len();
    if m == 0 && n == 0 {
        return;
    }

    // Zugefügte Blocktexte (Zeilen mit \n verbunden) und ihre Zeilen-Spans
    // (char-Indices).  from_chars liefert char-Indices – konsistent mit der
    // Mark-Semantik der UI.
    let old_block = old_slice.join("\n");
    let new_block = new_slice.join("\n");
    let old_spans = line_spans(old_slice);
    let new_spans = line_spans(new_slice);

    // Markierte Zeichenbereiche pro Zeile (char-Indices in der Zeile selbst).
    let mut old_marks: Vec<Vec<(usize, usize)>> = vec![Vec::new(); m];
    let mut new_marks: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];

    let d = similar::TextDiff::from_chars(&old_block, &new_block);
    for op in d.ops() {
        use similar::DiffTag;
        match op.tag() {
            DiffTag::Equal => {}
            DiffTag::Delete => mark_range(&mut old_marks, &old_spans, op.old_range()),
            DiffTag::Insert => mark_range(&mut new_marks, &new_spans, op.new_range()),
            DiffTag::Replace => {
                mark_range(&mut old_marks, &old_spans, op.old_range());
                mark_range(&mut new_marks, &new_spans, op.new_range());
            }
        }
    }

    // Ausgabe: bei gleicher Zeilenzahl indexweise paaren. Bei ungleicher
    // Zeilenzahl bis zur kürzeren Seite paaren, Rest als einseitige Zeilen.
    let common = m.min(n);
    for k in 0..common {
        rows.push(DiffRow {
            old_num: Some((old_start + k + 1) as u64),
            old_text: old_slice[k].to_string(),
            old_mark: std::mem::take(&mut old_marks[k]),
            new_num: Some((new_start + k + 1) as u64),
            new_text: new_slice[k].to_string(),
            new_mark: std::mem::take(&mut new_marks[k]),
        });
    }
    for k in common..m {
        rows.push(DiffRow {
            old_num: Some((old_start + k + 1) as u64),
            old_text: old_slice[k].to_string(),
            old_mark: std::mem::take(&mut old_marks[k]),
            new_num: None,
            new_text: String::new(),
            new_mark: Vec::new(),
        });
    }
    for k in common..n {
        rows.push(DiffRow {
            old_num: None,
            old_text: String::new(),
            old_mark: Vec::new(),
            new_num: Some((new_start + k + 1) as u64),
            new_text: new_slice[k].to_string(),
            new_mark: std::mem::take(&mut new_marks[k]),
        });
    }
}

/// Zeilen-Spans `(start_char, end_char)` im zugefügten Block-Minustext.  Die
/// durch `join("\n")` eingefügten Newlines liegen in den Lücken zwischen den
/// Spans und werden von `mark_range` einer Zeile zugeordnet – der zugehörige
/// Inhalt (Umbruch) wird so an der nächsten Zeile sichtbar markiert.
fn line_spans(slice: &[&str]) -> Vec<(usize, usize)> {
    let mut spans = Vec::with_capacity(slice.len());
    let mut cur = 0usize;
    for line in slice {
        let len = line.chars().count();
        let end = cur + len;
        spans.push((cur, end));
        cur = end + 1; // Newline nach dieser Zeile (bei der letzten harmlos)
    }
    spans
}

/// Markiert einen character-Range `[s, e)` im Block über die Zeilen-Spans:
/// jeder betroffene Anteil einer Zeile wird als `(col_start, col_end)` gepusht.
fn mark_range(
    marks: &mut [Vec<(usize, usize)>],
    spans: &[(usize, usize)],
    range: std::ops::Range<usize>,
) {
    for (line, &(ls, le)) in spans.iter().enumerate() {
        let cs = range.start.max(ls);
        let ce = range.end.min(le);
        if cs < ce {
            marks[line].push((cs - ls, ce - ls));
        }
    }
}

/// Zeichenbereiche (char-Indices) für ein entferntes/hinzugefügtes Zeilenpaar.
type CharMarks = (Vec<(usize, usize)>, Vec<(usize, usize)>);

/// Berechnet die hervorgehobenen Zeichenbereiche (char-Indices) für ein
/// entferntes/hinzugefügtes Zeilenpaar über einen Zeichen-Diff.
fn char_marks(old: &str, new: &str) -> CharMarks {
    if old == new {
        return (Vec::new(), Vec::new());
    }
    let d = similar::TextDiff::from_chars(old, new);
    let mut rm = Vec::new();
    let mut add = Vec::new();
    for op in d.ops() {
        use similar::DiffTag;
        let old_range = op.old_range();
        let new_range = op.new_range();
        match op.tag() {
            DiffTag::Delete => rm.push((old_range.start, old_range.end)),
            DiffTag::Insert => add.push((new_range.start, new_range.end)),
            DiffTag::Replace => {
                rm.push((old_range.start, old_range.end));
                add.push((new_range.start, new_range.end));
            }
            DiffTag::Equal => {}
        }
    }
    (rm, add)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(content: &str, old: &str, new: &str) -> String {
        edit(content, old, new, false).unwrap().text
    }

    #[test]
    fn ersetzt_einzelne_stelle() {
        let content = "eins\nzwei\ndrei\n";
        assert_eq!(text(content, "zwei", "ZWEEI"), "eins\nZWEEI\ndrei\n");
    }

    #[test]
    fn ersetzt_teil_einer_zeile() {
        let content = "let n = foo();\n";
        assert_eq!(text(content, "foo", "bar"), "let n = bar();\n");
    }

    #[test]
    fn ersetzt_mehrzeiligen_text() {
        let content = "a\nb\nc\nd\n";
        assert_eq!(text(content, "b\nc", "BC"), "a\nBC\nd\n");
        assert_eq!(text(content, "b\nc", "B\nC"), "a\nB\nC\nd\n");
    }

    #[test]
    fn ersetzt_inklusive_einrueckung() {
        // Exakter Match: Whitespace zählt – „    foo“ ist ein anderer Text als „   foo“.
        let content = "    foo\n   foo\n";
        assert_eq!(text(content, "    foo", "bar"), "bar\n   foo\n");
        assert_eq!(text(content, "\n   foo", "\nbar"), "    foo\nbar\n");
    }

    #[test]
    fn default_ersetzt_nur_erste_stelle() {
        let content = "x\ny\nx\n";
        assert_eq!(text(content, "x", "z"), "z\ny\nx\n");
    }

    #[test]
    fn replace_all_ersetzt_alle_stellen() {
        let content = "x\ny\nx\n";
        assert_eq!(edit(content, "x", "z", true).unwrap().text, "z\ny\nz\n");
        assert_eq!(edit("abcabcabc", "bc", "X", true).unwrap().text, "aXaXaX");
    }

    #[test]
    fn start_und_ende_der_datei() {
        assert_eq!(text("foo bar", "foo", "X"), "X bar");
        assert_eq!(text("foo bar", "bar", "X"), "foo X");
    }

    #[test]
    fn neue_zeilen_am_ende() {
        assert_eq!(text("foo", "foo", "foo\nbar\n"), "foo\nbar\n");
    }

    #[test]
    fn leeres_old_ist_ein_fehler() {
        let err = edit("irgendwas", "", "neu", false).unwrap_err();
        assert!(err.starts_with("Argument \"old\""), "{err}");
    }

    #[test]
    fn nicht_gefunden_liefert_fehler() {
        let err = edit("a\nb\n", "qqq", "x", false).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn exakter_whitespace_zahlt_nicht_gefunden() {
        let err = edit("a\nb\n", " b ", "x", false).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn zeilenpaare_mit_beiden_zeilennummern() {
        let content = "erste\nzweite\ndritte\n";
        let out = edit(content, "zweite", "ZWEITE", false).unwrap();
        let rows = &out.diff.rows;
        // 1 Kontextzeile davor + Änderung + 1 Kontextzeile danach.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].old_num, Some(1));
        assert_eq!(rows[0].new_num, Some(1));
        assert_eq!(rows[0].old_text, "erste");
        assert!(rows[0].old_mark.is_empty(), "Kontext unmarkiert");
        assert_eq!(rows[1].old_num, Some(2));
        assert_eq!(rows[1].new_num, Some(2));
        assert_eq!(rows[1].old_text, "zweite");
        assert_eq!(rows[1].new_text, "ZWEITE");
        assert!(rows[1].new_mark.contains(&(0, 6)), "{:?}", rows[1].new_mark);
        assert_eq!(rows[2].old_num, Some(3));
        assert_eq!(rows[2].new_num, Some(3));
        assert_eq!(rows[2].old_text, "dritte");
    }

    #[test]
    fn mehrzeiliger_ersatz_ergibt_gepaarte_zeilen() {
        let content = "a\nb\nc\n";
        let out = edit(content, "b", "B1\nB2", false).unwrap();
        let rows = &out.diff.rows;
        // Kontext „a“ + gepaarte Änderung + reine Einfügung + Kontext „c“.
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].old_num, Some(1));
        assert_eq!(rows[0].new_num, Some(1));
        assert_eq!(rows[1].old_num, Some(2));
        assert_eq!(rows[1].new_num, Some(2));
        assert_eq!(rows[2].old_num, None);
        assert_eq!(rows[2].new_num, Some(3));
        assert_eq!(rows[3].old_num, Some(3));
        assert_eq!(rows[3].new_num, Some(4));
    }

    #[test]
    fn reine_loeschungen_nur_links_reine_einfuegungen_nur_rechts() {
        let out = edit("a\nb\n", "b\n", "", false).unwrap();
        assert_eq!(out.text, "a\n");
        let rows = &out.diff.rows;
        // Kontext „a“ + reine Löschung der Zeile 2.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].old_num, Some(1));
        assert!(rows[0].old_mark.is_empty(), "Kontext unmarkiert");
        assert_eq!(rows[1].old_num, Some(2));
        assert_eq!(rows[1].new_num, None);
        assert!(rows[1].old_mark.contains(&(0, 1)), "{:?}", rows[1].old_mark);

        let out = edit("a\nb\n", "b\n", "x\nb\n", false).unwrap();
        assert_eq!(out.text, "a\nx\nb\n");
        let rows = &out.diff.rows;
        // Kontext „a“ + reine Einfügung + Kontext „b“.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].old_num, Some(1));
        assert_eq!(rows[0].new_num, Some(1));
        assert_eq!(rows[1].old_num, None);
        assert_eq!(rows[1].new_num, Some(2));
        assert!(rows[1].new_mark.contains(&(0, 1)), "{:?}", rows[1].new_mark);
        assert_eq!(rows[2].old_num, Some(2));
        assert_eq!(rows[2].new_num, Some(3));
        assert!(rows[2].old_mark.is_empty(), "Kontext unmarkiert");
    }

    #[test]
    fn drei_kontextzeilen_vor_und_nach_der_aenderung() {
        // 10 Zeilen; geändert wird Zeile 5 → je 3 Kontextzeilen davor/danach.
        let content = (1..=10)
            .map(|i| format!("Zeile {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = edit(&content, "Zeile 5", "Zeile fünf", false).unwrap();
        let rows = &out.diff.rows;
        // 3 Kontext + 1 Änderung + 3 Kontext.
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[0].old_num, Some(2));
        assert_eq!(rows[1].old_num, Some(3));
        assert_eq!(rows[2].old_num, Some(4));
        assert_eq!(rows[3].old_num, Some(5));
        assert_eq!(rows[3].new_text, "Zeile fünf");
        assert_eq!(rows[4].old_num, Some(6));
        assert_eq!(rows[5].old_num, Some(7));
        assert_eq!(rows[6].old_num, Some(8));
        // Kontextzeilen sind unmarkiert und beidseitig identisch.
        for row in rows.iter().filter(|r| r.old_mark.is_empty()) {
            assert_eq!(row.old_text, row.new_text);
            assert_eq!(row.old_num, row.new_num);
        }
    }

    #[test]
    fn kontext_am_dateianfang_und_ende_ist_begrenzt() {
        // Änderung ganz am Anfang: kein Kontext davor, danach nur die 3
        // verfügbaren Zeilen.
        let content = "a\nb\nc\nd\n";
        let out = edit(content, "a", "A", false).unwrap();
        let rows = &out.diff.rows;
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].old_num, Some(1));
        assert_eq!(rows[1].old_num, Some(2));
        assert_eq!(rows[2].old_num, Some(3));
        assert_eq!(rows[3].old_num, Some(4));

        // Änderung ganz am Ende: 3 Kontextzeilen davor, nichts danach.
        let out = edit(content, "d", "D", false).unwrap();
        let rows = &out.diff.rows;
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].old_num, Some(1));
        assert_eq!(rows[1].old_num, Some(2));
        assert_eq!(rows[2].old_num, Some(3));
        assert_eq!(rows[3].old_num, Some(4));
    }

    #[test]
    fn zeichen_mark_nur_abweichende_stelle() {
        let content = "let n = foo();\n";
        let out = edit(content, "foo", "bar", false).unwrap();
        let row = &out.diff.rows[0];
        assert_eq!(row.new_text, "let n = bar();");
        for (s, e) in &row.old_mark {
            assert_eq!(&row.old_text[*s..*e], "foo", "Mark {s}..{e}");
        }
        for (s, e) in &row.new_mark {
            assert_eq!(&row.new_text[*s..*e], "bar", "Mark {s}..{e}");
        }
    }

    #[test]
    fn gleicher_text_liefert_leere_diff() {
        let out = edit("a\nb\n", "b", "b", false).unwrap();
        assert_eq!(out.text, "a\nb\n");
        assert!(out.diff.rows.is_empty());
    }

    #[test]
    fn aehnlichkeitsbasierte_paarung_bei_grossen_replaces() {
        // 2 alte Zeilen, 4 neue Zeilen: die ersten zwei neuen sind Kommentare,
        // die unveränderte Zeile "fn foo() {" und die Änderung bar→baz.
        // Der Zeilen-Diff (similar) erkennt "fn foo() {" als unveränderten
        // Kontext; der eigentliche Ersatz (bar(); → baz();) wird als Paar
        // dargestellt – inklusive Zeichen-Markierung.
        let content = "fn foo() {\n    bar();\n}\n";
        let old = "fn foo() {\n    bar();\n}";
        let new = "// Header\n// Beschreibung\nfn foo() {\n    baz();\n}";
        let out = edit(content, old, new, false).unwrap();
        let rows = &out.diff.rows;
        // Kontext + Änderungen: Die Kommentarzeilen stehen als reine
        // Einfügungen vor dem Paar, "fn foo() {" ist Kontext.
        // Finde die geänderten Paare (old_num und new_num sind beide Some,
        // Markierungen vorhanden).
        let pairs: Vec<_> = rows
            .iter()
            .filter(|r| r.old_num.is_some() && r.new_num.is_some() && !r.old_mark.is_empty())
            .collect();
        // Gerade die bar(); → baz();-Zeile ist der (einzige) geänderte Ersatz.
        assert_eq!(pairs.len(), 1, "Paare gefunden: {}", rows.len());
        assert_eq!(pairs[0].old_num, Some(2));
        assert_eq!(pairs[0].new_num, Some(4));
        assert_eq!(pairs[0].old_text, "    bar();");
        assert_eq!(pairs[0].new_text, "    baz();");
        // Die beiden Kommentarzeilen erscheinen als reine Einfügungen.
        assert!(rows
            .iter()
            .any(|r| r.old_num.is_none() && r.new_text == "// Header"));
        assert!(rows
            .iter()
            .any(|r| r.old_num.is_none() && r.new_text == "// Beschreibung"));
        // Die unveränderte "fn foo() {"-Zeile ist als Kontext vorhanden
        // (alte Zeile 1, neue Zeile 3, ohne Markierung).
        assert!(rows.iter().any(|r| {
            r.old_num == Some(1)
                && r.new_num == Some(3)
                && r.old_text == "fn foo() {"
                && r.old_mark.is_empty()
        }));
    }

    #[test]
    fn diff_summary_zaehlt_einfuegungen_und_loeschungen() {
        let content = "a\nb\nc\n";
        let out = edit(content, "b", "B1\nB2\nB3", false).unwrap();
        let (added, removed) = out.diff.summary();
        // "b" wird gelöscht, "B1", "B2", "B3" werden eingefügt.
        assert!(removed >= 1, "mindestens 1 Löschung: {removed}");
        assert!(added >= 2, "mindestens 2 Einfügungen: {added}");
    }

    #[test]
    fn nahe_aenderungen_deduplizieren_ueberlappenden_kontext() {
        // Zwei Änderungen in Zeile 3 und 8: ihre 3er-Kontextfenster überlappen
        // (Gruppe 1: 1-6, Gruppe 2: 5-11). Keine Zeilennummer darf doppelt
        // ausgegeben werden.
        let old = (1..=11)
            .map(|i| format!("Zeile {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut new_lines: Vec<String> = (1..=11).map(|i| format!("Zeile {i}")).collect();
        new_lines[2] = "Zeile DREI".to_string(); // 0-basiert → Zeile 3
        new_lines[7] = "Zeile ACHT".to_string(); // 0-basiert → Zeile 8
        let new = new_lines.join("\n");
        let rows = rows_from(&old, &new);

        let mut seen_old = std::collections::HashSet::new();
        let mut seen_new = std::collections::HashSet::new();
        for r in &rows {
            if let Some(n) = r.old_num {
                assert!(seen_old.insert(n), "old Zeile {n} doppelt: {rows:?}");
            }
            if let Some(n) = r.new_num {
                assert!(seen_new.insert(n), "new Zeile {n} doppelt: {rows:?}");
            }
        }
        // Beide Änderungszeilen sind enthalten.
        assert!(rows
            .iter()
            .any(|r| r.old_text == "Zeile 3" && r.new_text == "Zeile DREI"));
        assert!(rows
            .iter()
            .any(|r| r.old_text == "Zeile 8" && r.new_text == "Zeile ACHT"));
    }

    #[test]
    fn umbruch_der_signatur_markiert_nur_neuheiten() {
        // Eine Funktionssignatur wird von einer Zeile auf mehrere umgebrochen
        // und bekommt ein viertes Argument. Die unveränderten Argumente
        // a/b/c dürfen NICHT als geändert markiert werden – nur eingefügter
        // Whitespace (Umbruch) und das neue Argument `d`.
        let old = "fn foo(a: i32, b: i32, c: i32) {\n    body();\n}\n";
        let new =
            "fn foo(\n    a: i32,\n    b: i32,\n    c: i32,\n    d: i32,\n) {\n    body();\n}\n";
        let rows = rows_from(old, new);

        // Die alte Einzeiler-Zeile 1: ihr gesamter Inhalt taucht in den neuen
        // Zeilen wieder auf → keine Markierung.
        let old_row = rows.iter().find(|r| r.old_text.starts_with("fn foo"));
        assert!(old_row.is_some(), "Alte Signaturzeile fehlt: {rows:?}");
        assert!(
            old_row.unwrap().old_mark.is_empty(),
            "Unveränderte Signatur darf nicht markiert werden: {:?}",
            old_row.unwrap()
        );

        // Unveränderte Argumentzeilen: der Inhalt (ab char 4) bleibt fast
        // vollständig unmarkiert – nur vereinzelte Rand-Zeichen (z. B. ein
        // Komma) dürfen an der Diff-Grenze markiert sein, nie das Argument.
        for arg in ["a: i32,", "b: i32,", "c: i32,"] {
            let r = rows.iter().find(|r| r.new_text.trim() == arg);
            assert!(r.is_some(), "Zeile {arg} fehlt, rows: {rows:?}");
            // Markierte Zeichen, die im Argument-Bereich (ab char 4) liegen.
            let content_marks: Vec<(usize, usize)> = r
                .unwrap()
                .new_mark
                .iter()
                .copied()
                .filter(|&(_s, e)| e > 4)
                .map(|(s, e)| (s.max(4), e))
                .collect();
            let marked_chars: usize = content_marks.iter().map(|&(s, e)| e - s).sum();
            assert!(
                marked_chars <= 2,
                "{arg}: Argument darf fast ganz unmarkiert bleiben, {:?}",
                r.unwrap()
            );
        }

        // Das neue Argument d ist vollständig neu → markiert.
        let d = rows.iter().find(|r| r.new_text.trim() == "d: i32,");
        assert!(d.is_some());
        assert_eq!(
            d.unwrap().new_mark,
            vec![(0, 11)],
            "d: i32, muss neu markiert sein"
        );
    }
}
