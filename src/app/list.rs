//! Gemeinsame, reine Navigations-Abstraktion für Auswahllisten.
//!
//! Bündelt das, was früher über alle Auswahl-UI verstreut war (Channel-Picker,
//! Modell-Picker, Options-Dialog, Bestätigungsdialoge, die drei Spalten des
//! Channel-Builders): Cursor-Bewegung mit Klemmen ODER Umlauf (Builder-Stil),
//! Seiten-Bewegung, Sprung an den Anfang/das Ende und – für lange Listen –
//! ein Scroll-Offset samt „Follow"-Logik, die den Cursor (auch bei mehrzeilig
//! umgebrochenen Einträgen) sichtbar hält.
//!
//! [`ListNav`] ist die reine Bewegung; [`Selection<T>`] bündelt die konkreten
//! Einträge (`items`) mit ihrer Navigation (`nav`) – so wie die Picker und die
//! Builder-Spalten sie verwenden. Für das Rendern liefert [`ListNav::visible`]
//! einen einfachen (Einzeiler-)Ausschnitt, [`ListNav::visible_rows`] einen
//! umbruchsbewussten (mehrzeilige Einträge) – beide starten am `offset`.
//!
//! Das Modul ist bewusst rein und ohne Rendering-Abhängigkeit (keine
//! `Frame`-Typen, kein Terminal-I/O): alle Operationen sind direkt
//! unit-testbar. Die Eingabe-Tasten kommen als `crossterm`-Event-Enum, das
//! nur die Eingabe abbildet (kein Terminal-Zugriff).

use crossterm::event::{self, KeyCode};

/// Cursor + Scroll-Offset einer Auswahlliste.
///
/// - `cursor` ist der markierte Eintrag (0-basiert).
/// - `offset` ist der erste sichtbare Eintrag; bei langen Listen, die nicht in
///   die verfügbare Höhe passen, blättert der Renderer mit `visible()` über
///   diesen Offset und `follow()` hält den Cursor dabei im Fenster.
/// - `wrap` aktiviert den Builder-Stil: Über die Ränder hinaus dreht die
///   Bewegung um (`% max`) statt zu klemmen. Die Picker klemmen (wrap=false).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListNav {
    len: usize,
    cursor: usize,
    offset: usize,
    wrap: bool,
}

impl ListNav {
    /// Neue Navigation mit `len` Einträgen (klemmen am Rand).
    pub(crate) fn new(len: usize) -> Self {
        Self {
            len,
            cursor: 0,
            offset: 0,
            wrap: false,
        }
    }

    /// Neue Navigation mit Umlauf am Rand (wie die drei Builder-Spalten:
    /// `(idx + 1) % max`). Startet auf dem ersten Eintrag; für einen
    /// vorgegebenen Start-Cursor siehe [`ListNav::with_wrap_at`].
    pub(crate) fn with_wrap_at(len: usize, cursor: usize) -> Self {
        Self {
            len,
            cursor: cursor.min(len.saturating_sub(1)),
            offset: 0,
            wrap: true,
        }
    }

    /// Neue Navigation, deren Cursor zunächst auf `cursor` steht (statt 0) –
    /// für Bestätigungsdialoge, die eine sichere Option vorauswählen
    /// (z. B. „Abbrechen"): `len` ist die Options-Anzahl.
    pub(crate) fn new_at(len: usize, cursor: usize) -> Self {
        Self {
            len,
            cursor: cursor.min(len.saturating_sub(1)),
            offset: 0,
            wrap: false,
        }
    }

    /// Setzt den Cursor direkt (geklemmt auf die Listenlänge) – z. B. wenn
    /// eine externe Liste geändert und danach ein konkreter Eintrag
    /// angewählt werden soll. Der Scroll-Offset korrigiert sich beim
    /// nächsten `follow` (Renderer) automatisch.
    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.len.saturating_sub(1));
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Aktuelle Cursor-Position (0-basiert).
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// Erster sichtbarer Eintrag fürs Scrollen.
    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    /// Index des markierten Eintrags – `None` bei leerer Liste.
    pub(crate) fn selected(&self) -> Option<usize> {
        (self.cursor < self.len).then_some(self.cursor)
    }

    /// Dynamische Listen (z. B. Modell-Refresh, Worktree-Neuauswahl, „add path"
    /// im Builder): Länge anpassen und Cursor (sowie Offset) nötigenfalls
    /// klemmen – statt bei Index-Fehlern zu paniken.
    pub(crate) fn set_len(&mut self, len: usize) {
        self.len = len;
        self.cursor = self.cursor.min(len.saturating_sub(1));
        self.offset = self.offset.min(len.saturating_sub(1));
    }

    /// Eine Zeile nach unten (klemmen ODER umlaufen, je nach `wrap`).
    pub(crate) fn move_down(&mut self) {
        if self.len == 0 {
            return;
        }
        self.cursor = if self.wrap && self.cursor + 1 >= self.len {
            0
        } else {
            (self.cursor + 1).min(self.len - 1)
        };
    }

    /// Eine Zeile nach oben (klemmen ODER umlaufen, je nach `wrap`).
    pub(crate) fn move_up(&mut self) {
        if self.len == 0 {
            return;
        }
        self.cursor = if self.wrap && self.cursor == 0 {
            self.len - 1
        } else {
            self.cursor.saturating_sub(1)
        };
    }

    /// `page` Zeilen nach unten (PgDn; `page` = Fensterhöhe des Aufrufers).
    pub(crate) fn page_down(&mut self, page: usize) {
        if self.len == 0 {
            return;
        }
        let step = page.max(1);
        self.cursor = if self.wrap {
            (self.cursor + step) % self.len
        } else {
            (self.cursor + step).min(self.len - 1)
        };
    }

    /// `page` Zeilen nach oben (PgUp).
    pub(crate) fn page_up(&mut self, page: usize) {
        if self.len == 0 {
            return;
        }
        let step = page.max(1);
        self.cursor = if self.wrap {
            (self.cursor + self.len - (step % self.len)) % self.len
        } else {
            self.cursor.saturating_sub(step)
        };
    }

    /// Sprung an den Anfang der Liste (Home).
    pub(crate) fn move_top(&mut self) {
        self.cursor = 0;
    }

    /// Sprung ans Ende der Liste (End).
    pub(crate) fn move_bottom(&mut self) {
        self.cursor = self.len.saturating_sub(1);
    }

    /// Scroll-Follow: hält den Cursor nach einer Bewegung im sichtbaren Fenster.
    ///
    /// `viewport` ist die verfügbare Höhe (in Zeilen) für die Liste,
    /// `row_of(i)` die (ggf. umgebrochene) Höhe von Eintrag `i` – dadurch bleibt
    /// auch ein mehrzeilig umgebrochener, ausgewählter Eintrag vollständig
    /// sichtbar. Ein einzelner Eintrag, der höher ist als das Fenster selbst,
    /// kann nicht vollständig gezeigt werden (Offset stoppt am Cursor).
    pub(crate) fn follow(&mut self, viewport: u16, row_of: impl Fn(usize) -> u16) {
        if self.len == 0 || viewport == 0 {
            self.offset = 0;
            return;
        }
        // Cursor liegt oberhalb des Fensters → Fenster zurückschieben.
        if self.cursor < self.offset {
            self.offset = self.cursor;
        }
        // Cursor (incl. seiner Umbruchzeilen) liegt unterhalb → Fenster so weit
        // nachziehen, dass die Zeilen `offset..=cursor` ins Fenster passen.
        let last = self.cursor.min(self.len - 1);
        let mut rows: u16 = 0;
        for i in self.offset..=last {
            rows += row_of(i);
        }
        while rows > viewport && self.offset < last {
            rows = rows.saturating_sub(row_of(self.offset));
            self.offset += 1;
        }
    }

    /// Sichtbarer Ausschnitt fürs Rendern: `offset..offset+viewport` (in
    /// Einträgen), begrenzt auf die Listenlänge. Leere Liste/fensterlose
    /// Angabe liefert ein leeres Intervall.
    pub(crate) fn visible(&self, viewport: u16) -> std::ops::Range<usize> {
        if self.len == 0 || viewport == 0 {
            return 0..0;
        }
        let start = self.offset.min(self.len);
        let end = (start + viewport as usize).min(self.len);
        start..end
    }

    /// Sichtbarer Ausschnitt inkl. Umbruch: ab `offset` so viele Einträge,
    /// wie in `viewport` Zeilen passen (Zeilenhöhe je Eintrag über `row_of`).
    /// Anders als [`ListNav::visible`] (das `viewport` als Einträge zählt)
    /// für Listen mit mehrzeiligen (umgebrochenen) Einträgen – z. B. die
    /// Builder-Spalten. Zeigt mindestens einen Eintrag, solange einer existiert.
    pub(crate) fn visible_rows(
        &self,
        viewport: u16,
        row_of: impl Fn(usize) -> u16,
    ) -> std::ops::Range<usize> {
        if self.len == 0 || viewport == 0 {
            return 0..0;
        }
        let start = self.offset.min(self.len);
        let mut rows: u16 = 0;
        let mut end = start;
        while end < self.len && rows + row_of(end).max(1) <= viewport {
            rows += row_of(end).max(1);
            end += 1;
        }
        if end == start && start < self.len {
            end = start + 1;
        }
        start..end
    }

    /// Verarbeitet die gemeinsamen Bewegungstasten einer Auswahlliste:
    /// Pfeil runter/hoch und `j`/`k`, dazu `PgUp`/`PgDn` (um `viewport`),
    /// `Home` und `End`. Wendet die Bewegung an und hält den Cursor über
    /// [`ListNav::follow`] im sichtbaren Fenster.
    ///
    /// Liefert `true`, wenn `key` von der Navigation konsumiert wurde.
    /// `Enter`/`Esc` und Sondertasten (z. B. `r` im Modell-Picker, `Del` im
    /// Kanal-Picker) gehören den Aufrufern, deren Konsequenzen je Dialog
    /// verschieden sind.
    pub(crate) fn handle_move(
        &mut self,
        key: &event::KeyEvent,
        viewport: u16,
        row_of: impl Fn(usize) -> u16,
    ) -> bool {
        let moved = match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_down();
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_up();
                true
            }
            KeyCode::PageDown => {
                self.page_down(viewport as usize);
                true
            }
            KeyCode::PageUp => {
                self.page_up(viewport as usize);
                true
            }
            KeyCode::Home => {
                self.move_top();
                true
            }
            KeyCode::End => {
                self.move_bottom();
                true
            }
            _ => false,
        };
        if moved {
            self.follow(viewport, row_of);
        }
        moved
    }
}

/// Eine konkrete Auswahlliste: Items + gemeinsamer Navigationszustand.
///
/// Bündelt die bisher getrennt gehaltenen `cursor`-Felder samt Item-Listen der
/// Picker (Kanal/Modell), der Options- und Bestätigungsdialoge sowie (später)
/// der drei Builder-Spalten. `nav` steuert Cursor + Scroll, `items` hält die
/// konkreten Einträge.
#[derive(Debug, Clone)]
pub(crate) struct Selection<T> {
    pub items: Vec<T>,
    pub nav: ListNav,
}

impl<T> Selection<T> {
    /// Neue Auswahl, deren Cursor zunächst auf `cursor` steht (statt 0) –
    /// z. B. beim Kanal-Picker auf den gebundenen Kanal der Session.
    pub(crate) fn new_at(items: Vec<T>, cursor: usize) -> Self {
        let len = items.len();
        Self {
            items,
            nav: ListNav::new_at(len, cursor),
        }
    }

    /// Neue Auswahl mit Umlauf am Rand (alternierend) und Start-Cursor –
    /// für die drei Builder-Spalten, die am Rand umlaufen statt zu klemmen.
    pub(crate) fn wrap_at(items: Vec<T>, cursor: usize) -> Self {
        let len = items.len();
        Self {
            items,
            nav: ListNav::with_wrap_at(len, cursor),
        }
    }

    /// Hängt einen Eintrag an und hält die Navigationslänge synchron
    /// (z. B. neu eingegebener Host-Pfad im Builder).
    pub(crate) fn push(&mut self, item: T) {
        self.items.push(item);
        self.nav.set_len(self.items.len());
    }

    /// Markiert einen konkreten Eintrag direkt (geklemmt).
    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.nav.set_cursor(cursor);
    }

    /// Dynamische Listen (z. B. Modell-Refresh): Items ersetzen; der Cursor
    /// bleibt nach Möglichkeit erhalten und wird auf die neue Länge geklemmt.
    pub(crate) fn set_items(&mut self, items: Vec<T>) {
        let len = items.len();
        self.items = items;
        self.nav.set_len(len);
    }

    /// Markierter Eintrag (bei leerer/inkonsistenter Liste `None`).
    pub(crate) fn selected(&self) -> Option<&T> {
        self.nav.selected().map(|i| &self.items[i])
    }

    /// Gemeinsame Bewegungstasten für Einzeiler-Listen (Delegiert an
    /// [`ListNav::handle_move`] mit Zeilenhöhe 1).
    pub(crate) fn handle_move(&mut self, key: &event::KeyEvent, viewport: u16) -> bool {
        self.nav.handle_move(key, viewport, |_| 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Grundzustand ─────────────────────────────────────────────────────────

    #[test]
    fn leere_liste_ist_sicher() {
        let nav = ListNav::new(0);
        assert!(nav.is_empty());
        assert_eq!(nav.len(), 0);
        assert_eq!(nav.cursor(), 0);
        assert_eq!(nav.offset(), 0);
        assert_eq!(nav.selected(), None);

        // Bewegung auf leerer Liste darf nie paniken/verrutschen.
        let mut nav = nav;
        nav.move_down();
        nav.move_up();
        nav.page_down(5);
        nav.page_up(5);
        nav.move_bottom();
        nav.move_top();
        nav.follow(10, |_| 1);
        assert_eq!(nav.visible(10), 0..0);
        assert_eq!(nav.cursor(), 0);
        assert_eq!(nav.offset(), 0);
    }

    #[test]
    fn selected_zeigt_den_markierten_eintrag() {
        assert_eq!(ListNav::new(3).selected(), Some(0));
        let mut nav = ListNav::new(3);
        nav.move_down();
        nav.move_down();
        assert_eq!(nav.selected(), Some(2));
    }

    #[test]
    fn new_at_startet_am_vorgegebenen_cursor_und_klemmt() {
        let nav = ListNav::new_at(3, 1);
        assert_eq!(nav.len(), 3);
        assert_eq!(nav.cursor(), 1, "vorgewählter Cursor (sichere Option)");
        // Zu groß gewählter Start-Cursor wird auf das letzte Element geklemmt.
        let nav = ListNav::new_at(3, 99);
        assert_eq!(nav.cursor(), 2);
        // Leere Liste: Cursor 0 bleibt 0.
        let nav = ListNav::new_at(0, 0);
        assert_eq!(nav.cursor(), 0);
    }

    // ── Bewegung (klemmen) ───────────────────────────────────────────────────

    #[test]
    fn move_down_klemmt_am_ende() {
        let mut nav = ListNav::new(5);
        for _ in 0..5 {
            nav.move_down();
        }
        assert_eq!(nav.cursor(), 4);
        nav.move_down();
        assert_eq!(nav.cursor(), 4, "am Ende klemmt die Bewegung");
    }

    #[test]
    fn move_up_klemmt_am_anfang() {
        let mut nav = ListNav::new(5);
        for _ in 0..3 {
            nav.move_down();
        }
        assert_eq!(nav.cursor(), 3);
        for _ in 0..5 {
            nav.move_up();
        }
        assert_eq!(nav.cursor(), 0, "am Anfang klemmt die Bewegung");
    }

    // ── Bewegung (Umlauf, Builder-Stil) ──────────────────────────────────────

    #[test]
    fn wrap_dreht_am_rand_um() {
        let mut nav = ListNav::with_wrap_at(3, 0);
        assert_eq!(nav.cursor(), 0);
        nav.move_down(); // 0 → 1
        nav.move_down(); // 1 → 2
        assert_eq!(nav.cursor(), 2);
        nav.move_down(); // 2 → 0 (Umlauf)
        assert_eq!(nav.cursor(), 0);
        nav.move_up(); // 0 → 2 (Umlauf zurück)
        assert_eq!(nav.cursor(), 2);
    }

    #[test]
    fn wrap_mit_single_eintrag_bleibt_stabil() {
        let mut nav = ListNav::with_wrap_at(1, 0);
        nav.move_down();
        nav.move_down();
        assert_eq!(nav.cursor(), 0);
        nav.move_up();
        assert_eq!(nav.cursor(), 0);
    }

    // ── Seiten-Bewegung und Sprünge ──────────────────────────────────────────

    #[test]
    fn page_down_page_up_klemmen() {
        let mut nav = ListNav::new(10);
        assert_eq!(nav.cursor(), 0);
        nav.page_down(3);
        assert_eq!(nav.cursor(), 3);
        nav.page_down(100);
        assert_eq!(nav.cursor(), 9, "Seite über das Ende hinaus klemmen");
        nav.page_up(2);
        assert_eq!(nav.cursor(), 7);
        nav.page_up(100);
        assert_eq!(nav.cursor(), 0, "Seite über den Anfang hinaus klemmen");
        nav.page_down(0);
        assert_eq!(nav.cursor(), 1, "page 0 bewegt mindestens einen Schritt");
    }

    #[test]
    fn page_dreht_beim_wrap_um() {
        let mut nav = ListNav::with_wrap_at(5, 0);
        nav.page_down(6);
        assert_eq!(nav.cursor(), 1, "6 mod 5 = 1");
        nav.page_up(6);
        assert_eq!(nav.cursor(), 0, "1 - 6 mod 5 = 0");
        nav.page_up(1);
        assert_eq!(nav.cursor(), 4, "Umlauf zurück an das Ende");
    }

    #[test]
    fn to_top_und_to_bottom() {
        let mut nav = ListNav::new(10);
        nav.page_down(5);
        assert_eq!(nav.cursor(), 5);
        nav.move_bottom();
        assert_eq!(nav.cursor(), 9);
        nav.move_top();
        assert_eq!(nav.cursor(), 0);
    }

    // ── set_len (dynamische Listen) ──────────────────────────────────────────

    #[test]
    fn set_len_klemmt_cursor_und_offset() {
        let mut nav = ListNav::new(5);
        nav.page_down(4);
        assert_eq!(nav.cursor(), 4);
        nav.offset = 3;
        nav.set_len(2);
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.cursor(), 1, "Cursor klemmt auf die neue Länge");
        assert_eq!(nav.offset(), 1, "Offset klemmt auf die neue Länge");

        nav.set_len(0);
        assert_eq!(nav.len(), 0);
        assert_eq!(nav.cursor(), 0);
        assert_eq!(nav.offset(), 0);
    }

    // ── Scroll-Follow (einzeilig) ────────────────────────────────────────────

    #[test]
    fn follow_haelt_einzeiligen_cursor_im_fenster() {
        let mut nav = ListNav::new(20);
        nav.page_down(15);
        assert_eq!(nav.cursor(), 15);
        // Fenster der Höhe 10 ohne Follow: Cursor (Zeile 15) liegt dahinter.
        assert_eq!(nav.offset(), 0);
        nav.follow(10, |_| 1);
        assert_eq!(nav.offset(), 6, "offset = cursor+1-viewport = 15+1-10");
        assert!(nav.cursor() >= nav.offset());
        assert!(nav.cursor() < nav.offset() + 10);
    }

    #[test]
    fn follow_laesst_offset_bei_passendem_cursor_unangetastet() {
        let mut nav = ListNav::new(20);
        nav.page_down(5);
        assert_eq!(nav.cursor(), 5);
        nav.follow(10, |_| 1);
        assert_eq!(
            nav.offset(),
            0,
            "Cursor passt ins Fenster – kein Scroll nötig"
        );
    }

    #[test]
    fn follow_schiebt_fenster_bei_cursor_oberhalb_zurueck() {
        let mut nav = ListNav::new(20);
        nav.offset = 10;
        nav.move_top();
        nav.follow(10, |_| 1);
        assert_eq!(nav.offset(), 0, "Fenster folgt dem Cursor nach oben");
    }

    // ── Scroll-Follow (mehrzeilige Einträge) ─────────────────────────────────

    #[test]
    fn follow_haelt_mehrzeiligen_eintrag_komplett_sichtbar() {
        // Eintrag 2 ist 4 Zeilen hoch; Fensterhöhe 5.
        let row_of = |i: usize| -> u16 {
            if i == 2 {
                4
            } else {
                1
            }
        };
        let mut nav = ListNav::new(10);
        nav.page_down(2);
        assert_eq!(nav.cursor(), 2);
        nav.follow(5, row_of);
        // Zeilen für offset..=cursor müssen ≤ 5 sein:
        // offset=0 → 1+1+4 = 6 > 5 → also offset=1 → 1+4 = 5 ✓
        assert_eq!(nav.offset(), 1);
    }

    #[test]
    fn follow_einziger_eintrag_hoeher_als_fenster_bleibt_am_cursor() {
        // Ein einzelner Eintrag (Index 2) ist höher als das Fenster:
        // Offset kann nicht mehr schieben, bleibt am Cursor stehen.
        let row_of = |i: usize| -> u16 {
            if i == 2 {
                8
            } else {
                1
            }
        };
        let mut nav = ListNav::new(10);
        nav.offset = 0;
        nav.page_down(2);
        nav.follow(5, row_of);
        assert_eq!(nav.offset(), 2, "Offset stoppt am Cursor, nicht darüber");
        assert_eq!(nav.cursor(), 2);
    }

    #[test]
    fn follow_mit_leerer_liste_oder_null_fenster_setzt_offset_null() {
        let mut nav = ListNav::new(0);
        nav.offset = 3;
        nav.follow(10, |_| 1);
        assert_eq!(nav.offset(), 0);

        let mut nav2 = ListNav::new(5);
        nav2.offset = 3;
        nav2.page_down(4);
        nav2.follow(0, |_| 1);
        assert_eq!(
            nav2.offset(),
            0,
            "kein Fenster → Scroll-Offset zurückgesetzt"
        );
    }

    // ── visible (sichtbarer Ausschnitt) ──────────────────────────────────────

    #[test]
    fn visible_liefert_den_fensterausschnitt() {
        let mut nav = ListNav::new(20);
        nav.offset = 5;
        assert_eq!(nav.visible(3), 5..8);
        nav.offset = 18;
        assert_eq!(nav.visible(3), 18..20, "Fenster klemmt an der Listenlänge");
        nav.offset = 10;
        assert_eq!(nav.visible(100), 10..20, "großes Fenster zeigt den Rest");
    }

    #[test]
    fn visible_bei_leerer_liste_und_null_fenster() {
        assert_eq!(ListNav::new(0).visible(5), 0..0);
        let nav = ListNav::new(5);
        assert_eq!(nav.visible(0), 0..0);
    }

    // ── handle_move (gemeinsame Bewegungstasten) ─────────────────────────────

    fn key(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent::new(code, event::KeyModifiers::NONE)
    }

    #[test]
    fn handle_move_erkennt_pfeile_und_jk() {
        let mut nav = ListNav::new(5);
        // Pfeil runter und `j` bewegen identisch nach unten.
        assert!(nav.handle_move(&key(KeyCode::Down), 10, |_| 1));
        assert_eq!(nav.cursor(), 1);
        assert!(nav.handle_move(&key(KeyCode::Char('j')), 10, |_| 1));
        assert_eq!(nav.cursor(), 2);
        // Pfeil hoch und `k` bewegen identisch nach oben.
        assert!(nav.handle_move(&key(KeyCode::Up), 10, |_| 1));
        assert_eq!(nav.cursor(), 1);
        assert!(nav.handle_move(&key(KeyCode::Char('k')), 10, |_| 1));
        assert_eq!(nav.cursor(), 0);
        // Am Rand klemmen die Bewegungstasten.
        assert!(nav.handle_move(&key(KeyCode::Up), 10, |_| 1));
        assert_eq!(nav.cursor(), 0);
    }

    #[test]
    fn handle_move_ignoriert_fremde_tasten() {
        let mut nav = ListNav::new(5);
        nav.page_down(3);
        assert_eq!(nav.cursor(), 3);
        // Enter/Esc/Sondertasten gehören den Aufrufern – die Navigation
        // konsumiert sie nicht und verändert den Cursor nicht.
        for code in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Char('r'),
            KeyCode::Char(' '),
        ] {
            assert!(!nav.handle_move(&key(code), 10, |_| 1));
        }
        assert_eq!(nav.cursor(), 3, "Cursor bleibt bei fremden Tasten");
    }

    #[test]
    fn handle_move_unterstuetzt_seiten_und_sprünge() {
        let mut nav = ListNav::new(20);
        assert!(nav.handle_move(&key(KeyCode::PageDown), 10, |_| 1));
        assert_eq!(nav.cursor(), 10, "PgDn springt um die Fensterhöhe");
        assert!(nav.handle_move(&key(KeyCode::End), 10, |_| 1));
        assert_eq!(nav.cursor(), 19);
        assert!(nav.handle_move(&key(KeyCode::PageUp), 10, |_| 1));
        assert_eq!(nav.cursor(), 9, "PgUp springt zurück um die Fensterhöhe");
        assert!(nav.handle_move(&key(KeyCode::Home), 10, |_| 1));
        assert_eq!(nav.cursor(), 0);
    }

    #[test]
    fn handle_move_laesst_cursor_im_fenster_und_scrolled_nach() {
        // Nach einem PgDn weit hinter das Fenster hält `follow` den Cursor
        // sichtbar – der Offset zieht nach.
        let mut nav = ListNav::new(30);
        nav.handle_move(&key(KeyCode::End), 10, |_| 1);
        // Cursor 29, Fenster 10 → Offset 20, damit 29 sichtbar bleibt.
        assert_eq!(nav.offset(), 20);
        assert!(nav.cursor() < nav.offset() + 10);
    }

    // ── Selection<T> (Items + Navigation) ────────────────────────────────────

    #[test]
    fn selection_navigation_und_selected() {
        let mut sel = Selection::new_at(vec![10, 20, 30], 0);
        assert_eq!(sel.selected(), Some(&10));
        assert!(sel.handle_move(&key(KeyCode::Down), 3));
        assert_eq!(sel.selected(), Some(&20));
        assert!(sel.handle_move(&key(KeyCode::Char('j')), 3));
        assert_eq!(sel.selected(), Some(&30));
        // Fremde Taste bewegt nicht.
        assert!(!sel.handle_move(&key(KeyCode::Char('r')), 3));
        assert_eq!(sel.nav.cursor(), 2);
    }

    #[test]
    fn selection_new_at_und_set_items_klemmen_cursor() {
        let mut sel = Selection::new_at(vec![10, 20, 30], 2);
        assert_eq!(sel.nav.cursor(), 2);

        // Schrumpfen: Cursor klemmt auf die neue Länge.
        sel.set_items(vec![100]);
        assert_eq!(sel.nav.cursor(), 0);
        assert_eq!(sel.selected(), Some(&100));

        // Leer: keine Auswahl.
        sel.set_items(vec![]);
        assert_eq!(sel.selected(), None);
    }

    #[test]
    fn wrap_at_dreht_um_und_startet_am_cursor() {
        let mut sel = Selection::wrap_at(vec![10, 20, 30], 2);
        assert_eq!(sel.nav.cursor(), 2);
        assert!(sel.nav.wrap, "Builder-Spalten laufen um");
        sel.handle_move(&key(KeyCode::Down), 3); // 2 → 0 (Umlauf)
        assert_eq!(sel.selected(), Some(&10));
        sel.handle_move(&key(KeyCode::Up), 3); // 0 → 2 (Umlauf zurück)
        assert_eq!(sel.selected(), Some(&30));
    }

    #[test]
    fn push_haelt_nav_sync_und_set_cursor_klemmt() {
        let mut sel = Selection::wrap_at(vec![10, 20], 1);
        sel.push(30);
        assert_eq!(sel.nav.len(), 3, "push verlängert die Navigation");
        assert_eq!(sel.selected(), Some(&20), "Cursor bleibt wo er war");
        sel.set_cursor(99);
        assert_eq!(
            sel.nav.cursor(),
            2,
            "set_cursor klemmt auf das letzte Element"
        );
        assert_eq!(sel.selected(), Some(&30));
    }

    // ── visible_rows (umbruchsbewusster Ausschnitt) ───────────────────────────

    #[test]
    fn visible_rows_zählt_zeilen_statt_einträge() {
        // Zwei Einträge: einer ist doppelt so hoch (Zeilenhöhe 2).
        let row_of = |i: usize| -> u16 {
            if i == 1 {
                2
            } else {
                1
            }
        };
        let mut nav = ListNav::new(4);
        nav.set_cursor(3); // Cursor auf dem letzten Eintrag
        nav.follow(3, row_of);
        // offset so, dass Eintrag 3 (Höhe 1) + 2 (Höhe 1) passen: Fenster 3..
        let range = nav.visible_rows(3, row_of);
        assert!(
            range == (2..4) || range == (1..4),
            "passt bei Höhe 3 nur 2–3 Einträge, war {range:?}"
        );
    }

    #[test]
    fn visible_rows_zeigt_mindestens_einen_eintrag() {
        // Ein Eintrag ist höher als das Fenster – trotzdem mindestens einer.
        let mut nav = ListNav::new(5);
        let row_of = |i: usize| -> u16 {
            if i == 0 {
                99
            } else {
                1
            }
        };
        nav.follow(3, row_of); // follow stoppt am Cursor (Eintrag 0)
        assert_eq!(nav.offset(), 0);
        let range = nav.visible_rows(3, row_of);
        assert_eq!(range, 0..1, "zeigt den (überhohen) Eintrag 0 trotzdem");
    }
}
