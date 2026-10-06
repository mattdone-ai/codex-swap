use super::*;
use crate::{
    cli::{Action, Cli, Output},
    store::Account,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{sync::mpsc, time::Duration};

struct Fixture {
    _directory: tempfile::TempDir,
    cli: Cli,
    main: PathBuf,
    runtime: PathBuf,
    source: Account,
    target: Account,
    source_runtime: Vec<u8>,
    target_saved: Vec<u8>,
}

fn credentials(number: u32, token: &str) -> Value {
    let payload = URL_SAFE_NO_PAD.encode(format!(
        r#"{{"email":"user-{number}@example.test","https://api.openai.com/auth":{{"chatgpt_user_id":"user-{number}"}}}}"#
    ));
    json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "account_id": format!("workspace-{number}"),
            "access_token": format!("access-{token}"),
            "refresh_token": format!("refresh-{token}"),
            "id_token": format!("e30.{payload}.synthetic")
        }
    })
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("main");
        fsutil::private_dir(&main).unwrap();
        let cli = Cli {
            data_dir: Some(directory.path().join("registry")),
            codex_home: Some(main.clone()),
            codex_bin: None,
            stop_codex: false,
            command: Action::Status(Output { json: false }),
        };
        let mut store = Store::open(&cli).unwrap();
        let runtime = home(&store);
        ensure_marker(&runtime).unwrap();
        let mut accounts = Vec::new();
        for number in [1, 2] {
            let account_home = store.root.join("accounts").join(number.to_string());
            fsutil::private_dir(&account_home).unwrap();
            fsutil::atomic_json(
                &account_home.join("auth.json"),
                &credentials(number, &format!("saved-{number}")),
            )
            .unwrap();
            accounts.push(Account {
                number,
                alias: Some(format!("account-{number}")),
                home: account_home,
                managed: true,
                share_history: true,
                identity: Some(
                    auth::require(&store.root.join("accounts").join(number.to_string())).unwrap(),
                ),
                enabled: true,
            });
        }
        store.data.accounts = accounts;
        store.data.default = Some(1);
        store.data.next_number = 3;
        store.save().unwrap();
        fsutil::atomic_json(
            &runtime.join("auth.json"),
            &credentials(1, "runtime-newest"),
        )
        .unwrap();
        let source_runtime = fs::read(runtime.join("auth.json")).unwrap();
        let target_saved = fs::read(store.data.accounts[1].home.join("auth.json")).unwrap();
        Self {
            _directory: directory,
            cli,
            main,
            runtime,
            source: store.data.accounts[0].clone(),
            target: store.data.accounts[1].clone(),
            source_runtime,
            target_saved,
        }
    }

    fn guard(&self) -> ActivationGuard {
        ActivationGuard {
            source: self.source.identity.clone().unwrap(),
            target: self.target.identity.clone().unwrap(),
            target_number: self.target.number,
            autoswitch: Store::open(&self.cli).unwrap().data.preferences.autoswitch,
        }
    }
}

#[test]
fn activation_uses_runtime_source_while_session_lease_is_held() {
    let fixture = Fixture::new();
    let store = Store::open(&fixture.cli).unwrap();
    let session_lease = store.lease(&fixture.runtime, false).unwrap();
    drop(store);

    activate_guarded(&fixture.cli, &fixture.guard()).unwrap();

    assert_eq!(
        fs::read(fixture.source.home.join("auth.json")).unwrap(),
        fixture.source_runtime
    );
    assert_eq!(
        fs::read(fixture.runtime.join("auth.json")).unwrap(),
        fixture.target_saved
    );
    assert!(!fixture.main.join("auth.json").exists());
    drop(session_lease);
}

#[test]
fn activation_revalidates_runtime_owner_without_disclosing_credentials() {
    let fixture = Fixture::new();
    fsutil::atomic_json(
        &fixture.runtime.join("auth.json"),
        &credentials(2, "unexpected-secret-sentinel"),
    )
    .unwrap();
    let source_before = fs::read(fixture.source.home.join("auth.json")).unwrap();
    let error = format!(
        "{:#}",
        activate_guarded(&fixture.cli, &fixture.guard()).unwrap_err()
    );
    assert!(error.contains("identity changed"));
    assert!(!error.contains("unexpected-secret-sentinel"));
    assert_eq!(
        fs::read(fixture.source.home.join("auth.json")).unwrap(),
        source_before
    );
    assert!(!fixture.main.join("auth.json").exists());
}

#[test]
fn activation_refuses_while_codext_holds_the_refresh_lock() {
    let fixture = Fixture::new();
    let first = auth_lock(&fixture.runtime).unwrap();
    let runtime = fixture.runtime.clone();
    let (sender, receiver) = mpsc::channel();
    let waiter = std::thread::spawn(move || sender.send(auth_lock(&runtime).is_ok()).unwrap());
    assert!(!receiver.recv_timeout(Duration::from_secs(2)).unwrap());
    drop(first);
    waiter.join().unwrap();
    assert!(auth_lock(&fixture.runtime).is_ok());
}

#[test]
fn activation_never_uses_the_stock_main_home_as_a_saved_snapshot() {
    let fixture = Fixture::new();
    fsutil::atomic_json(
        &fixture.main.join("auth.json"),
        &credentials(1, "stock-main"),
    )
    .unwrap();
    let main_before = fs::read(fixture.main.join("auth.json")).unwrap();
    let mut store = Store::open(&fixture.cli).unwrap();
    store.data.accounts[0].home = fixture.main.clone();
    store.data.accounts[0].managed = false;
    store.save().unwrap();
    drop(store);

    let error = activate_guarded(&fixture.cli, &fixture.guard())
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not have an isolated saved home"));
    assert_eq!(
        fs::read(fixture.main.join("auth.json")).unwrap(),
        main_before
    );
}

#[test]
fn configured_binary_requires_matching_explicit_pin() {
    let fixture = Fixture::new();
    let binary = fixture._directory.path().join("codext-patched");
    fs::write(&binary, b"synthetic patched codext").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o555)).unwrap();
    }
    let mut store = Store::open(&fixture.cli).unwrap();
    store.data.preferences.seamless_codext_bin = Some(binary.to_string_lossy().into_owned());
    store.data.preferences.seamless_codext_sha256 =
        Some(format!("{:x}", Sha256::digest(b"synthetic patched codext")));
    let verified = configured_binary(&store).unwrap();
    #[cfg(target_os = "linux")]
    assert!(
        Path::new(&verified.command).starts_with("/proc/self/fd"),
        "verified launch must stay bound to its open file"
    );
    #[cfg(all(unix, not(target_os = "linux")))]
    assert!(Path::new(&verified.command).starts_with("/dev/fd"));
    #[cfg(windows)]
    assert_eq!(verified.command, binary.as_os_str());
    store.data.preferences.seamless_codext_sha256 = Some("0".repeat(64));
    let error = configured_binary(&store).unwrap_err().to_string();
    assert!(error.contains("digest does not match"));
}

#[cfg(unix)]
#[test]
fn marker_and_lock_are_private_regular_files() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    let lock = auth_lock(&fixture.runtime).unwrap();
    for name in [MARKER_FILE, AUTH_LOCK_FILE] {
        let metadata = fs::symlink_metadata(fixture.runtime.join(name)).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o777, 0o600);
    }
    drop(lock);
}

#[test]
fn managed_overrides_and_hidden_exec_subcommands_are_rejected() {
    for argument in [
        "-c=sqlite_home=\"/tmp/escape\"",
        "-c=cli_auth_credentials_store=\"keyring\"",
    ] {
        let error = launch::check_overrides(&[argument.into()], true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("xswap owns"), "{error}");
    }
    for args in [
        vec!["--model".into(), "synthetic".into(), "exec".into()],
        vec!["-m=synthetic".into(), "e".into()],
        vec!["x".into()],
        vec!["--sandbox=read-only".into(), "review".into()],
    ] {
        assert!(matches!(
            config_assets::subcommand(&args),
            Some("exec" | "e" | "x" | "review")
        ));
    }
}

#[test]
fn interactive_sessions_force_embedded_mode_and_reject_daemon_escape_paths() {
    for args in [
        vec![],
        vec!["--model".into(), "synthetic".into()],
        vec!["--profile=work".into(), "resume".into(), "--last".into()],
        vec!["-C".into(), "/tmp".into(), "fork".into(), "--last".into()],
    ] {
        let forwarded = session_args(&args).unwrap();
        assert_eq!(forwarded.first().unwrap(), "--no-daemon");
        assert_eq!(&forwarded[1..], args);
    }
    let already_embedded = vec!["resume".into(), "--no-daemon".into(), "--last".into()];
    assert_eq!(session_args(&already_embedded).unwrap(), already_embedded);

    let foreground = vec![
        "--model".into(),
        "synthetic".into(),
        "app-server".into(),
        "--stdio".into(),
    ];
    assert_eq!(session_args(&foreground).unwrap(), foreground);

    for args in [
        vec!["--profile".into(), "work".into(), "agents".into()],
        vec!["remote-control".into(), "start".into()],
        vec!["queue".into(), "thread".into(), "message".into()],
        vec!["app-server".into(), "daemon".into(), "start".into()],
        vec!["app-server".into(), "proxy".into()],
        vec![
            "--model".into(),
            "synthetic".into(),
            "app-server".into(),
            "daemon".into(),
            "start".into(),
        ],
        vec!["--remote=ws://example.test".into(), "resume".into()],
        vec![
            "resume".into(),
            "--remote".into(),
            "ws://example.test".into(),
        ],
        vec!["fork".into(), "--remote-auth-token-env=TOKEN".into()],
    ] {
        assert!(session_args(&args).is_err(), "accepted {args:?}");
    }

    let literal_prompt = vec!["--".into(), "--remote=not-an-option".into()];
    let forwarded = session_args(&literal_prompt).unwrap();
    assert_eq!(forwarded.first().unwrap(), "--no-daemon");
    assert_eq!(&forwarded[1..], literal_prompt);
}

#[test]
fn reauthentication_updates_the_active_runtime_under_its_auth_lock() {
    let fixture = Fixture::new();
    let document = credentials(1, "reauthenticated");
    let (_, identity) =
        auth::credentials_from_bytes(&serde_json::to_vec(&document).unwrap()).unwrap();
    let path = fixture.source.home.join("auth.json");
    let destination = crate::account_state::LoginDestination {
        account: fixture.source.clone(),
        effective_home: fixture.source.home.clone(),
        concurrent: true,
        previous: vec![(path.clone(), fsutil::optional_bytes(&path).unwrap())],
    };

    crate::account_state::commit_login(&fixture.cli, &destination, &document, identity).unwrap();

    assert_eq!(auth::credentials(&fixture.runtime).unwrap().0, document);
    assert_eq!(auth::credentials(&fixture.source.home).unwrap().0, document);
}

#[test]
fn purge_refuses_an_unmarked_runtime() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.runtime.join(MARKER_FILE)).unwrap();

    let error = crate::maintenance::purge(&fixture.cli, true, &Output { json: false })
        .unwrap_err()
        .to_string();

    assert!(error.contains(MARKER_FILE), "{error}");
    assert!(fixture.runtime.exists());
}

#[test]
fn session_setup_refuses_to_adopt_an_existing_unmarked_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let managed = directory.path().join("managed");
    fsutil::private_dir(&managed).unwrap();
    let runtime = managed.join("runtime");
    fsutil::private_dir(&runtime).unwrap();
    fs::write(runtime.join("unrelated"), b"preserve me").unwrap();

    let error = ensure_marker(&runtime).unwrap_err().to_string();

    assert!(
        error.contains("without the xswap managed-runtime marker"),
        "{error}"
    );
    assert_eq!(fs::read(runtime.join("unrelated")).unwrap(), b"preserve me");
    assert!(!runtime.join(MARKER_FILE).exists());
}

#[cfg(unix)]
#[test]
fn configured_binary_requires_an_immutable_unix_artifact() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let binary = fixture._directory.path().join("codext-writable");
    fs::write(&binary, b"synthetic patched codext").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let mut store = Store::open(&fixture.cli).unwrap();
    store.data.preferences.seamless_codext_bin = Some(binary.to_string_lossy().into_owned());
    store.data.preferences.seamless_codext_sha256 =
        Some(format!("{:x}", Sha256::digest(b"synthetic patched codext")));

    let error = configured_binary(&store).unwrap_err().to_string();

    assert!(error.contains("mode 0555"));
}

#[cfg(unix)]
#[test]
fn runtime_parent_must_not_be_a_symlink() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let main = directory.path().join("main");
    fsutil::private_dir(&main).unwrap();
    let cli = Cli {
        data_dir: Some(directory.path().join("registry")),
        codex_home: Some(main),
        codex_bin: None,
        stop_codex: false,
        command: Action::Status(Output { json: false }),
    };
    let store = Store::open(&cli).unwrap();
    let outside = directory.path().join("outside");
    fsutil::private_dir(&outside).unwrap();
    symlink(&outside, store.root.join("runtime")).unwrap();

    let error = ensure_marker(&home(&store)).unwrap_err().to_string();
    assert!(error.contains("real directory"), "{error}");
}
