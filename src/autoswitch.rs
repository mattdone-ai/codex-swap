use crate::{
    account_state::{self, ActivationGuard},
    auth,
    cli::{Auto, Cli},
    fsutil,
    store::{AutoswitchPreferences, Store, require_registered_identity},
    usage_client,
    usage_model::{self, Usage},
};
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

const STATE_FILE: &str = "autoswitch-state.json";
const LOCK_FILE: &str = ".auto.lock";

#[derive(Clone)]
struct Target {
    number: u32,
    home: PathBuf,
    identity: auth::Identity,
}

struct Snapshot {
    settings: AutoswitchPreferences,
    source: auth::Identity,
    active: u32,
    targets: Vec<Target>,
    setup_errors: BTreeMap<u32, String>,
    _leases: Vec<File>,
}

#[derive(Clone)]
struct Sample {
    number: u32,
    identity: auth::Identity,
    fetched_at: i64,
    usage: Option<Usage>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Metrics {
    runway: f64,
    raw_headroom: f64,
    hard_limit: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    #[serde(default)]
    active: Option<u32>,
    #[serde(default)]
    unhealthy_ticks: u32,
    #[serde(default)]
    last_switch_at: Option<i64>,
    #[serde(default)]
    last_from: Option<u32>,
    #[serde(default)]
    last_to: Option<u32>,
    #[serde(default)]
    backoff_until: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Event<'a> {
    schema_version: u32,
    event: &'a str,
    ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thresholds: Option<BTreeMap<&'static str, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_percent: Option<BTreeMap<u32, BTreeMap<String, f64>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    errors: Option<BTreeMap<u32, String>>,
    #[serde(skip_serializing_if = "is_false")]
    dry_run: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl<'a> Event<'a> {
    fn new(event: &'a str) -> Self {
        Self {
            schema_version: 1,
            event,
            ts: usage_model::timestamp(Utc::now()),
            reason: None,
            trigger: None,
            active: None,
            target: None,
            detail: None,
            retry_after_seconds: None,
            thresholds: None,
            usage_percent: None,
            errors: None,
            dry_run: false,
        }
    }
}

fn emit(args: &Auto, event: &Event<'_>) -> Result<()> {
    if args.json {
        serde_json::to_writer(std::io::stdout().lock(), event)?;
        println!();
        return Ok(());
    }
    match event.event {
        "poll" => println!(
            "Checked Codex quota usage for account {}.",
            event.active.unwrap_or(0)
        ),
        "switch" => println!(
            "{} account {} to {} ({}).",
            if event.dry_run {
                "Would switch"
            } else {
                "Switched"
            },
            event.active.unwrap_or(0),
            event.target.unwrap_or(0),
            event.trigger.unwrap_or("quota policy")
        ),
        _ => println!(
            "No switch: {}{}",
            event.reason.unwrap_or(event.event),
            event
                .detail
                .as_deref()
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default()
        ),
    }
    Ok(())
}

fn state(path: &Path) -> Result<State> {
    let Some(bytes) = fsutil::optional_bytes(path)? else {
        return Ok(State::default());
    };
    serde_json::from_slice(&bytes).context("invalid autoswitch state")
}

fn save_state(path: &Path, state: &State) -> Result<()> {
    fsutil::atomic_json(path, state)
}

fn record_switch(state: &mut State, dry_run: bool, now: i64, from: u32, to: u32) {
    if dry_run {
        return;
    }
    state.last_switch_at = Some(now);
    state.last_from = Some(from);
    state.last_to = Some(to);
    state.active = Some(to);
    state.unhealthy_ticks = 0;
}

fn target_descriptors(
    store: &Store,
    active: &crate::store::Account,
) -> (Vec<Target>, BTreeMap<u32, String>) {
    let mut targets = Vec::new();
    let mut errors = BTreeMap::new();
    for account in store.data.accounts.iter().filter(|account| account.enabled) {
        let identity = match require_registered_identity(account.number, &account.identity) {
            Ok(identity) => identity.clone(),
            Err(error) => {
                errors.insert(account.number, format!("{error:#}"));
                continue;
            }
        };
        let effective = store.effective_account(account, Some(active));
        targets.push(Target {
            number: account.number,
            home: effective.home,
            identity,
        });
    }
    (targets, errors)
}

fn snapshot(cli: &Cli) -> Result<Snapshot> {
    let store = Store::open_with_stop_policy(cli, crate::cli::StopCodex::Never)?;
    let source = auth::identity(&store.data.main_home)?
        .context("no active Codex login; register and activate an account before autoswitch")?;
    let active = store
        .account_for_identity(&source)?
        .context("the active Codex login is not registered; import or add it before autoswitch")?;
    let (targets, setup_errors) = target_descriptors(&store, &active);
    let mut homes = BTreeSet::new();
    homes.extend(targets.iter().map(|target| target.home.clone()));
    let leases = homes
        .iter()
        .map(|home| store.lease(home, false))
        .collect::<Result<Vec<_>>>()?;
    Ok(Snapshot {
        settings: store.data.preferences.autoswitch.clone(),
        source,
        active: active.number,
        targets,
        setup_errors,
        _leases: leases,
    })
}

fn collect(snapshot: &Snapshot) -> Result<(Vec<Sample>, Option<i64>)> {
    let client = usage_client::client()?;
    let mut samples = Vec::new();
    let mut backoff_until = None;
    for target in &snapshot.targets {
        match usage_client::fetch(&client, &target.home, &Some(target.identity.clone())) {
            Ok((identity, response)) => {
                let fetched_at = Utc::now();
                match usage_model::parse(&response, fetched_at) {
                    Ok(usage) => samples.push(Sample {
                        number: target.number,
                        identity,
                        fetched_at: fetched_at.timestamp(),
                        usage: Some(usage),
                        error: None,
                    }),
                    Err(error) => samples.push(Sample {
                        number: target.number,
                        identity,
                        fetched_at: fetched_at.timestamp(),
                        usage: None,
                        error: Some(format!("{error:#}")),
                    }),
                }
            }
            Err(error) => {
                let failed_at = Utc::now().timestamp();
                if let Some(rate) = error.downcast_ref::<usage_client::RateLimited>() {
                    let deadline = failed_at.saturating_add(rate.retry_after_seconds as i64);
                    backoff_until =
                        Some(backoff_until.map_or(deadline, |until: i64| until.max(deadline)));
                }
                samples.push(Sample {
                    number: target.number,
                    identity: target.identity.clone(),
                    fetched_at: failed_at,
                    usage: None,
                    error: Some(format!("{error:#}")),
                });
            }
        }
    }
    Ok((samples, backoff_until))
}

fn threshold(settings: &AutoswitchPreferences, seconds: i64) -> Option<f64> {
    match seconds {
        18_000 => Some(settings.five_hour_threshold),
        604_800 => Some(settings.seven_day_threshold),
        _ => None,
    }
}

fn metrics(sample: &Sample, settings: &AutoswitchPreferences, now: i64) -> Option<Metrics> {
    if sample.error.is_some()
        || now.saturating_sub(sample.fetched_at) > (settings.interval_seconds * 2) as i64
    {
        return None;
    }
    let usage = sample.usage.as_ref()?;
    let configured: BTreeSet<_> = settings
        .supplementary_scopes
        .iter()
        .map(String::as_str)
        .collect();
    let mut seen = BTreeSet::new();
    let mut runway = f64::INFINITY;
    let mut headroom = f64::INFINITY;
    let mut hard = usage.allowed == Some(false) || usage.limit_reached == Some(true);
    for window in &usage.windows {
        let selected = window.scope == "codex" || configured.contains(window.scope.as_str());
        if !selected {
            continue;
        }
        let limit = window
            .window_seconds
            .and_then(|seconds| threshold(settings, seconds))?;
        if !window.used_percent.is_finite() {
            return None;
        }
        seen.insert(window.scope.as_str());
        runway = runway.min(limit - window.used_percent);
        headroom = headroom.min(100.0 - window.used_percent);
        hard |= window.used_percent >= 100.0;
    }
    if !seen.contains("codex") || !configured.iter().all(|scope| seen.contains(scope)) {
        return None;
    }
    Some(Metrics {
        runway,
        raw_headroom: headroom,
        hard_limit: hard,
    })
}

fn reported_hard_limit(sample: &Sample, settings: &AutoswitchPreferences, now: i64) -> bool {
    sample.error.is_none()
        && now.saturating_sub(sample.fetched_at) <= (settings.interval_seconds * 2) as i64
        && sample
            .usage
            .as_ref()
            .is_some_and(|usage| usage.allowed == Some(false) || usage.limit_reached == Some(true))
}

fn best_candidate(
    samples: &[Sample],
    active: u32,
    settings: &AutoswitchPreferences,
    now: i64,
    trigger: &str,
    active_metrics: Option<Metrics>,
) -> Option<u32> {
    let mut candidates: Vec<_> = samples
        .iter()
        .filter(|sample| sample.number != active)
        .filter_map(|sample| metrics(sample, settings, now).map(|metrics| (sample.number, metrics)))
        .filter(|(_, candidate)| !candidate.hard_limit)
        .filter(|(_, candidate)| match trigger {
            "at-limit" => candidate.raw_headroom > 0.0,
            "failover" => candidate.runway > 0.0 && candidate.runway >= settings.hysteresis_percent,
            _ => {
                candidate.runway > 0.0
                    && active_metrics.is_some_and(|active| candidate.runway > active.runway)
            }
        })
        .collect();
    candidates.sort_by(|(left_number, left), (right_number, right)| {
        right
            .runway
            .total_cmp(&left.runway)
            .then_with(|| left_number.cmp(right_number))
    });
    candidates.first().map(|(number, _)| *number)
}

fn poll_event(snapshot: &Snapshot, samples: &[Sample]) -> Event<'static> {
    let mut event = Event::new("poll");
    event.active = Some(snapshot.active);
    event.thresholds = Some(BTreeMap::from([
        ("fiveHour", snapshot.settings.five_hour_threshold),
        ("sevenDay", snapshot.settings.seven_day_threshold),
    ]));
    let usage = samples
        .iter()
        .filter_map(|sample| {
            let windows = sample.usage.as_ref()?.windows.iter().map(|window| {
                let key = format!(
                    "{}:{}",
                    window.scope,
                    window
                        .window_seconds
                        .map_or_else(|| window.kind.to_owned(), |seconds| seconds.to_string())
                );
                (key, window.used_percent)
            });
            Some((sample.number, windows.collect()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut errors = snapshot.setup_errors.clone();
    errors.extend(
        samples
            .iter()
            .filter_map(|sample| sample.error.clone().map(|error| (sample.number, error))),
    );
    event.usage_percent = (!usage.is_empty()).then_some(usage);
    event.errors = (!errors.is_empty()).then_some(errors);
    event
}

fn retry_delay(backoff_until: Option<i64>, now: i64) -> u64 {
    backoff_until
        .map(|until| until.saturating_sub(now).max(0) as u64)
        .unwrap_or(0)
}

fn tick(cli: &Cli, args: &Auto, state_path: &Path) -> Result<u64> {
    let now = Utc::now().timestamp();
    let mut saved = state(state_path)?;
    if let Some(until) = saved.backoff_until.filter(|until| *until > now) {
        let mut event = Event::new("no-switch");
        event.reason = Some("rate-limit-backoff");
        event.retry_after_seconds = Some((until - now) as u64);
        emit(args, &event)?;
        return Ok((until - now) as u64);
    }

    let snapshot = snapshot(cli)?;
    if saved.active != Some(snapshot.active) {
        saved.active = Some(snapshot.active);
        saved.unhealthy_ticks = 0;
    }
    let (samples, backoff_until) = collect(&snapshot)?;
    let evaluated_at = Utc::now().timestamp();
    emit(args, &poll_event(&snapshot, &samples))?;
    saved.backoff_until = backoff_until;
    let retry_after = retry_delay(backoff_until, evaluated_at);

    let active_sample = samples
        .iter()
        .find(|sample| sample.number == snapshot.active);
    let active_metrics =
        active_sample.and_then(|sample| metrics(sample, &snapshot.settings, evaluated_at));
    let trigger = if active_sample
        .is_some_and(|sample| reported_hard_limit(sample, &snapshot.settings, evaluated_at))
    {
        saved.unhealthy_ticks = 0;
        Some("at-limit")
    } else if let Some(active) = active_metrics {
        saved.unhealthy_ticks = 0;
        if active.hard_limit {
            Some("at-limit")
        } else if active.runway <= 0.0 {
            Some("proactive")
        } else {
            None
        }
    } else {
        saved.unhealthy_ticks = saved.unhealthy_ticks.saturating_add(1);
        (saved.unhealthy_ticks >= snapshot.settings.unhealthy_ticks).then_some("failover")
    };

    let Some(trigger) = trigger else {
        let mut event = Event::new("no-switch");
        event.active = Some(snapshot.active);
        if active_metrics.is_some() {
            event.reason = Some("below-threshold");
        } else {
            event.reason = Some("active-usage-unavailable");
            event.detail = Some(format!(
                "unhealthy sample {}/{}",
                saved.unhealthy_ticks, snapshot.settings.unhealthy_ticks
            ));
        }
        save_state(state_path, &saved)?;
        emit(args, &event)?;
        return Ok(retry_after.max(snapshot.settings.interval_seconds));
    };

    let in_cooldown = saved.last_switch_at.is_some_and(|last| {
        evaluated_at.saturating_sub(last) < snapshot.settings.cooldown_seconds as i64
    });
    if trigger != "at-limit" && in_cooldown {
        let mut event = Event::new("no-switch");
        event.active = Some(snapshot.active);
        event.reason = Some("cooldown");
        save_state(state_path, &saved)?;
        emit(args, &event)?;
        return Ok(retry_after.max(snapshot.settings.interval_seconds));
    }

    let Some(target_number) = best_candidate(
        &samples,
        snapshot.active,
        &snapshot.settings,
        evaluated_at,
        trigger,
        active_metrics,
    ) else {
        let mut event = Event::new("no-switch");
        event.active = Some(snapshot.active);
        event.reason = Some(if trigger == "failover" {
            "no-safe-failover-target"
        } else {
            "no-qualifying-candidate"
        });
        event.trigger = Some(trigger);
        save_state(state_path, &saved)?;
        emit(args, &event)?;
        return Ok(retry_after.max(snapshot.settings.interval_seconds));
    };

    let target = samples
        .iter()
        .find(|sample| sample.number == target_number)
        .context("autoswitch target sample disappeared")?;
    let guard = ActivationGuard {
        source: snapshot.source.clone(),
        target: target.identity.clone(),
        target_number,
        autoswitch: snapshot.settings.clone(),
    };
    let active = snapshot.active;
    let interval = snapshot.settings.interval_seconds;
    drop(snapshot);

    let mut event = Event::new("switch");
    event.active = Some(active);
    event.target = Some(target_number);
    event.trigger = Some(trigger);
    event.dry_run = args.dry_run;
    if !args.dry_run {
        if let Err(error) = account_state::select_global_guarded(cli, &guard) {
            event.event = "no-switch";
            event.reason = Some(if format!("{error:#}").contains("Codex is running") {
                "blocked-running-codex"
            } else {
                "activation-revalidation-failed"
            });
            event.detail = Some(format!("{error:#}"));
            save_state(state_path, &saved)?;
            emit(args, &event)?;
            return Ok(retry_after.max(interval));
        }
    }
    record_switch(
        &mut saved,
        args.dry_run,
        Utc::now().timestamp(),
        active,
        target_number,
    );
    save_state(state_path, &saved)?;
    emit(args, &event)?;
    Ok(retry_after.max(interval))
}

pub fn run(cli: &Cli, args: &Auto) -> Result<()> {
    let initial = Store::open_with_stop_policy(cli, crate::cli::StopCodex::Never)?;
    let root = initial.root.clone();
    drop(initial);
    let _singleton = fsutil::lock(&root.join(LOCK_FILE), true, false)
        .context("another xswap auto process is already running")?;
    let state_path = root.join(STATE_FILE);
    loop {
        let delay = match tick(cli, args, &state_path) {
            Ok(delay) => delay,
            Err(error) => {
                let mut event = Event::new("error");
                event.reason = Some("poll-failed");
                event.detail = Some(format!("{error:#}"));
                emit(args, &event)?;
                let settings = Store::open_with_stop_policy(cli, crate::cli::StopCodex::Never)?
                    .data
                    .preferences
                    .autoswitch;
                settings.interval_seconds
            }
        };
        if args.once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(delay.max(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Account;
    use crate::usage_model::Window;

    fn identity(number: u32) -> auth::Identity {
        auth::Identity {
            account_id: format!("workspace-{number}"),
            user_id: Some(format!("user-{number}")),
            email: Some(format!("user-{number}@example.test")),
            plan: None,
            legacy_hint_unusable: false,
        }
    }

    fn sample(number: u32, windows: &[(&str, i64, f64)], fetched_at: i64) -> Sample {
        Sample {
            number,
            identity: identity(number),
            fetched_at,
            usage: Some(Usage {
                plan: None,
                allowed: Some(true),
                limit_reached: Some(false),
                windows: windows
                    .iter()
                    .map(|(scope, seconds, used)| Window {
                        scope: (*scope).into(),
                        kind: if *seconds == 604_800 {
                            "weekly"
                        } else {
                            "session"
                        },
                        used_percent: *used,
                        remaining_percent: 100.0 - used,
                        window_seconds: Some(*seconds),
                        resets_at: None,
                        resets_at_epoch_seconds: None,
                        reset_after_seconds: None,
                        pacing: None,
                    })
                    .collect(),
                credits: None,
                warnings: vec![],
            }),
            error: None,
        }
    }

    #[test]
    fn five_hour_boundary_and_weekly_primary_are_independent() {
        let settings = AutoswitchPreferences::default();
        let five = sample(1, &[("codex", 18_000, 94.0), ("codex", 604_800, 20.0)], 100);
        let weekly = sample(2, &[("codex", 604_800, 98.0)], 100);
        assert_eq!(metrics(&five, &settings, 100).unwrap().runway, 0.0);
        assert_eq!(metrics(&weekly, &settings, 100).unwrap().runway, 0.0);
    }

    #[test]
    fn supplementary_scopes_are_opt_in_and_code_review_is_excluded() {
        let windows = [
            ("codex", 18_000, 10.0),
            ("model-x", 604_800, 99.0),
            ("code_review", 604_800, 100.0),
        ];
        let value = sample(1, &windows, 100);
        let settings = AutoswitchPreferences::default();
        assert_eq!(metrics(&value, &settings, 100).unwrap().runway, 84.0);
        let mut selected = settings.clone();
        selected.supplementary_scopes = vec!["model-x".into()];
        assert_eq!(metrics(&value, &selected, 100).unwrap().runway, -1.0);
    }

    #[test]
    fn unknown_stale_and_nonfinite_values_are_never_healthy() {
        let settings = AutoswitchPreferences::default();
        let mut unknown = sample(1, &[("other", 18_000, 1.0)], 100);
        assert!(metrics(&unknown, &settings, 100).is_none());
        unknown = sample(1, &[("codex", 18_000, 1.0)], -1000);
        assert!(metrics(&unknown, &settings, 100).is_none());
        unknown.usage.as_mut().unwrap().windows[0].used_percent = f64::NAN;
        assert!(metrics(&unknown, &settings, 100).is_none());
    }

    #[test]
    fn valid_window_cannot_mask_unknown_or_nonfinite_binding_window() {
        let settings = AutoswitchPreferences::default();
        let mut value = sample(1, &[("codex", 18_000, 10.0), ("codex", 604_800, 20.0)], 100);
        value.usage.as_mut().unwrap().windows[1].window_seconds = None;
        assert!(metrics(&value, &settings, 100).is_none());

        value.usage.as_mut().unwrap().windows[1].window_seconds = Some(604_800);
        value.usage.as_mut().unwrap().windows[1].used_percent = f64::NAN;
        assert!(metrics(&value, &settings, 100).is_none());
    }

    #[test]
    fn ranking_uses_minimum_threshold_runway_and_failover_hysteresis() {
        let settings = AutoswitchPreferences::default();
        let samples = vec![
            sample(1, &[("codex", 18_000, 94.0)], 100),
            sample(2, &[("codex", 18_000, 80.0), ("codex", 604_800, 95.0)], 100),
            sample(3, &[("codex", 18_000, 70.0), ("codex", 604_800, 70.0)], 100),
        ];
        assert_eq!(
            best_candidate(
                &samples,
                1,
                &settings,
                100,
                "proactive",
                metrics(&samples[0], &settings, 100)
            ),
            Some(3)
        );
        assert_eq!(
            best_candidate(&samples, 1, &settings, 100, "failover", None),
            Some(3)
        );
    }

    #[test]
    fn proactive_switch_accepts_any_strictly_better_safe_runway() {
        let settings = AutoswitchPreferences::default();
        let samples = vec![
            sample(1, &[("codex", 18_000, 94.0), ("codex", 604_800, 20.0)], 100),
            sample(2, &[("codex", 18_000, 20.0), ("codex", 604_800, 97.0)], 100),
        ];
        assert_eq!(
            best_candidate(
                &samples,
                1,
                &settings,
                100,
                "proactive",
                metrics(&samples[0], &settings, 100)
            ),
            Some(2)
        );
    }

    #[test]
    fn zero_margin_failover_still_requires_positive_runway() {
        let settings = AutoswitchPreferences {
            hysteresis_percent: 0.0,
            ..AutoswitchPreferences::default()
        };
        let samples = vec![
            sample(1, &[("codex", 18_000, 94.0)], 100),
            sample(2, &[("codex", 18_000, 94.0)], 100),
        ];
        assert_eq!(
            best_candidate(&samples, 1, &settings, 100, "failover", None),
            None
        );
        let safe = vec![
            samples[0].clone(),
            sample(2, &[("codex", 18_000, 93.0)], 100),
        ];
        assert_eq!(
            best_candidate(&safe, 1, &settings, 100, "failover", None),
            Some(2)
        );
    }

    #[test]
    fn hard_limit_escape_accepts_positive_raw_headroom() {
        let settings = AutoswitchPreferences::default();
        let samples = vec![
            sample(1, &[("codex", 18_000, 100.0)], 100),
            sample(2, &[("codex", 18_000, 99.0)], 100),
        ];
        assert_eq!(
            best_candidate(&samples, 1, &settings, 100, "at-limit", None),
            Some(2)
        );
    }

    #[test]
    fn hard_limit_escape_rejects_blocked_targets_and_ranks_by_threshold_runway() {
        let settings = AutoswitchPreferences::default();
        let mut blocked = sample(2, &[("codex", 18_000, 0.0)], 100);
        blocked.usage.as_mut().unwrap().allowed = Some(false);
        let samples = vec![
            sample(1, &[("codex", 18_000, 100.0)], 100),
            blocked,
            // Raw headroom 10, threshold runway 4.
            sample(3, &[("codex", 18_000, 90.0), ("codex", 604_800, 0.0)], 100),
            // Raw headroom 9, threshold runway 7: this is the safer target.
            sample(4, &[("codex", 18_000, 0.0), ("codex", 604_800, 91.0)], 100),
        ];
        assert_eq!(
            best_candidate(&samples, 1, &settings, 100, "at-limit", None),
            Some(4)
        );
    }

    #[test]
    fn retry_delay_is_measured_from_the_rate_limit_receipt_deadline() {
        assert_eq!(retry_delay(Some(160), 100), 60);
        assert_eq!(retry_delay(Some(160), 130), 30);
        assert_eq!(retry_delay(Some(160), 170), 0);
        assert_eq!(retry_delay(None, 100), 0);
    }

    #[test]
    fn cooldown_state_survives_serialization() {
        let state = State {
            last_switch_at: Some(42),
            last_from: Some(1),
            last_to: Some(2),
            ..State::default()
        };
        let restored: State = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
        assert_eq!(restored.last_switch_at, Some(42));
        assert_eq!(restored.last_to, Some(2));
    }

    #[test]
    fn dry_run_does_not_record_a_switch() {
        let mut state = State {
            active: Some(1),
            unhealthy_ticks: 2,
            ..State::default()
        };
        record_switch(&mut state, true, 100, 1, 2);
        assert_eq!(state.active, Some(1));
        assert_eq!(state.unhealthy_ticks, 2);
        assert_eq!(state.last_switch_at, None);
    }

    #[test]
    fn singleton_lock_refuses_a_second_engine() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".auto.lock");
        let first = fsutil::lock(&path, true, false).unwrap();
        let second = fsutil::lock(&path, true, false);
        assert!(second.is_err());
        drop(first);
        assert!(fsutil::lock(&path, true, false).is_ok());
    }

    #[test]
    fn incomplete_enabled_peer_is_reported_without_hiding_ready_targets() {
        let directory = tempfile::tempdir().unwrap();
        let cli = Cli {
            data_dir: Some(directory.path().join("data")),
            codex_home: Some(directory.path().join("main")),
            codex_bin: None,
            stop_codex: false,
            command: crate::cli::Action::List(crate::cli::Output { json: false }),
        };
        let mut store = Store::open(&cli).unwrap();
        let ready = Account {
            number: 1,
            alias: Some("ready".into()),
            home: directory.path().join("ready"),
            managed: true,
            share_history: false,
            identity: Some(identity(1)),
            enabled: true,
        };
        let incomplete = Account {
            number: 2,
            alias: Some("incomplete".into()),
            home: directory.path().join("incomplete"),
            managed: true,
            share_history: false,
            identity: None,
            enabled: true,
        };
        store.data.accounts = vec![ready.clone(), incomplete];
        let (targets, errors) = target_descriptors(&store, &ready);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].number, 1);
        assert!(errors.get(&2).unwrap().contains("setup is incomplete"));
    }
}
