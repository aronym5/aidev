use crate::channel::ChannelKind;

/// Berechtigung, mit der das Modell in einem Turn auf die Kanal-Werkzeuge
/// zugreifen darf. Aufeinander aufbauend: `Execute` darf alles, `Write` auch
/// schreiben/editieren, `Read` nur lesen/suchen/abrufen. Der User wählt die
/// Berechtigung vor dem Absenden (Tab wechselt), `/run` ist davon unabhängig.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Permission {
    /// Nur `grep`, `read`, `glob`, `webfetch` – schwächste Stufe (Default beim
    /// Local-Kanal).
    #[default]
    Read,
    /// Zusätzlich `write` und `edit`.
    Write,
    /// Zusätzlich `run` – volle Werkzeugmenge (Default beim Podman-Kanal).
    Execute,
}

impl Permission {
    /// Werkzeuge, die das Modell unter dieser Berechtigung aufrufen darf.
    pub fn tools(self) -> &'static [&'static str] {
        match self {
            Permission::Read => &["grep", "read", "glob", "webfetch"],
            Permission::Write => &["grep", "read", "glob", "webfetch", "write", "edit"],
            Permission::Execute => &["grep", "read", "glob", "webfetch", "write", "edit", "run"],
        }
    }

    /// Darf dieses Werkzeug mit dieser Berechtigung ausgeführt werden?
    pub fn allows(self, tool: &str) -> bool {
        self.tools().contains(&tool)
    }

    /// Mindest-Berechtigung, die ein Werkzeug benötigt.
    pub fn required_for(tool: &str) -> Permission {
        match tool {
            "run" => Permission::Execute,
            "write" | "edit" => Permission::Write,
            _ => Permission::Read,
        }
    }

    /// Die nächste Stufe im Tab-Zyklus (read → write → execute → read).
    pub fn next(self) -> Permission {
        match self {
            Permission::Read => Permission::Write,
            Permission::Write => Permission::Execute,
            Permission::Execute => Permission::Read,
        }
    }

    /// Kanalabhängiger Startwert: Podman-Kanäle beginnen direkt mit `execute`,
    /// der lokale Kanal vorsichtig mit `read`.
    pub fn default_for(kind: ChannelKind) -> Permission {
        match kind {
            ChannelKind::Local => Permission::Read,
            ChannelKind::PodmanAttach | ChannelKind::PodmanRun => Permission::Execute,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Write => "write",
            Permission::Execute => "exec",
        }
    }

    /// Kurze verbale Beschreibung für den Prompt.
    pub fn description(self) -> &'static str {
        match self {
            Permission::Read => "read-only/search",
            Permission::Write => "+ write",
            Permission::Execute => "+ execute",
        }
    }
}
