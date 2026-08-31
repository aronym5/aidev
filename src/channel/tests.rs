use super::*;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

#[test]
fn truncate_output_haelt_das_ende_und_schneidet_ganze_zeilen_vorne() {
    assert_eq!(run::truncate_output("kurz\nende"), "kurz\nende");
    let lang: String = (1..=8000).map(|i| format!("zeile {i}\n")).collect();
    assert!(lang.chars().count() > 64 * 1024, "Eingabe über dem Limit");
    let out = run::truncate_output(&lang);
    assert!(out.starts_with("…\n[vorne gekürzt]\n"), "Markierung: {out}");
    assert!(out.ends_with("zeile 8000"), "letzte Zeile bleibt: {out}");
    assert!(!out.contains("zeile 1"), "vorne wird abgeschnitten: {out}");
    assert!(
        out.chars().count() <= 64 * 1024,
        "bleibt im Limit: {}",
        out.chars().count()
    );
}

#[test]
fn truncate_output_schneidet_einzelne_riesenzeile_nur_am_ende() {
    let riesig = "x".repeat(100_000);
    let out = run::truncate_output(&riesig);
    assert!(out.starts_with('…'), "{out}");
    assert!(out.chars().count() <= 64 * 1024, "{}", out.chars().count());
    assert!(out.ends_with('x'), "Ende bleibt lesbar");
}

#[test]
fn trim_front_behaelt_den_schwanz_an_zeilengrenzen() {
    let mut s = String::new();
    for i in 0..100_000 {
        s.push_str(&format!("zeile {i}\n"));
    }
    assert!(run::trim_front(&mut s, 4096), "gekürzt");
    assert!(!s.is_empty(), "nicht leer");
    assert!(
        s.trim_end().ends_with("zeile 99999"),
        "Ende bleibt: {}",
        &s[..32.min(s.len())]
    );
    assert!(!s.starts_with("zeile 0"), "vorne weg");
    assert!(
        s.starts_with("zeile "),
        "ganze Zeile vorn: {}",
        &s[..32.min(s.len())]
    );
    assert!(
        s.len() <= 4096 + "zeile 99999\n".len(),
        "nahe am Limit: {}",
        s.len()
    );

    let mut kurz = "a\nb\nc".to_string();
    assert!(!run::trim_front(&mut kurz, 1000));
    assert_eq!(kurz, "a\nb\nc");
}

#[test]
fn resolve_haltet_pfad_im_kanal() {
    let base = std::env::temp_dir().join(format!("aidev-resolve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    assert_eq!(
        resolve::resolve(&base, Path::new("a/b.txt")).unwrap(),
        base.join("a/b.txt")
    );
    assert_eq!(
        resolve::resolve(&base, Path::new("/a")).unwrap(),
        base.join("a")
    );
    assert_eq!(
        resolve::resolve(&base, Path::new("./x")).unwrap(),
        base.join("x")
    );
    assert!(resolve::resolve(&base, Path::new("../x")).is_err());
    assert!(resolve::resolve(&base, Path::new("a/../../x")).is_err());
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn resolve_weist_symlink_escape_ab() {
    use std::os::unix::fs::symlink;
    let base = std::env::temp_dir().join(format!("aidev-sym-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!("aidev-symout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("geheim.txt"), "geheim\n").unwrap();
    std::fs::create_dir_all(base.join("sub")).unwrap();
    symlink(&outside, base.join("evil")).unwrap();
    symlink(outside.join("geheim.txt"), base.join("leak.txt")).unwrap();
    symlink(outside.join("nicht-da"), base.join("kaputt")).unwrap();
    assert!(resolve::resolve(&base, Path::new("evil/geheim.txt")).is_err());
    assert!(resolve::resolve(&base, Path::new("leak.txt")).is_err());
    assert!(resolve::resolve(&base, Path::new("kaputt")).is_err());
    let inner = base.join("ziel.txt");
    std::fs::write(&inner, "ok\n").unwrap();
    symlink(&inner, base.join("intern")).unwrap();
    assert_eq!(
        resolve::resolve(&base, Path::new("intern")).unwrap(),
        base.join("intern")
    );
    assert!(resolve::resolve(&base, Path::new("sub/x")).is_ok());
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn resolve_weist_kaputte_symlink_wurzel_ab() {
    use std::os::unix::fs::symlink;
    let real = std::env::temp_dir().join(format!("aidev-symroot-{}", std::process::id()));
    let base = std::env::temp_dir().join(format!("aidev-symrootl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&real);
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&real).unwrap();
    symlink(real.join("nicht-da"), &base).unwrap();
    assert!(resolve::resolve(&base, Path::new("x.txt")).is_err());
    let _ = std::fs::remove_dir_all(&real);
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn local_dateioperationen_weist_symlink_escape_ab() {
    use std::os::unix::fs::symlink;
    let dir = std::env::temp_dir().join(format!("aidev-symops-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!("aidev-symops-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("geheim.txt"), "geheim\n").unwrap();
    symlink(outside.join("geheim.txt"), dir.join("leak.txt")).unwrap();
    symlink(&outside, dir.join("evil")).unwrap();
    let ch = Local::new(dir.clone());
    assert!(ch.read(Path::new("leak.txt")).is_err());
    assert!(ch.read(Path::new("evil/geheim.txt")).is_err());
    assert!(ch.write(Path::new("leak.txt"), "überschreiben").is_err());
    assert!(ch.list(Path::new("evil")).is_err());
    assert!(ch.grep("geheim", Path::new("evil"), None, 0).is_err());
    ch.write(Path::new("intern.txt"), "ok\n").unwrap();
    assert_eq!(ch.read(Path::new("intern.txt")).unwrap(), "ok\n");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn local_read_write_list_run() {
    let dir = std::env::temp_dir().join(format!("aidev-local-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ch = Local::new(dir.clone());
    ch.write(Path::new("src/hello.txt"), "hallo welt\n")
        .unwrap();
    assert_eq!(ch.read(Path::new("src/hello.txt")).unwrap(), "hallo welt\n");
    let entries = ch.list(Path::new("src")).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "hello.txt");
    let out = ch
        .run(
            "echo",
            &["hallo".into(), "channel".into()],
            Path::new("src"),
        )
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert!(out.stdout.contains("hallo channel"));
    assert!(ch.read(Path::new("../etc/hostname")).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn local_shell_erkennt_bash_fallback_sh_und_wertet_aus() {
    let dir = std::env::temp_dir().join(format!("aidev-shell-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ch = Local::new(dir.clone());
    let shell = ch.shell().expect("Shell-Erkennung läuft durch");
    assert!(shell == "bash" || shell == "sh", "genutzte Shell: {shell}");
    let pipe = ch
        .run(
            &shell,
            &["-c".to_string(), "printf abc | tr a-z A-Z".to_string()],
            Path::new("."),
        )
        .expect("Pipe via Shell");
    assert_eq!(pipe.exit_code, Some(0));
    assert!(pipe.stdout.contains("ABC"), "{}", pipe.stdout);
    let chain = ch
        .run(
            &shell,
            &["-c".to_string(), "echo eins && echo zwei".to_string()],
            Path::new("."),
        )
        .expect("Verkettung via Shell");
    assert!(
        chain.stdout.contains("eins") && chain.stdout.contains("zwei"),
        "{}",
        chain.stdout
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_ls_und_cli_run_mit_local_kanal() {
    let dir = std::env::temp_dir().join(format!("aidev-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (registry, ch) = test_registry("repo", dir.clone(), Some("repo"));
    ch.write(Path::new("src/lib.rs"), "pub fn hallo() {}\n")
        .unwrap();
    ch.write(Path::new("README.md"), "# demo\n").unwrap();
    let listing = cli_ls(&registry, None).unwrap();
    assert!(
        listing.contains("Kanal \u{201E}repo\u{201C}"),
        "Default-Kanal gewählt: {listing}"
    );
    assert!(
        listing.contains("Wurzel: Local:"),
        "Wurzel sichtbar: {listing}"
    );
    assert!(
        listing.contains("[dir] src/"),
        "Verzeichnis markiert: {listing}"
    );
    assert!(listing.contains("README.md"), "Datei gelistet: {listing}");
    assert_eq!(listing, cli_ls(&registry, Some("repo")).unwrap());
    let out = cli_run(&registry, None, "echo", &["hallo".into(), "channel".into()]).unwrap();
    assert!(out.contains("$ echo hallo channel"));
    assert!(
        out.trim_end().ends_with("hallo channel"),
        "Kommando wird nicht doppelt übergeben: {out:?}"
    );
    assert!(cli_ls(&registry, Some("fremd")).is_err());
    let leer = ChannelRegistry {
        default: None,
        map: Default::default(),
        managed: Arc::new(Mutex::new(Vec::new())),
    };
    assert!(cli_ls(&leer, None).is_err(), "ohne Kanal kein Listing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn local_kanal_aus_config_via_channel_from_config() {
    let dir = std::env::temp_dir().join(format!("aidev-cfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cc = ChannelConfig {
        kind: "local".into(),
        image: None,
        container: None,
        run_container: None,
        workdir: "/app".into(),
        host_root: Some(dir.display().to_string()),
        home: None,
    };
    let ch = channel_from_config("sandbox", &cc, 60, Arc::new(Mutex::new(Vec::new())), None, crate::config::PodmanUserMapping::KeepId)
        .expect("local-Kanal aus Config bauen");
    assert_eq!(ch.kind(), ChannelKind::Local);
    assert!(ch.root().starts_with("Local:"));
    ch.write(Path::new("datei.txt"), "inhalt\n").unwrap();
    assert_eq!(ch.read(Path::new("datei.txt")).unwrap(), "inhalt\n");
    assert!(ch.run("echo", &["ok".into()], Path::new(".")).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn local_config_fehlerfaelle() {
    let managed = Arc::new(Mutex::new(Vec::new()));
    let ohne = ChannelConfig {
        kind: "local".into(),
        image: None,
        container: None,
        run_container: None,
        workdir: "/app".into(),
        host_root: None,
        home: None,
    };
    assert!(channel_from_config("x", &ohne, 60, managed.clone(), None, crate::config::PodmanUserMapping::KeepId).is_err());
    let mit_img = ChannelConfig {
        kind: "local".into(),
        image: Some("node:22".into()),
        container: None,
        run_container: None,
        workdir: "/app".into(),
        host_root: Some("/tmp".into()),
        home: None,
    };
    assert!(channel_from_config("x", &mit_img, 60, managed.clone(), None, crate::config::PodmanUserMapping::KeepId).is_err());
    let fremd = ChannelConfig {
        kind: "docker".into(),
        image: None,
        container: None,
        run_container: None,
        workdir: "/app".into(),
        host_root: Some("/tmp".into()),
        home: None,
    };
    assert!(channel_from_config("x", &fremd, 60, managed, None, crate::config::PodmanUserMapping::KeepId).is_err());
}

#[test]
fn cli_ls_und_cli_run_mit_config_basiertem_local() {
    let dir = std::env::temp_dir().join(format!("aidev-cfgcli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn sagt().\n").unwrap();
    let mut channels = HashMap::new();
    channels.insert(
        "sandbox".to_string(),
        ChannelConfig {
            kind: "local".into(),
            image: None,
            container: None,
            run_container: None,
            workdir: "/app".into(),
            host_root: Some(dir.display().to_string()),
            home: None,
        },
    );
    let cfg = crate::config::Config {
        model: "test/m".into(),
        provider: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "test".to_string(),
                crate::config::ProviderConfig {
                    base_url: "x".into(),
                    api_key: Some("x".into()),
                    user_agent: None,
                },
            );
            m
        },
        max_tool_rounds: 16,
        default_channel: Some("sandbox".into()),
        channels,
        symbols: crate::config::SymbolMode::Glyph,
        context_window: 200_000,
        compact_at: 0.8,
        compact_keep_turns: 3,
        compact_summary_tokens: 4_000,
        compact_auto: true,
        mouse: false,
        paths: Default::default(),
        ..crate::config::Config::default()
    };
    let registry = ChannelRegistry::new(&cfg);
    let listing = cli_ls(&registry, None).unwrap();
    assert!(
        listing.contains("Kanal \u{201E}sandbox\u{201C}"),
        "{listing}"
    );
    assert!(listing.contains("[dir] src/"), "{listing}");
    let out = cli_run(&registry, None, "echo", &["hallo".into()]).unwrap();
    assert!(out.contains("hallo"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workdir_join_ohne_escape() {
    assert_eq!(
        resolve::join_workdir("/app", Path::new("src"))
            .unwrap()
            .as_str(),
        "/app/src"
    );
    assert_eq!(
        resolve::join_workdir("/app", Path::new(""))
            .unwrap()
            .as_str(),
        "/app"
    );
    assert_eq!(
        resolve::join_workdir("/app", Path::new("/x/y"))
            .unwrap()
            .as_str(),
        "/app/x/y"
    );
    assert!(resolve::join_workdir("/app", Path::new("../x")).is_err());
}

#[test]
fn sanitize_macht_wortnamen() {
    assert_eq!(run::sanitize("Mein Kanal 2"), "mein-kanal-2");
    assert_eq!(run::sanitize("!!!"), "channel");
}

#[test]
fn parse_search_lines_zerlegt_rg_grep_ausgabe() {
    let out = "src/lib.rs:1:pub fn hallo() {}\nREADME.md:7:# demo\n";
    let m = search::parse_search_lines(out);
    assert_eq!(m.len(), 2);
    assert_eq!(m[0].path, "src/lib.rs");
    assert_eq!(m[0].text, "pub fn hallo() {}");
    assert_eq!(m[1].path, "README.md");
    assert_eq!(m[1].text, "# demo");
}

#[test]
fn grep_fallback_findet_treffer_und_strippt_pfadpraefix() {
    let dir = std::env::temp_dir().join(format!("aidev-grep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn hallo() {}\n").unwrap();
    std::fs::write(dir.join("ignoriert.log"), "ignoriert\n").unwrap();
    let (matches, raw) = search::grep_search("fn hallo", &dir, Duration::from_secs(10), None, 0)
        .expect("grep-Fallback findet Treffer");
    assert!(raw.is_none());
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].path, "src/lib.rs", "kein ./-Präfix");
    assert!(matches[0].text.contains("fn hallo"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grep_fallback_keine_treffer_ist_kein_fehler() {
    let dir = std::env::temp_dir().join(format!("aidev-grepn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "inhalt\n").unwrap();
    let (matches, _) = search::grep_search("gibt-es-nicht", &dir, Duration::from_secs(10), None, 0)
        .expect("keine Treffer ist kein Fehler");
    assert!(matches.is_empty(), "{matches:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn grep_fallback_folgt_keinen_symlinks_nach_aussen() {
    use std::os::unix::fs::symlink;
    let dir = std::env::temp_dir().join(format!("aidev-grepsym-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!("aidev-grepsymout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("geheim.txt"), "NUR-AUSSEN\n").unwrap();
    std::fs::write(dir.join("sub/eigen.txt"), "NUR-INNEN\n").unwrap();
    symlink(&outside, dir.join("sub/evil")).unwrap();
    let (matches, _) = search::grep_search("NUR-AUSSEN", &dir, Duration::from_secs(10), None, 0)
        .expect("Suche läuft");
    assert!(
        matches.is_empty(),
        "kein Treffer außerhalb der Wurzel: {matches:?}"
    );
    let (matches, _) = search::grep_search("NUR-INNEN", &dir, Duration::from_secs(10), None, 0)
        .expect("Suche läuft");
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].path, "sub/eigen.txt");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn is_excluded_path_erkennt_ausgeschlossene_verzeichnisse() {
    assert!(search::is_excluded_path(".git/HEAD"));
    assert!(search::is_excluded_path("target/debug/aidev"));
    assert!(search::is_excluded_path("node_modules/x/index.js"));
    assert!(!search::is_excluded_path("src/lib.rs"));
    assert!(!search::is_excluded_path("targets.md"));
    assert!(!search::is_excluded_path(""));
}

#[test]
fn essential_diff_paths_liest_podman_diff_und_filtert_ueberfluessiges() {
    let diff = "A /app/quell.txt\nC /app\nA /tmp/npm-cache/x\nC /var/tmp\nA /run/secrets/token\nA /opt/tool/version\nM /opt/tool/version\nA /usr/local/bin/hilf\nD /etc/wichtig\nC /root/.cache\n";
    let paths = container::essential_diff_paths(diff, "/app", "/tmp");
    assert_eq!(
        paths,
        vec![
            "etc/wichtig".to_string(),
            "opt/tool/version".to_string(),
            "root/.cache".to_string(),
            "usr/local/bin/hilf".to_string()
        ],
        "{paths:?}"
    );
    let paths = container::essential_diff_paths(diff, "/app", "/home/aidev");
    assert!(paths.contains(&"root/.cache".to_string()), "{paths:?}");
    assert!(
        !paths.contains(&"tmp/npm-cache/x".to_string()),
        "/tmp ist flüchtig: {paths:?}"
    );
    assert!(!paths.contains(&"app/quell.txt".to_string()), "{paths:?}");
    let paths = container::essential_diff_paths(
        "C /tmp/x\nA /app/sub/y\nD /run/p\nA /dev/loop0\nA /root/cache\n",
        "/app",
        "/root",
    );
    assert!(paths.is_empty(), "{paths:?}");
    assert!(container::essential_diff_paths("", "/app", "/tmp").is_empty());
}

#[test]
fn git_porcelain_note_zaehlt_und_nennt_pfad() {
    assert_eq!(container::git_porcelain_note(""), None);
    let out = " M src/lib.rs\n?? README.md\n";
    let note = container::git_porcelain_note(out).expect("uncommittete Änderungen");
    assert!(note.contains("2 uncommitted changes"), "{note}");
    assert!(note.contains("src/lib.rs"), "{note}");
    let note = container::git_porcelain_note("?? neu.txt\n").expect("eine Änderung");
    assert!(note.contains("1 uncommitted change"), "{note}");
    assert!(note.contains("neu.txt"), "{note}");
}

#[test]
fn host_uid_gid_liefert_numerische_identitaet() {
    let (uid, gid) = run::host_uid_gid().expect("id -u/-g verfügbar");
    assert!(uid < 1_000_000, "UID-Plausibilität: {uid}");
    assert!(gid < 1_000_000, "GID-Plausibilität: {gid}");
}

#[test]
fn run_modus_exec_argv_als_host_identitaet() {
    let (uid, gid) = run::host_uid_gid().expect("id verfügbar");
    let ch = PodmanChannel {
        name: "build".into(),
        mode: PodmanMode::Run,
        container: "aidev-build".into(),
        workdir: "/app".into(),
        host_root: Some(PathBuf::from("/tmp/x")),
        image: Some("build:latest".into()),
        timeout: Duration::from_secs(60),
        uid,
        gid,
        home: "/home/aidev".into(),
        usermapping: crate::config::PodmanUserMapping::KeepId,
        seq: AtomicUsize::new(1),
        status: Arc::new(Mutex::new(ChannelStatus::Unknown)),
        worktree: None,
        managed: Arc::new(Mutex::new(Vec::new())),
        shell: Mutex::new(None),
    };
    let argv = ch.exec_argv("/app", "cargo", &["build".into(), "-j2".into()]);
    let user_pos = argv
        .iter()
        .position(|a| a == "--user")
        .expect("--user gesetzt");
    assert_eq!(argv[user_pos + 1], format!("{uid}:{gid}"), "Host-Identität");
    let env_pos = argv
        .iter()
        .position(|a| a == "--env")
        .expect("--env gesetzt");
    assert_eq!(argv[env_pos + 1], "HOME=/home/aidev", "schreibbares $HOME");
}

#[test]
fn attach_modus_exec_argv_ohne_user() {
    let ch = PodmanChannel {
        name: "ci".into(),
        mode: PodmanMode::Attach,
        container: "aidev-ws".into(),
        workdir: "/app".into(),
        host_root: Some(PathBuf::from("/tmp/x")),
        image: None,
        timeout: Duration::from_secs(60),
        uid: 0,
        gid: 0,
        home: String::new(),
        usermapping: crate::config::PodmanUserMapping::KeepId,
        seq: AtomicUsize::new(1),
        status: Arc::new(Mutex::new(ChannelStatus::Unknown)),
        worktree: None,
        managed: Arc::new(Mutex::new(Vec::new())),
        shell: Mutex::new(None),
    };
    let argv = ch.exec_argv("/app", "ls", &[]);
    assert!(
        !argv.contains(&"--user".to_string()),
        "kein --user im Attach-Modus: {argv:?}"
    );
    assert!(
        !argv.contains(&"--env".to_string()),
        "kein HOME-Override: {argv:?}"
    );
}

#[test]
fn run_from_config_loest_host_identitaet_auf() {
    let managed = Arc::new(Mutex::new(Vec::new()));
    let cc = ChannelConfig {
        kind: "podman".into(),
        image: Some("node:22".into()),
        container: None,
        run_container: None,
        workdir: "/app".into(),
        host_root: Some("/tmp/x".into()),
        home: None,
    };
    let ch = podman::podman_from_config("node", &cc, 60, managed, None, crate::config::PodmanUserMapping::KeepId).expect("Run-Kanal bauen");
    let (uid, gid) = run::host_uid_gid().unwrap();
    assert_eq!(ch.uid, uid, "UID == Host");
    assert_eq!(ch.gid, gid, "GID == Host");
    assert_eq!(ch.home, "/tmp", "Default-Home");
}

#[test]
fn parse_inspect_state_liest_running_und_details() {
    assert_eq!(
        container::parse_inspect_state("true|keep-id:keep-gid|[/host/x:/app]|node:22", true),
        container::ContainerState::Running {
            userns: "keep-id:keep-gid".into(),
            binds: "[/host/x:/app]".into(),
            image: "node:22".into(),
        }
    );
    assert_eq!(
        container::parse_inspect_state("false|keep-id:keep-gid|[/host/x:/app]|node:22", true),
        container::ContainerState::Stopped,
    );
    assert_eq!(
        container::parse_inspect_state("", false),
        container::ContainerState::Missing,
    );
}

#[test]
fn reuse_container_nur_bei_passendem_laufenden_container() {
    let bind = "/host/x:/app";
    let running = || container::ContainerState::Running {
        userns: "keep-id:keep-gid".into(),
        binds: "[/host/x:/app]".into(),
        image: "node:22".into(),
    };
    let keep_id = crate::config::PodmanUserMapping::KeepId;
    let uidmap = crate::config::PodmanUserMapping::Uidmap;
    assert!(container::reuse_container(&running(), bind, Some("node:22"), keep_id));
    assert!(!container::reuse_container(
        &container::ContainerState::Running {
            userns: "host".into(),
            binds: "[/host/x:/app]".into(),
            image: "node:22".into(),
        },
        bind,
        Some("node:22"),
        keep_id
    ));
    assert!(!container::reuse_container(
        &container::ContainerState::Stopped,
        bind,
        Some("node:22"),
        keep_id
    ));
    assert!(!container::reuse_container(
        &container::ContainerState::Missing,
        bind,
        Some("node:22"),
        keep_id
    ));
    // uidmap-Modus: ein keep-id-Container ist NICHT wiederverwendbar (anderes
    // Mapping)…
    assert!(!container::reuse_container(&running(), bind, Some("node:22"), uidmap));
    // …aber ein privates (nicht keep-id) Userns passt.
    let private_ns = container::ContainerState::Running {
        userns: "private".into(),
        binds: "[/host/x:/app]".into(),
        image: "node:22".into(),
    };
    assert!(container::reuse_container(&private_ns, bind, Some("node:22"), uidmap));
}

#[test]
fn local_kanal_status_ist_running() {
    let ch = Local::new(PathBuf::from("/tmp/aidev-test"));
    assert_eq!(ch.status(), ChannelStatus::Running);
}

#[test]
fn podman_kanal_start_unknown() {
    let managed = Arc::new(Mutex::new(Vec::new()));
    let cc = ChannelConfig {
        kind: "podman".into(),
        image: Some("node:22".into()),
        container: None,
        run_container: None,
        workdir: "/app".into(),
        host_root: Some("/tmp/x".into()),
        home: None,
    };
    let ch = podman::podman_from_config("node", &cc, 60, managed, None, crate::config::PodmanUserMapping::KeepId).expect("Run-Kanal bauen");
    assert_eq!(ch.status(), ChannelStatus::Unknown);
}

#[test]
fn podman_probe_status_entscheidet_je_nach_modus() {
    let running_passt = container::ContainerState::Running {
        userns: "keep-id:keep-gid".into(),
        binds: "[/host/x:/app]".into(),
        image: "node:22".into(),
    };
    let running_falsch = container::ContainerState::Running {
        userns: "host".into(),
        binds: "[/host/x:/app]".into(),
        image: "node:22".into(),
    };
    let bind = "/host/x:/app";
    let keep_id = crate::config::PodmanUserMapping::KeepId;
    assert_eq!(
        container::podman_probe_status(
            &container::ContainerState::Missing,
            PodmanMode::Run,
            bind,
            Some("node:22"),
            keep_id
        ),
        ChannelStatus::Unknown
    );
    assert_eq!(
        container::podman_probe_status(
            &container::ContainerState::Stopped,
            PodmanMode::Run,
            bind,
            Some("node:22"),
            keep_id
        ),
        ChannelStatus::Unknown
    );
    assert_eq!(
        container::podman_probe_status(
            &running_passt,
            PodmanMode::Run,
            bind,
            Some("node:22"),
            keep_id
        ),
        ChannelStatus::Running
    );
    assert_eq!(
        container::podman_probe_status(
            &running_falsch,
            PodmanMode::Run,
            bind,
            Some("node:22"),
            keep_id
        ),
        ChannelStatus::Unknown
    );
    assert_eq!(
        container::podman_probe_status(
            &container::ContainerState::Missing,
            PodmanMode::Attach,
            "",
            None,
            keep_id
        ),
        ChannelStatus::Problem
    );
    assert_eq!(
        container::podman_probe_status(
            &container::ContainerState::Stopped,
            PodmanMode::Attach,
            "",
            None,
            keep_id
        ),
        ChannelStatus::Problem
    );
    assert_eq!(
        container::podman_probe_status(&running_passt, PodmanMode::Attach, "", None, keep_id),
        ChannelStatus::Running
    );
}

#[test]
fn register_fuegt_kanal_zur_auswahl_hinzu() {
    let (mut registry, _ch) = test_registry("repo", PathBuf::from("/tmp/aidev-x"), Some("repo"));
    let neu: Arc<dyn Channel> = Arc::new(Local::new(PathBuf::from("/tmp/aidev-y")));
    assert_eq!(
        registry.register(neu.label(), neu.clone()),
        "Local:/tmp/aidev-y"
    );
    let names = registry.names();
    assert!(names.contains(&"Local:/tmp/aidev-y".to_string()));
    assert!(Arc::ptr_eq(
        &registry.get("Local:/tmp/aidev-y").expect("registriert"),
        &neu
    ));
    assert_eq!(registry.register("repo".to_string(), neu.clone()), "repo-2");
    assert_eq!(registry.register("repo".to_string(), neu), "repo-3");
    assert_eq!(registry.names().len(), 4);
}

#[test]
fn find_by_container_findet_passenden_run_kanal() {
    let managed = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ChannelRegistry {
        default: None,
        map: std::collections::HashMap::new(),
        managed: managed.clone(),
    };
    // Run-Kanal mit explizitem Container-Namen erzeugen.
    let cfg = crate::config::ChannelConfig {
        kind: "podman".into(),
        image: Some("alpine".into()),
        container: None,
        run_container: Some("aidev-proj-feature-alpine".into()),
        workdir: "/app".into(),
        host_root: Some("/tmp/aidev-proj".into()),
        home: None,
    };
    let ch = channel_from_config("alpine /tmp/aidev-proj", &cfg, 60, managed, None, crate::config::PodmanUserMapping::KeepId).unwrap();
    let name = registry.register("alpine /tmp/aidev-proj".into(), ch);
    // Über den Containernamen eines Run-Kanals wiederfinden (wie es der
    // Channel Builder beim Wiederverwenden tut).
    let found = registry.find_by_container("aidev-proj-feature-alpine");
    assert!(found.is_some(), "Kanal über Containernamen gefunden");
    assert_eq!(found.unwrap().0, name);
    // Unbekannter Container → kein Treffer.
    assert!(registry.find_by_container("does-not-exist").is_none());
}

#[test]
fn label_podman_name_default_root_local() {
    let ch = PodmanChannel {
        name: "node".into(),
        mode: PodmanMode::Run,
        container: "aidev-node".into(),
        workdir: "/app".into(),
        host_root: Some(PathBuf::from("/tmp/x")),
        image: Some("node:22".into()),
        timeout: Duration::from_secs(60),
        uid: 0,
        gid: 0,
        home: String::new(),
        usermapping: crate::config::PodmanUserMapping::KeepId,
        seq: AtomicUsize::new(1),
        status: Arc::new(Mutex::new(ChannelStatus::Unknown)),
        worktree: None,
        managed: Arc::new(Mutex::new(Vec::new())),
        shell: Mutex::new(None),
    };
    assert_eq!(ch.label(), "node");
    let local = Local::new(PathBuf::from("/tmp/aidev-test"));
    assert_eq!(local.label(), "Local:/tmp/aidev-test");
}

#[test]
fn local_und_attach_warmup_ist_ein_kein_op() {
    let local = Local::new(PathBuf::from("/tmp/aidev-test"));
    local.warmup();
    assert_eq!(local.status(), ChannelStatus::Running);
    let attach = PodmanChannel {
        name: "ci".into(),
        mode: PodmanMode::Attach,
        container: "aidev-ws".into(),
        workdir: "/app".into(),
        host_root: Some(PathBuf::from("/tmp/x")),
        image: None,
        timeout: Duration::from_secs(60),
        uid: 0,
        gid: 0,
        home: String::new(),
        usermapping: crate::config::PodmanUserMapping::KeepId,
        seq: AtomicUsize::new(1),
        status: Arc::new(Mutex::new(ChannelStatus::Unknown)),
        worktree: None,
        managed: Arc::new(Mutex::new(Vec::new())),
        shell: Mutex::new(None),
    };
    attach.warmup();
    assert_eq!(attach.status(), ChannelStatus::Unknown);
}

#[test]
fn timeout_kehrt_zeitnah_zurueck() {
    let start = std::time::Instant::now();
    let out = run::run_with_timeout(
        "sleep",
        &["100".into()],
        Path::new("."),
        Duration::from_millis(300),
    )
    .expect("Kommando startbar");
    let elapsed = start.elapsed();
    assert_eq!(out.exit_code, None);
    assert!(
        elapsed < Duration::from_secs(5),
        "Rückkehr trotz laufendem Prozess: {elapsed:?}"
    );
}

#[test]
fn schnelles_kind_mit_erbe_blockiert_nicht() {
    let start = std::time::Instant::now();
    let out = run::run_with_timeout(
        "sh",
        &["-c".into(), "sleep 30 & echo hello".into()],
        Path::new("."),
        Duration::from_secs(10),
    )
    .expect("Kommando startbar");
    let elapsed = start.elapsed();
    assert_eq!(out.exit_code, Some(0));
    assert!(
        out.stdout.contains("hello"),
        "Ausgabe trotz offener Erben-Pipe: {:?}",
        out.stdout
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "kein Blockieren an der geerbten Pipe: {elapsed:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn process_group_ist_eigene_gruppe() {
    use std::os::unix::process::CommandExt;
    use std::process::Command as StdCommand;
    let mut child = StdCommand::new("sh")
        .arg("-c")
        .arg("sleep 30")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    assert_eq!(run::proc_pgrp(pid), Some(pid as i32));
    child.kill().ok();
    let _ = child.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn prozessgruppe_wird_gekillt() {
    let dir = std::env::temp_dir().join(format!("aidev-kill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("enkel.pid");
    let start = std::time::Instant::now();
    let out = run::run_with_timeout(
        "sh",
        &[
            "-c".into(),
            format!("sleep 60 & echo $! > {}; wait", pidfile.display()),
        ],
        Path::new("."),
        Duration::from_millis(500),
    )
    .expect("Kommando startbar");
    let elapsed = start.elapsed();
    assert_eq!(out.exit_code, None);
    let gpid: u32 = std::fs::read_to_string(&pidfile)
        .expect("Enkel-PID geschrieben")
        .trim()
        .parse()
        .expect("numerische Enkel-PID");
    let now = std::time::Instant::now();
    loop {
        match proc_state(gpid) {
            None => break,
            Some('Z') => break,
            Some(_) if proc_is_sleep(gpid) => {}
            Some(_) => break,
        }
        assert!(
            Instant::now() - now < Duration::from_secs(3),
            "Enkel stirbt mit der Gruppe"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !proc_is_sleep(gpid),
        "Enkelprozess wurde beim Timeout mit der Gruppe beendet"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "Rückkehr trotz laufender Gruppe: {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
fn proc_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rfind(')')?;
    stat[after + 1..].trim_start().chars().next()
}

#[cfg(target_os = "linux")]
fn proc_is_sleep(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|c| c.trim() == "sleep")
        .unwrap_or(false)
}
