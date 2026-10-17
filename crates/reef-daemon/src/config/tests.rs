use std::collections::HashMap;

use tempfile::TempDir;

use super::*;

fn resolve(
    overrides: &PathOverrides,
    vars: &[(&str, &str)],
    runtime_dir: Option<&str>,
) -> Result<Paths, ConfigError> {
    let vars: HashMap<_, _> = vars.iter().copied().collect();
    Paths::resolve_with(
        overrides,
        |name| vars.get(name).map(OsString::from),
        || {
            Ok(PlatformDefaults {
                state_dir: PathBuf::from("/home/u/.local/state/reef"),
                runtime_dir: runtime_dir.map(PathBuf::from),
            })
        },
    )
}

/// A state directory in a temporary directory, with `config.toml` holding
/// `file` when given.
fn state_dir(file: Option<&str>) -> TempDir {
    let tmp = tempfile::Builder::new().prefix("reef-").tempdir().unwrap();
    if let Some(text) = file {
        std::fs::write(tmp.path().join("config.toml"), text).unwrap();
    }
    tmp
}

/// [`Config::load_with`] with the state directory `tmp` given through
/// `REEF_STATE_DIR`, plus `vars`.
fn load(
    tmp: &TempDir,
    overrides: &PathOverrides,
    vars: &[(&str, &str)],
) -> Result<Config, ConfigError> {
    let mut vars: HashMap<String, OsString> = vars
        .iter()
        .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
        .collect();
    vars.insert(STATE_DIR_ENV.to_owned(), tmp.path().as_os_str().to_owned());
    Config::load_with(
        overrides,
        |name| vars.get(name).cloned(),
        || Err(ConfigError::NoHome),
    )
}

fn load_file(text: &str) -> Result<Config, ConfigError> {
    load(&state_dir(Some(text)), &PathOverrides::default(), &[])
}

#[test]
fn defaults_and_layout() {
    let paths = resolve(&PathOverrides::default(), &[], None).unwrap();
    let state = Path::new("/home/u/.local/state/reef");
    assert_eq!(paths.state_dir, state);
    assert_eq!(paths.socket, state.join("daemon.sock"));
    assert_eq!(paths.config_file, state.join("config.toml"));
    assert_eq!(paths.db, state.join("reef.sqlite3"));
    assert_eq!(paths.lock, state.join("daemon.lock"));
    assert_eq!(paths.pid, state.join("daemon.pid"));
    assert_eq!(paths.logs, state.join("logs"));
    let id = RepoId::generate();
    assert_eq!(paths.repo_dir(id), state.join("repos").join(id.to_string()));
}

#[test]
fn runtime_dir_only_for_the_default_state_dir() {
    let paths = resolve(&PathOverrides::default(), &[], Some("/run/user/1")).unwrap();
    assert_eq!(paths.socket, Path::new("/run/user/1/reef/daemon.sock"));
    let paths = resolve(
        &PathOverrides::default(),
        &[(STATE_DIR_ENV, "/tmp/s")],
        Some("/run/user/1"),
    )
    .unwrap();
    assert_eq!(paths.socket, Path::new("/tmp/s/daemon.sock"));
}

#[test]
fn overrides_win_over_env() {
    let overrides = PathOverrides {
        state_dir: Some("/a".into()),
        socket: None,
        config_file: Some("/c.toml".into()),
    };
    let paths = resolve(
        &overrides,
        &[
            (STATE_DIR_ENV, "/b"),
            (SOCKET_ENV, "/s.sock"),
            (CONFIG_ENV, "/d.toml"),
        ],
        None,
    )
    .unwrap();
    assert_eq!(paths.state_dir, Path::new("/a"));
    assert_eq!(paths.socket, Path::new("/s.sock"));
    assert_eq!(paths.config_file, Path::new("/c.toml"));
}

#[test]
fn empty_env_is_unset_and_relative_is_absolutized() {
    let paths = resolve(&PathOverrides::default(), &[(STATE_DIR_ENV, "")], None).unwrap();
    assert_eq!(paths.state_dir, Path::new("/home/u/.local/state/reef"));
    let paths = resolve(&PathOverrides::default(), &[(STATE_DIR_ENV, "rel")], None).unwrap();
    assert!(paths.state_dir.is_absolute());
    assert!(paths.state_dir.ends_with("rel"));
}

#[test]
fn socket_length_limit() {
    let ok = format!("/{}", "s".repeat(SOCKET_PATH_MAX - 1));
    assert!(resolve(&PathOverrides::default(), &[(SOCKET_ENV, &ok)], None).is_ok());
    let long = format!("/{}", "s".repeat(SOCKET_PATH_MAX));
    let err = resolve(&PathOverrides::default(), &[(SOCKET_ENV, &long)], None).unwrap_err();
    assert!(
        matches!(err, ConfigError::SocketPathTooLong { max: SOCKET_PATH_MAX, ref path } if path.as_os_str().len() == SOCKET_PATH_MAX + 1)
    );
    assert_eq!(err.kind(), ErrorKind::InvalidArgument);
}

#[test]
fn in_state_dir_ignores_the_environment() {
    let paths = Paths::in_state_dir(Path::new("/tmp/reef-x")).unwrap();
    assert_eq!(paths.socket, Path::new("/tmp/reef-x/daemon.sock"));
}

/// Test 1: every default is the README's.
#[test]
fn defaults_load_with_no_file() {
    let tmp = state_dir(None);
    let config = load(&tmp, &PathOverrides::default(), &[]).unwrap();
    let state = tmp.path();
    let f = &config.file;
    let mins = Duration::from_mins;
    let days = |n: u64| Duration::from_hours(n * 24);

    assert_eq!(config.paths.socket, state.join("daemon.sock"));
    assert_eq!(config.paths.pid, state.join("daemon.pid"));
    assert_eq!(f.daemon, DaemonSection::default());
    assert_eq!(f.identity.name, "reef");
    assert_eq!(f.identity.email, None);
    assert_eq!(config.identity.name, "reef");
    assert_eq!(
        config.identity.email,
        format!("reef@{}", machine_hostname().to_lowercase())
    );
    assert_eq!(f.log.keep_days, 7);
    assert_eq!(config.log.keep_days, 7);
    assert_eq!(f.lease.default, mins(30));
    assert_eq!(f.lease.max, Duration::from_hours(24));
    assert_eq!(f.lease.grace, mins(5));
    assert_eq!(f.lease.restart_grace, mins(5));
    assert_eq!(f.lease.heartbeat, Duration::from_secs(60));
    assert_eq!(f.lease.tick, Duration::from_secs(10));
    assert_eq!(f.watcher.backend, WatcherBackend::Auto);
    assert_eq!(f.watcher.debounce, Duration::from_millis(500));
    assert_eq!(f.watcher.min_interval, Duration::from_secs(1));
    assert_eq!(f.watcher.fallback_interval, Duration::from_secs(60));
    assert_eq!(f.watcher.max_concurrent_scans, None);
    assert_eq!(
        config.max_concurrent_scans(),
        std::thread::available_parallelism().unwrap().get()
    );
    assert_eq!(f.watcher.watchman_threshold_files, 50_000);
    assert_eq!(f.workspace.backend, BackendChoice::Auto);
    assert_eq!(f.workspace.root, "<state>/repos/<repo-id>/ws");
    assert_eq!(f.workspace.fuse_overlayfs, "fuse-overlayfs");
    assert!(!f.workspace.overlay_nosuid);
    assert_eq!(f.base_cache.max_entries, 8);
    assert_eq!(f.gc.undo_window, days(7));
    assert_eq!(f.gc.git_executable, "git");
    assert_eq!(f.gc.reindex_limit, 100_000);
    assert_eq!(f.backup.retention, days(30));
    assert_eq!(f.backup.quiesce_max, Duration::from_secs(60));
    assert_eq!(f.backup.sqlite_pages_per_step, 256);
    assert_eq!(f.snapshot.max_new_file_size, ByteSize(1 << 20));

    assert!(config.sources.values().all(|s| *s == Source::Default));
    assert_eq!(
        config.sources.len(),
        KEYS.iter().map(|(_, keys)| keys.len()).sum::<usize>()
    );
    assert_eq!(config.origin.state_dir, Source::Env);
    assert!(!config.origin.config_file_read);
    assert!(
        std::fs::read_dir(state).unwrap().next().is_none(),
        "loading creates nothing"
    );
}

/// Test 2.
#[test]
fn file_wins_over_default() {
    let config = load_file("[lease]\ndefault = \"2h\"\n").unwrap();
    assert_eq!(config.file.lease.default, Duration::from_hours(2));
    assert_eq!(config.sources["lease.default"], Source::File);
    assert_eq!(config.sources["lease.max"], Source::Default);
    assert!(config.origin.config_file_read);
}

/// Test 3, and the rest of the socket's layers.
#[test]
fn env_socket_wins_over_file() {
    let tmp = state_dir(Some("[daemon]\nsocket = \"/tmp/file.sock\"\n"));
    let none = PathOverrides::default();
    let config = load(&tmp, &none, &[(SOCKET_ENV, "/tmp/env.sock")]).unwrap();
    assert_eq!(config.paths.socket, Path::new("/tmp/env.sock"));
    assert_eq!(config.sources["daemon.socket"], Source::Env);

    let config = load(&tmp, &none, &[]).unwrap();
    assert_eq!(config.paths.socket, Path::new("/tmp/file.sock"));
    assert_eq!(config.sources["daemon.socket"], Source::File);

    let flag = PathOverrides {
        socket: Some("/tmp/flag.sock".into()),
        ..PathOverrides::default()
    };
    let config = load(&tmp, &flag, &[(SOCKET_ENV, "/tmp/env.sock")]).unwrap();
    assert_eq!(config.paths.socket, Path::new("/tmp/flag.sock"));
    assert_eq!(config.sources["daemon.socket"], Source::Flag);
}

#[test]
fn relative_paths_in_the_file_are_under_the_state_dir() {
    let tmp = state_dir(Some(
        "[daemon]\nsocket = \"s.sock\"\npid_file = \"run/p.pid\"\n",
    ));
    let config = load(&tmp, &PathOverrides::default(), &[]).unwrap();
    assert_eq!(config.paths.socket, tmp.path().join("s.sock"));
    assert_eq!(config.paths.pid, tmp.path().join("run/p.pid"));
    assert_eq!(config.sources["daemon.pid_file"], Source::File);
}

#[test]
fn a_named_config_file_must_exist() {
    let tmp = state_dir(None);
    let missing = tmp.path().join("confg.toml");
    let err = load(
        &tmp,
        &PathOverrides::default(),
        &[(CONFIG_ENV, missing.to_str().unwrap())],
    )
    .unwrap_err();
    assert!(
        matches!(&err, ConfigError::Missing { path, source_layer: Source::Env } if *path == missing),
        "{err:?}"
    );
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert!(err.to_string().contains(CONFIG_ENV), "{err}");
    let flag = PathOverrides {
        config_file: Some(missing.clone()),
        ..PathOverrides::default()
    };
    let err = load(&tmp, &flag, &[]).unwrap_err();
    assert!(err.to_string().contains("--config"), "{err}");
}

#[test]
fn reef_config_names_the_file() {
    let tmp = state_dir(None);
    let other = tmp.path().join("other.toml");
    std::fs::write(&other, "[base_cache]\nmax_entries = 3\n").unwrap();
    let config = load(
        &tmp,
        &PathOverrides::default(),
        &[(CONFIG_ENV, other.to_str().unwrap())],
    )
    .unwrap();
    assert_eq!(config.paths.config_file, other);
    assert_eq!(config.origin.config_file, Source::Env);
    assert_eq!(config.file.base_cache.max_entries, 3);
}

/// Test 4.
#[test]
fn unknown_key_names_the_key_and_the_file() {
    for (text, expected) in [
        ("[leas]\ndefault = \"2h\"\n", "leas.default"),
        ("leas.default = \"2h\"\n", "leas.default"),
        ("[lease]\ndefualt = \"2h\"\n", "lease.defualt"),
    ] {
        let err = load_file(text).unwrap_err();
        let ConfigError::UnknownKey { key, path, .. } = &err else {
            panic!("{text:?}: {err:?}");
        };
        assert_eq!(key, expected);
        assert!(path.ends_with("config.toml"));
        assert_eq!(err.kind(), ErrorKind::InvalidArgument);
        let message = reef_lib::error::ErrorChain(&err).to_string();
        assert!(message.contains(expected), "{message}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("line "), "toml's span is kept: {message}");
    }
}

#[test]
fn syntax_and_type_errors_are_parse_errors() {
    for text in [
        "[lease\n",
        "[lease]\ndefault = 5\n",
        "[lease]\ndefault = \"2 fortnights\"\n",
        "[base_cache]\nmax_entries = \"many\"\n",
        "[workspace]\nbackend = \"zfs\"\n",
        "[watcher]\nbackend = \"inotify\"\n",
    ] {
        let err = load_file(text).unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "{text:?}: {err:?}"
        );
        assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    }
}

/// Test 5, and every other rule.
#[test]
fn validation_names_the_key() {
    for (text, expected) in [
        (
            "[lease]\ndefault = \"2d\"\nmax = \"24h\"\n",
            "lease.default",
        ),
        ("[lease]\ntick = \"5m\"\n", "lease.tick"),
        ("[lease]\ntick = \"0s\"\n", "lease.tick"),
        ("[lease]\nheartbeat = \"0s\"\n", "lease.heartbeat"),
        ("[lease]\ndefault = \"1m\"\n", "lease.grace"),
        ("[lease]\nrestart_grace = \"0s\"\n", "lease.restart_grace"),
        (
            "[lease]\nmax = \"1h\"\ndefault = \"1h\"\nrestart_grace = \"2h\"\n",
            "lease.restart_grace",
        ),
        (
            "[lease]\ndefault = \"10m\"\nheartbeat = \"10m\"\n",
            "lease.heartbeat",
        ),
        ("[watcher]\ndebounce = \"2m\"\n", "watcher.debounce"),
        (
            "[watcher]\nmax_concurrent_scans = 0\n",
            "watcher.max_concurrent_scans",
        ),
        ("[base_cache]\nmax_entries = 0\n", "base_cache.max_entries"),
        ("[log]\nkeep_days = 0\n", "log.keep_days"),
        (
            "[backup]\nsqlite_pages_per_step = 0\n",
            "backup.sqlite_pages_per_step",
        ),
        (
            "[snapshot]\nmax_new_file_size = \"1MB\"\n",
            "snapshot.max_new_file_size",
        ),
        (
            "[snapshot]\nmax_new_file_size = -1\n",
            "snapshot.max_new_file_size",
        ),
        ("[workspace]\nroot = \"/ws\"\n", "workspace.root"),
        ("[daemon]\nsocket = \"\"\n", "daemon.socket"),
        ("[daemon]\npid_file = \"\"\n", "daemon.pid_file"),
        ("[identity]\nname = \"\"\n", "identity.name"),
        ("[identity]\nemail = \"\"\n", "identity.email"),
        (
            "[workspace]\nfuse_overlayfs = \"\"\n",
            "workspace.fuse_overlayfs",
        ),
        ("[gc]\ngit_executable = \"\"\n", "gc.git_executable"),
        (
            "[workspace]\nroot = \"/x/<state>/<repo-id>\"\n",
            "workspace.root",
        ),
    ] {
        let err = load_file(text).unwrap_err();
        let ConfigError::Invalid { key, .. } = &err else {
            panic!("{text:?}: {err:?}");
        };
        assert_eq!(key, expected, "{text:?}");
        assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    }
}

#[test]
fn heartbeat_near_the_default_warns() {
    assert_eq!(load_file("").unwrap().warnings(), Vec::<String>::new());
    let config = load_file("[lease]\ndefault = \"10m\"\nheartbeat = \"5m\"\n").unwrap();
    let warnings = config.warnings();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with("lease.heartbeat (5m)"),
        "{warnings:?}"
    );
}

#[test]
fn sizes_and_durations_parse() {
    let config = load_file(
        "[snapshot]\nmax_new_file_size = 4096\n[lease]\ndefault = \"90s\"\ngrace = \"1m\"\n[gc]\nundo_window = \"36h\"\n",
    )
    .unwrap();
    assert_eq!(config.file.snapshot.max_new_file_size, ByteSize(4096));
    assert_eq!(config.file.lease.default, Duration::from_secs(90));
    assert_eq!(config.file.gc.undo_window, Duration::from_hours(36));
    let config = load_file("[snapshot]\nmax_new_file_size = \"16MiB\"\n").unwrap();
    assert_eq!(config.file.snapshot.max_new_file_size, ByteSize(16 << 20));
}

#[test]
fn formats_round_trip() {
    for (value, text) in [
        (Duration::from_millis(500), "500ms"),
        (Duration::from_secs(90), "90s"),
        (Duration::from_secs(60), "60s"),
        (Duration::from_mins(2), "2m"),
        (Duration::from_hours(1), "1h"),
        (Duration::from_mins(30), "30m"),
        (Duration::from_hours(24), "24h"),
        (Duration::from_hours(7 * 24), "7d"),
        (Duration::ZERO, "0s"),
        (Duration::from_nanos(1_500), "1500ns"),
    ] {
        assert_eq!(format_duration(value), text);
        assert_eq!(
            humantime_serde::re::humantime::parse_duration(text).unwrap(),
            value
        );
    }
    for (bytes, text) in [
        (1 << 20, "1MiB"),
        (3 << 30, "3GiB"),
        (1000, "1000B"),
        (0, "0B"),
    ] {
        assert_eq!(ByteSize(bytes).to_string(), text);
        assert_eq!(parse_byte_size(text).unwrap(), bytes);
    }
}

/// Test 6.
#[test]
fn workspace_root_substitutes_the_repo_id() {
    let id = RepoId::generate();
    let tmp = state_dir(None);
    let config = load(&tmp, &PathOverrides::default(), &[]).unwrap();
    assert_eq!(
        config.workspace_root(id),
        tmp.path().join("repos").join(id.to_string()).join("ws")
    );
    let config = load_file("[workspace]\nroot = \"/srv/ws/<repo-id>\"\n").unwrap();
    assert_eq!(
        config.workspace_root(id),
        Path::new("/srv/ws").join(id.to_string())
    );
    let tmp = state_dir(Some("[workspace]\nroot = \"elsewhere/<repo-id>\"\n"));
    let config = load(&tmp, &PathOverrides::default(), &[]).unwrap();
    assert_eq!(
        config.workspace_root(id),
        tmp.path().join("elsewhere").join(id.to_string())
    );
}

/// Test 7, with the companion: a snapshot written with the daemon's
/// settings records the host and user.
#[test]
fn jj_settings_carry_the_operation_identity() {
    let tmp = state_dir(Some(
        "[identity]\nemail = \"agents@example.com\"\n[snapshot]\nmax_new_file_size = \"2MiB\"\n",
    ));
    let config = load(&tmp, &PathOverrides::default(), &[]).unwrap();
    let host = nix::unistd::gethostname().unwrap().into_string().unwrap();
    assert_eq!(config.operation.hostname, host);
    let settings = config.jj_settings().unwrap();
    assert_eq!(settings.operation_hostname(), host);
    assert_eq!(settings.operation_username(), os_username());
    assert_ne!(settings.operation_username(), "");
    assert_eq!(settings.user_email(), "agents@example.com");
    assert_eq!(
        reef_lib::jj::settings::max_new_file_size(&settings).unwrap(),
        2 << 20
    );

    let root = tmp.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let mut repo =
        pollster::block_on(reef_lib::jj::Repo::init_internal_git(&root, &settings)).unwrap();
    std::fs::write(root.join("a.txt"), "hello\n").unwrap();
    pollster::block_on(repo.snapshot("test")).unwrap();
    let head = &pollster::block_on(repo.op_log(1)).unwrap()[0];
    assert_eq!(head.hostname, host);
    assert_eq!(head.username, os_username());
}

/// Strings toml has to escape or write in another form, and durations and
/// sizes outside the exact-unit defaults, load back unchanged.
#[test]
fn effective_toml_round_trips_odd_values() {
    let tmp = state_dir(Some(concat!(
        "[identity]\nname = 'A \"quoted\" \\ name\twith tab'\nemail = \"a@b\\nc\"\n",
        "[daemon]\nsocket = \"/tmp/it's here.sock\"\n",
        "[lease]\ndefault = \"1h 30m\"\nmax = \"2w\"\ntick = \"1500ms\"\n",
        "[snapshot]\nmax_new_file_size = 1536\n",
        "[workspace]\nroot = \"<state>/ws dir/<repo-id>\"\n",
    )));
    let config = load(&tmp, &PathOverrides::default(), &[]).unwrap();
    let text = config.to_effective_toml();
    let back: FileConfig = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(back, config.effective_file());
    assert_eq!(back.identity.name, "A \"quoted\" \\ name\twith tab");
    assert_eq!(back.lease.default, Duration::from_mins(90));
}

#[test]
fn effective_toml_round_trips() {
    let config = load_file("[lease]\ndefault = \"2h\"\n[workspace]\nbackend = \"copy\"\n").unwrap();
    let text = config.to_effective_toml();
    let back: FileConfig = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(back, config.effective_file());
    assert!(text.contains("default = \"2h\"  # source: file"), "{text}");
    assert!(
        text.contains("backend = \"copy\"  # source: file"),
        "{text}"
    );
    assert!(text.contains("# source: default"), "{text}");
    // Every key in KEYS is printed, under its section, in order.
    let mut printed = Vec::new();
    let mut section = "";
    for line in text.lines() {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
        } else if let Some((key, _)) = line.split_once(" = ") {
            printed.push(format!("{section}.{key}"));
        }
    }
    let expected: Vec<_> = default_sources().into_keys().collect();
    let mut sorted = printed.clone();
    sorted.sort();
    assert_eq!(sorted, expected);
    let in_keys_order: Vec<_> = KEYS
        .iter()
        .flat_map(|(s, keys)| keys.iter().map(move |k| format!("{s}.{k}")))
        .collect();
    assert_eq!(printed, in_keys_order);
}

/// [`KEYS`] lists exactly the fields of [`FileConfig`].
#[test]
fn keys_match_the_struct() {
    let config = Config::with_defaults(Paths::in_state_dir(Path::new("/tmp/reef-k")).unwrap());
    let table = toml::Table::try_from(config.effective_file()).unwrap();
    let fields: Vec<(String, Vec<String>)> = table
        .iter()
        .map(|(section, value)| {
            let mut keys: Vec<_> = value.as_table().unwrap().keys().cloned().collect();
            keys.sort();
            (section.clone(), keys)
        })
        .collect();
    let mut listed: Vec<(String, Vec<String>)> = KEYS
        .iter()
        .map(|(section, keys)| {
            let mut keys: Vec<_> = keys.iter().map(|k| (*k).to_owned()).collect();
            keys.sort();
            ((*section).to_owned(), keys)
        })
        .collect();
    listed.sort();
    let mut fields = fields;
    fields.sort();
    assert_eq!(fields, listed);
}
