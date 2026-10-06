use crate::{
    account_state::{ActivationGuard, validate_activation_guard},
    auth, config_assets, fsutil, launch, sharing,
    store::{Account, Store, require_registered_identity},
};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const RUNTIME_DIR: &str = "runtime/seamless";
const MARKER_FILE: &str = ".xswap-managed-runtime";
const MARKER_CONTENTS: &[u8] = b"v1\n";
const AUTH_LOCK_FILE: &str = "auth.json.xswap.lock";

pub(crate) struct RuntimeAuth {
    pub account: Account,
    pub document: serde_json::Value,
    pub home: PathBuf,
}

#[derive(Debug)]
struct VerifiedBinary {
    command: OsString,
    _file: File,
}

pub(crate) struct RuntimeLoginGuard {
    pub path: PathBuf,
    _lease: File,
    _auth_lock: File,
}

pub(crate) fn home(store: &Store) -> PathBuf {
    store.root.join(RUNTIME_DIR)
}

fn ensure_private_file(path: &Path) -> Result<()> {
    fsutil::regular(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = fs::metadata(path)?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            bail!("{} must be owned by you with mode 0600", path.display());
        }
    }
    #[cfg(windows)]
    crate::platform::private_permissions(path, false)?;
    Ok(())
}

fn validate_marker(runtime: &Path) -> Result<()> {
    let marker = runtime.join(MARKER_FILE);
    ensure_private_file(&marker)?;
    if fs::read(&marker)? != MARKER_CONTENTS {
        bail!(
            "{} is not a valid xswap managed-runtime marker",
            marker.display()
        );
    }
    Ok(())
}

pub(crate) fn validate_runtime(runtime: &Path) -> Result<()> {
    let parent = runtime.parent().context("managed runtime has no parent")?;
    fsutil::private_dir(parent)?;
    fsutil::private_dir(runtime)?;
    validate_marker(runtime)
}

pub(crate) fn ensure_marker(runtime: &Path) -> Result<()> {
    let parent = runtime.parent().context("managed runtime has no parent")?;
    fsutil::private_dir(parent)?;
    let created = match fs::symlink_metadata(runtime) {
        Ok(_) => {
            fsutil::private_dir(runtime)?;
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(runtime)?;
            #[cfg(windows)]
            {
                crate::platform::own_new(runtime)?;
                crate::platform::private_permissions(runtime, true)?;
            }
            true
        }
        Err(error) => return Err(error.into()),
    };
    let marker = runtime.join(MARKER_FILE);
    match fs::symlink_metadata(&marker) {
        Ok(_) => validate_marker(runtime)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && created => {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&marker)
                .with_context(|| format!("create {}", marker.display()))?;
            #[cfg(windows)]
            {
                crate::platform::own_new(&marker)?;
                crate::platform::private_permissions(&marker, false)?;
            }
            file.write_all(MARKER_CONTENTS)?;
            file.sync_all()?;
            #[cfg(unix)]
            File::open(runtime)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
            "{} already exists without the xswap managed-runtime marker; move or remove it explicitly before starting seamless mode",
            runtime.display()
        ),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn auth_lock(runtime: &Path) -> Result<File> {
    ensure_marker(runtime)?;
    let path = runtime.join(AUTH_LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(_) => ensure_private_file(&path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    fsutil::lock(&path, true, false)
        .context("managed Codext credentials are being refreshed; retry after the current request")
}

fn runtime_auth(store: &Store) -> Result<RuntimeAuth> {
    let runtime = home(store);
    let _lock = auth_lock(&runtime)?;
    let (document, identity) = auth::credentials(&runtime)
        .context("no active seamless session login; run xswap session first")?;
    let account = store
        .account_for_identity(&identity)?
        .context("the seamless runtime login is not a registered xswap account")?;
    require_registered_identity(account.number, &account.identity)?;
    Ok(RuntimeAuth {
        account,
        document,
        home: runtime,
    })
}

pub(crate) fn snapshot(store: &Store) -> Result<RuntimeAuth> {
    runtime_auth(store)
}

pub(crate) fn status_account(store: &Store) -> Result<Option<(Account, PathBuf)>> {
    let runtime = home(store);
    match fs::symlink_metadata(&runtime) {
        Ok(_) => {
            validate_runtime(&runtime)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let Some(identity) = auth::identity(&runtime)? else {
        return Ok(None);
    };
    let account = store
        .account_for_identity(&identity)?
        .context("the seamless runtime login is not a registered xswap account")?;
    Ok(Some((account, runtime)))
}

fn configured_binary(store: &Store) -> Result<VerifiedBinary> {
    let binary = OsString::from(
        store
            .data
            .preferences
            .seamless_codext_bin
            .as_deref()
            .context(
                "seamless sessions require an absolute patched Codext path; set seamless-codext-bin",
            )?,
    );
    let configured_path = Path::new(&binary);
    if !configured_path.is_absolute() {
        bail!(
            "seamless sessions require an absolute pinned Codext path; set seamless-codext-bin and seamless-codext-sha256"
        );
    }
    let expected = store
        .data
        .preferences
        .seamless_codext_sha256
        .as_deref()
        .context("seamless sessions require a pinned Codext digest; set seamless-codext-sha256")?;
    let path = crate::platform::resolve_codex_binary(configured_path.as_os_str())?;
    #[cfg(not(windows))]
    let mut file = File::open(&path)
        .with_context(|| format!("open configured Codext binary {}", path.display()))?;
    #[cfg(windows)]
    let mut file = {
        use std::os::windows::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
        options
            .open(&path)
            .with_context(|| format!("open configured Codext binary {}", path.display()))?
    };
    if !file.metadata()?.is_file() {
        bail!("configured Codext path is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o222 != 0 {
            bail!(
                "configured Codext binary must have no Unix write bits; install the verified pinned artifact with mode 0555"
            );
        }
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        bail!(
            "configured Codext binary digest does not match seamless-codext-sha256; update the explicit pin only after verifying the patched Codext build"
        );
    }
    #[cfg(unix)]
    let command = crate::platform::pinned_binary_path(&file)?;
    #[cfg(windows)]
    let command = crate::platform::pinned_binary_path(&file, &path)?;
    Ok(VerifiedBinary {
        command,
        _file: file,
    })
}

fn seed_runtime(store: &Store, identifier: Option<&str>, runtime: &Path) -> Result<Account> {
    let selected = store
        .selected_for_run(identifier)?
        .context("no saved account; register one before starting a seamless session")?;
    require_registered_identity(selected.number, &selected.identity)?;
    let live = store.observe_live_account();
    let effective = store.effective_account(&selected, live.as_ref());
    let _source_lease = store.lease(&effective.home, false)?;
    let (document, identity) = auth::verified_credentials(&effective.home, &selected.identity)?;
    let _lock = auth_lock(runtime)?;
    if let Some(bytes) = fsutil::optional_bytes(&runtime.join("auth.json"))? {
        let (_, active_identity) = auth::credentials_from_bytes(&bytes)?;
        return store
            .account_for_identity(&active_identity)?
            .context("the seamless runtime login is not a registered xswap account");
    }
    if !require_registered_identity(selected.number, &selected.identity)?.same_owner(&identity) {
        bail!("selected account identity changed before seamless runtime initialization");
    }
    fsutil::atomic_json(&runtime.join("auth.json"), &document)?;
    Ok(selected)
}

fn prepare_runtime(
    store: &Store,
    identifier: Option<&str>,
    args: &[OsString],
) -> Result<(PathBuf, Account)> {
    let runtime = home(store);
    ensure_marker(&runtime)?;
    sharing::config(&store.data.main_home, &runtime)?;
    config_assets::share(&store.data.main_home, &runtime, args)?;
    sharing::history(&store.data.main_home, &runtime)?;
    let account = if runtime.join("auth.json").exists() {
        let active = runtime_auth(store)?.account;
        if let Some(identifier) = identifier {
            let requested = store.resolve(identifier)?;
            if requested.number != active.number {
                bail!(
                    "the seamless runtime currently uses account {}; let xswap auto --seamless rotate it or remove the runtime after all seamless sessions exit",
                    active.number
                );
            }
        }
        active
    } else {
        seed_runtime(store, identifier, &runtime)?
    };
    Ok((runtime, account))
}

fn option_before_literal_args(args: &[OsString], option: &str) -> bool {
    args.iter()
        .take_while(|argument| *argument != "--")
        .any(|argument| {
            argument == option
                || argument
                    .to_str()
                    .is_some_and(|argument| argument.starts_with(&format!("{option}=")))
        })
}

fn app_server_escape(args: &[OsString]) -> bool {
    let mut app_server = false;
    for argument in args.iter().take_while(|argument| *argument != "--") {
        if !app_server {
            app_server = argument == "app-server";
        } else if matches!(argument.to_str(), Some("daemon" | "proxy")) {
            return true;
        }
    }
    false
}

fn session_args(args: &[OsString]) -> Result<Vec<OsString>> {
    let command = config_assets::subcommand(args);
    if command.is_some_and(|command| {
        matches!(
            command,
            "exec"
                | "e"
                | "x"
                | "review"
                | "agents"
                | "remote-control"
                | "queue"
                | "archive"
                | "delete"
                | "unarchive"
        )
    }) {
        bail!(
            "xswap session supports the embedded interactive Codext TUI, resume/fork, and foreground app-server; this command can bypass the managed runtime lifecycle"
        );
    }
    if option_before_literal_args(args, "--remote")
        || option_before_literal_args(args, "--remote-auth-token-env")
    {
        bail!("xswap session does not allow remote app-server attachment");
    }
    if command == Some("app-server") && app_server_escape(args) {
        bail!("xswap session does not allow app-server daemon or proxy attachment");
    }
    let mut forwarded = Vec::with_capacity(args.len() + 1);
    if command != Some("app-server") && !option_before_literal_args(args, "--no-daemon") {
        forwarded.push(OsString::from("--no-daemon"));
    }
    forwarded.extend_from_slice(args);
    Ok(forwarded)
}

pub fn run(cli: &crate::cli::Cli, identifier: Option<&str>, args: &[OsString]) -> Result<()> {
    let forwarded = session_args(args)?;
    let store = Store::open_with_stop_policy(cli, crate::cli::StopCodex::Never)?;
    let binary = configured_binary(&store)?;
    launch::check_overrides(args, true)?;
    let runtime = home(&store);
    let lease = store.lease(&runtime, false)?;
    let (runtime, account) = prepare_runtime(&store, identifier, args)?;
    auth::verify(&runtime, &account.identity)?;
    let mut command = launch::command(&binary.command, &runtime, true)?;
    let sqlite = toml::Value::String(runtime.to_string_lossy().into_owned()).to_string();
    command.args(["-c", &format!("sqlite_home={sqlite}")]);
    command.env("CODEX_SQLITE_HOME", &runtime);
    command.args(forwarded);
    crate::platform::keep_lease_across_exec(&lease)?;
    drop(store);
    let result = crate::platform::execute(command, lease);
    drop(binary);
    result
}

pub(crate) fn active_login_guard(
    store: &Store,
    account: &Account,
) -> Result<Option<RuntimeLoginGuard>> {
    let runtime = home(store);
    match fs::symlink_metadata(&runtime) {
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let lease = store.lease(&runtime, false)?;
    let lock = auth_lock(&runtime)?;
    let Some(identity) = auth::identity(&runtime)? else {
        return Ok(None);
    };
    if !account
        .identity
        .as_ref()
        .is_some_and(|saved| saved.same_owner(&identity))
    {
        return Ok(None);
    }
    Ok(Some(RuntimeLoginGuard {
        path: runtime.join("auth.json"),
        _lease: lease,
        _auth_lock: lock,
    }))
}

pub(crate) fn activate_guarded(cli: &crate::cli::Cli, guard: &ActivationGuard) -> Result<()> {
    let store = Store::open_with_stop_policy(cli, crate::cli::StopCodex::Never)?;
    let runtime = home(&store);
    let _runtime_lease = store.lease(&runtime, false)?;
    let _auth_lock = auth_lock(&runtime)?;
    let runtime_before = fsutil::optional_bytes(&runtime.join("auth.json"))?
        .context("the seamless runtime login disappeared before activation")?;
    let (_, source_identity) = auth::credentials_from_bytes(&runtime_before)?;
    validate_activation_guard(&store, &source_identity, guard)?;
    let source = store
        .account_for_identity(&source_identity)?
        .context("the seamless runtime login is not registered")?;
    let target = store.resolve(&guard.target_number.to_string())?;
    if source.number == target.number {
        bail!("the seamless activation target is already active");
    }
    let runtime = fsutil::absolute(&runtime)?;
    for (role, saved_home) in [("source", &source.home), ("target", &target.home)] {
        if saved_home == &store.data.main_home || saved_home == &runtime {
            bail!(
                "seamless activation refused: the {role} account does not have an isolated saved home"
            );
        }
    }
    if source.home == target.home {
        bail!("seamless activation requires distinct saved account homes");
    }
    let mut homes = [source.home.clone(), target.home.clone()];
    homes.sort();
    let _saved_leases = homes
        .iter()
        .map(|home| store.lease(home, true))
        .collect::<Result<Vec<_>>>()?;
    let source_saved_before = fsutil::optional_bytes(&source.home.join("auth.json"))?;
    let target_before = fsutil::optional_bytes(&target.home.join("auth.json"))?
        .context("the seamless target login disappeared before activation")?;
    let (_, target_identity) = auth::credentials_from_bytes(&target_before)?;
    if !guard.target.same_owner(&target_identity) {
        bail!("the seamless target credentials changed identity before activation");
    }
    if fsutil::optional_bytes(&runtime.join("auth.json"))?.as_deref()
        != Some(runtime_before.as_slice())
        || fsutil::optional_bytes(&target.home.join("auth.json"))?.as_deref()
            != Some(target_before.as_slice())
        || fsutil::optional_bytes(&source.home.join("auth.json"))? != source_saved_before
    {
        bail!("credentials changed during seamless activation; retrying from a fresh snapshot");
    }
    fsutil::atomic_bytes(&source.home.join("auth.json"), &runtime_before)?;
    if fsutil::optional_bytes(&runtime.join("auth.json"))?.as_deref()
        != Some(runtime_before.as_slice())
        || fsutil::optional_bytes(&target.home.join("auth.json"))?.as_deref()
            != Some(target_before.as_slice())
    {
        bail!("credentials changed during seamless activation; retrying from a fresh snapshot");
    }
    fsutil::atomic_bytes(&runtime.join("auth.json"), &target_before)
}

#[cfg(test)]
mod tests;
