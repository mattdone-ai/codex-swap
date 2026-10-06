use crate::{
    cli::{Cli, ConfigAction, Output},
    store::{AutoswitchPreferences, Store},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

fn value(store: &Store, key: &str) -> Result<Value> {
    match key {
        "codex-bin" => Ok(json!(
            store
                .data
                .preferences
                .codex_bin
                .as_deref()
                .unwrap_or("codex")
        )),
        "seamless-codext-sha256" => Ok(json!(
            store
                .data
                .preferences
                .seamless_codext_sha256
                .as_deref()
                .unwrap_or("")
        )),
        "seamless-codext-bin" => Ok(json!(
            store
                .data
                .preferences
                .seamless_codext_bin
                .as_deref()
                .unwrap_or("")
        )),
        "default-account" => Ok(store
            .data
            .default
            .map_or_else(|| json!("default"), |n| json!(n.to_string()))),
        "autoswitch.five-hour-threshold" => {
            Ok(json!(store.data.preferences.autoswitch.five_hour_threshold))
        }
        "autoswitch.seven-day-threshold" => {
            Ok(json!(store.data.preferences.autoswitch.seven_day_threshold))
        }
        "autoswitch.interval-seconds" => {
            Ok(json!(store.data.preferences.autoswitch.interval_seconds))
        }
        "autoswitch.cooldown-seconds" => {
            Ok(json!(store.data.preferences.autoswitch.cooldown_seconds))
        }
        "autoswitch.hysteresis-percent" => {
            Ok(json!(store.data.preferences.autoswitch.hysteresis_percent))
        }
        "autoswitch.unhealthy-ticks" => {
            Ok(json!(store.data.preferences.autoswitch.unhealthy_ticks))
        }
        "autoswitch.supplementary-scopes" => Ok(json!(
            store
                .data
                .preferences
                .autoswitch
                .supplementary_scopes
                .join(",")
        )),
        _ => bail!(
            "unknown preference {key:?}; supported keys: codex-bin, seamless-codext-bin, seamless-codext-sha256, default-account, autoswitch.five-hour-threshold, autoswitch.seven-day-threshold, autoswitch.interval-seconds, autoswitch.cooldown-seconds, autoswitch.hysteresis-percent, autoswitch.unhealthy-ticks, autoswitch.supplementary-scopes"
        ),
    }
}

fn emit(value: Value, output: &Output) -> Result<()> {
    if output.json {
        serde_json::to_writer_pretty(
            std::io::stdout().lock(),
            &json!({"schemaVersion": 1, "config": value}),
        )?;
        println!();
    } else if let Some(object) = value.as_object() {
        for (key, value) in object {
            println!(
                "{key} = {}",
                value.as_str().map_or_else(
                    || value.to_string(),
                    |value| value.escape_default().to_string()
                )
            );
        }
    } else {
        println!(
            "{}",
            value.as_str().map_or_else(
                || value.to_string(),
                |value| value.escape_default().to_string()
            )
        );
    }
    Ok(())
}

pub fn configure(cli: &Cli, action: Option<&ConfigAction>, output: &Output) -> Result<()> {
    let mut store = Store::open(cli)?;
    match action.unwrap_or(&ConfigAction::List) {
        ConfigAction::List => emit(
            json!({
                "codex-bin": value(&store, "codex-bin")?,
                "seamless-codext-bin": value(&store, "seamless-codext-bin")?,
                "seamless-codext-sha256": value(&store, "seamless-codext-sha256")?,
                "default-account": value(&store, "default-account")?,
                "autoswitch.five-hour-threshold": value(&store, "autoswitch.five-hour-threshold")?,
                "autoswitch.seven-day-threshold": value(&store, "autoswitch.seven-day-threshold")?,
                "autoswitch.interval-seconds": value(&store, "autoswitch.interval-seconds")?,
                "autoswitch.cooldown-seconds": value(&store, "autoswitch.cooldown-seconds")?,
                "autoswitch.hysteresis-percent": value(&store, "autoswitch.hysteresis-percent")?,
                "autoswitch.unhealthy-ticks": value(&store, "autoswitch.unhealthy-ticks")?,
                "autoswitch.supplementary-scopes": value(&store, "autoswitch.supplementary-scopes")?
            }),
            output,
        ),
        ConfigAction::Path => emit(json!(store.root.join("accounts.json")), output),
        ConfigAction::Get { key } => emit(value(&store, key)?, output),
        ConfigAction::Set { key, value: new } => {
            match key.as_str() {
                "codex-bin" => {
                    if new.trim().is_empty() || new.contains('\0') {
                        bail!("codex-bin must be an executable name or path");
                    }
                    let path = std::path::Path::new(new);
                    let binary = if path.components().count() > 1 && !path.is_absolute() {
                        crate::fsutil::absolute(path)?
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        new.clone()
                    };
                    store.data.preferences.codex_bin = Some(binary);
                    store.save()?;
                }
                "seamless-codext-sha256" => {
                    let digest = new.trim().to_ascii_lowercase();
                    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        bail!("seamless-codext-sha256 must be exactly 64 hexadecimal characters");
                    }
                    store.data.preferences.seamless_codext_sha256 = Some(digest);
                }
                "seamless-codext-bin" => {
                    let path = std::path::Path::new(new);
                    if !path.is_absolute() || new.contains('\0') {
                        bail!("seamless-codext-bin must be an absolute executable path");
                    }
                    store.data.preferences.seamless_codext_bin = Some(
                        crate::fsutil::absolute(path)?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                "default-account" => {
                    drop(store);
                    crate::commands::select_global(cli, Some(new))?;
                    store = Store::open(cli)?;
                }
                "autoswitch.five-hour-threshold" => {
                    store.data.preferences.autoswitch.five_hour_threshold = new
                        .parse()
                        .context("five-hour threshold must be a number")?
                }
                "autoswitch.seven-day-threshold" => {
                    store.data.preferences.autoswitch.seven_day_threshold = new
                        .parse()
                        .context("seven-day threshold must be a number")?
                }
                "autoswitch.interval-seconds" => {
                    store.data.preferences.autoswitch.interval_seconds =
                        new.parse().context("interval seconds must be an integer")?
                }
                "autoswitch.cooldown-seconds" => {
                    store.data.preferences.autoswitch.cooldown_seconds =
                        new.parse().context("cooldown seconds must be an integer")?
                }
                "autoswitch.hysteresis-percent" => {
                    store.data.preferences.autoswitch.hysteresis_percent =
                        new.parse().context("hysteresis percent must be a number")?
                }
                "autoswitch.unhealthy-ticks" => {
                    store.data.preferences.autoswitch.unhealthy_ticks =
                        new.parse().context("unhealthy ticks must be an integer")?
                }
                "autoswitch.supplementary-scopes" => {
                    store.data.preferences.autoswitch.supplementary_scopes = new
                        .split(',')
                        .map(str::trim)
                        .filter(|scope| !scope.is_empty())
                        .map(str::to_owned)
                        .collect();
                }
                _ => {
                    value(&store, key)?;
                    unreachable!()
                }
            }
            store.data.preferences.autoswitch.validate()?;
            store.save()?;
            emit(value(&store, key)?, output)
        }
        ConfigAction::Unset { key } => {
            match key.as_str() {
                "codex-bin" => store.data.preferences.codex_bin = None,
                "seamless-codext-bin" => store.data.preferences.seamless_codext_bin = None,
                "seamless-codext-sha256" => store.data.preferences.seamless_codext_sha256 = None,
                "default-account" => {
                    drop(store);
                    crate::commands::select_global(cli, Some("default"))?;
                    store = Store::open(cli)?;
                }
                "autoswitch.five-hour-threshold" => {
                    store.data.preferences.autoswitch.five_hour_threshold =
                        AutoswitchPreferences::default().five_hour_threshold
                }
                "autoswitch.seven-day-threshold" => {
                    store.data.preferences.autoswitch.seven_day_threshold =
                        AutoswitchPreferences::default().seven_day_threshold
                }
                "autoswitch.interval-seconds" => {
                    store.data.preferences.autoswitch.interval_seconds =
                        AutoswitchPreferences::default().interval_seconds
                }
                "autoswitch.cooldown-seconds" => {
                    store.data.preferences.autoswitch.cooldown_seconds =
                        AutoswitchPreferences::default().cooldown_seconds
                }
                "autoswitch.hysteresis-percent" => {
                    store.data.preferences.autoswitch.hysteresis_percent =
                        AutoswitchPreferences::default().hysteresis_percent
                }
                "autoswitch.unhealthy-ticks" => {
                    store.data.preferences.autoswitch.unhealthy_ticks =
                        AutoswitchPreferences::default().unhealthy_ticks
                }
                "autoswitch.supplementary-scopes" => store
                    .data
                    .preferences
                    .autoswitch
                    .supplementary_scopes
                    .clear(),
                _ => {
                    value(&store, key)?;
                    unreachable!()
                }
            }
            store.save()?;
            emit(value(&store, key)?, output)
        }
    }
}
