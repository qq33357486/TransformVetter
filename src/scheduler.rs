use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::Settings;
use crate::profile::ModerationProfile;
use crate::sample_rpc::SampleRpcConfig;
use crate::training::{evaluate_training_need, fetch_training_sample_count_via_rpc};

const RUNNING_STATUS_STALE_SECS: u64 = 2 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainingSubprocessCommand {
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ScannedProfile {
    pub profile_name: String,
    pub profile: ModerationProfile,
}

#[derive(Debug, Clone)]
pub struct PlannedTrainingAction {
    pub profile_name: String,
    pub sample_count: usize,
    pub reason: &'static str,
}

pub fn list_profiles(root_dir: &Path) -> Result<Vec<String>> {
    let profiles_dir = root_dir.join("configs").join("mod_profiles");
    if !profiles_dir.exists() {
        return Ok(Vec::new());
    }

    let mut profiles = fs::read_dir(&profiles_dir)
        .with_context(|| format!("failed to read {}", profiles_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() {
                return None;
            }

            let profile_name = entry.file_name().to_string_lossy().into_owned();
            let profile_json = entry.path().join("profile.json");
            profile_json.exists().then_some(profile_name)
        })
        .collect::<Vec<_>>();
    profiles.sort();
    Ok(profiles)
}

pub fn scan_profiles(root_dir: &Path) -> Result<Vec<ScannedProfile>> {
    list_profiles(root_dir)?
        .into_iter()
        .map(|profile_name| {
            let profile = ModerationProfile::load(root_dir, &profile_name)?;
            Ok(ScannedProfile {
                profile_name,
                profile,
            })
        })
        .collect()
}

pub fn cooldown_allows_training(
    profile: &ModerationProfile,
    cooldown_minutes: u64,
    now_unix_secs: u64,
) -> Result<bool> {
    if cooldown_minutes == 0 {
        return Ok(true);
    }

    let Some(status) = profile.training_status() else {
        return Ok(true);
    };
    let Some(state) = status.get("status").and_then(|value| value.as_str()) else {
        return Ok(true);
    };
    if state != "failed" {
        return Ok(true);
    }

    let Some(timestamp) = status.get("timestamp").and_then(|value| value.as_u64()) else {
        return Ok(true);
    };

    let elapsed_secs = now_unix_secs.saturating_sub(timestamp);
    let cooldown_secs = Duration::from_secs(cooldown_minutes.saturating_mul(60)).as_secs();
    Ok(elapsed_secs >= cooldown_secs)
}

pub fn training_launch_allowed(profile: &ModerationProfile, now_unix_secs: u64) -> Result<bool> {
    let Some(status) = profile.training_status() else {
        return Ok(true);
    };

    let Some(state) = status.get("status").and_then(|value| value.as_str()) else {
        return Ok(true);
    };

    if state != "running" {
        return Ok(true);
    }

    let Some(timestamp) = status.get("timestamp").and_then(|value| value.as_u64()) else {
        return Ok(true);
    };
    if now_unix_secs.saturating_sub(timestamp) >= RUNNING_STATUS_STALE_SECS {
        return Ok(true);
    }

    let Some(pid) = status
        .get("pid")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
    else {
        return Ok(false);
    };

    Ok(!training_process_matches_profile(pid, &profile.profile_name))
}

fn training_process_matches_profile(pid: u32, profile_name: &str) -> bool {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    let Ok(raw) = fs::read(cmdline_path) else {
        return false;
    };
    if raw.is_empty() {
        return false;
    }

    let args = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .filter_map(|part| std::str::from_utf8(part).ok())
        .collect::<Vec<_>>();

    args.windows(2).any(|window| {
        window[0] == "train-profile" && window[1] == profile_name
    })
}

pub fn build_training_subprocess_command(
    root_dir: &str,
    profile_name: &str,
    allowed_cpus: &str,
) -> Result<TrainingSubprocessCommand> {
    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let working_dir = PathBuf::from(root_dir);

    if !systemd_run_available() {
        info!("systemd-run not found in PATH; falling back to direct training subprocess");
        return Ok(TrainingSubprocessCommand {
            program: current_exe.display().to_string(),
            args: vec![
                "train-profile".to_string(),
                profile_name.to_string(),
            ],
            working_dir: Some(working_dir),
        });
    }

    Ok(TrainingSubprocessCommand {
        program: "systemd-run".to_string(),
        args: vec![
            "--scope".to_string(),
            "-p".to_string(),
            format!("AllowedCPUs={allowed_cpus}"),
            "--working-directory".to_string(),
            working_dir.display().to_string(),
            current_exe.display().to_string(),
            "train-profile".to_string(),
            profile_name.to_string(),
        ],
        working_dir: None,
    })
}

fn systemd_run_available() -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join("systemd-run");
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

pub async fn plan_training_round(
    root_dir: &Path,
    settings: &Settings,
    now_unix_secs: u64,
) -> Result<Vec<PlannedTrainingAction>> {
    let rpc = SampleRpcConfig::from_settings(settings)?;
    let mut planned = Vec::new();

    for scanned in scan_profiles(root_dir)? {
        if !training_launch_allowed(&scanned.profile, now_unix_secs)? {
            continue;
        }

        if !cooldown_allows_training(
            &scanned.profile,
            settings.training_scheduler_failure_cooldown_minutes,
            now_unix_secs,
        )? {
            continue;
        }

        let sample_count = match fetch_training_sample_count_via_rpc(&rpc, &scanned.profile).await {
            Ok(sample_count) => sample_count,
            Err(err) => {
                warn!(
                    profile = %scanned.profile_name,
                    error = %err,
                    "skipping profile during scheduler round because training sample count is unavailable"
                );
                continue;
            }
        };
        let model_mtime = fs::metadata(scanned.profile.training_model_path())
            .ok()
            .and_then(|meta| meta.modified().ok());
        let now = std::time::UNIX_EPOCH + Duration::from_secs(now_unix_secs);
        let decision = evaluate_training_need(&scanned.profile, sample_count, model_mtime, now)?;
        if decision.should_train {
            planned.push(PlannedTrainingAction {
                profile_name: scanned.profile_name,
                sample_count,
                reason: decision.reason,
            });
        }
    }

    Ok(planned)
}

pub fn spawn_detached_command(
    program: &str,
    args: &[String],
    profile_name: &str,
) -> Result<u32> {
    spawn_detached_command_with_dir(program, args, None, profile_name)
}

fn spawn_detached_command_with_dir(
    program: &str,
    args: &[String],
    working_dir: Option<&Path>,
    profile_name: &str,
) -> Result<u32> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn detached command for {}", profile_name))?;
    let pid = child.id();
    let profile_name = profile_name.to_string();
    std::thread::spawn(move || match child.wait() {
        Ok(status) => {
            if !status.success() {
                warn!(
                    profile = %profile_name,
                    status = ?status.code(),
                    "training subprocess exited unsuccessfully"
                );
            }
        }
        Err(err) => {
            warn!(
                profile = %profile_name,
                error = %err,
                "failed to wait for detached training subprocess"
            );
        }
    });
    Ok(pid)
}

pub async fn spawn_training_subprocess(
    settings: &Settings,
    profile_name: &str,
) -> Result<u32> {
    let command = build_training_subprocess_command(
        &settings.root_dir.display().to_string(),
        profile_name,
        &settings.training_subprocess_allowed_cpus,
    )?;
    spawn_detached_command_with_dir(
        &command.program,
        &command.args,
        command.working_dir.as_deref(),
        profile_name,
    )
}

pub async fn run_scheduler_once(settings: &Settings) -> Result<Vec<PlannedTrainingAction>> {
    let now_unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::ZERO)
        .as_secs();
    if !training_window_allows_launch(now_unix_secs) {
        return Ok(Vec::new());
    }
    let planned = plan_training_round(&settings.root_dir, settings, now_unix_secs).await?;

    for action in &planned {
        info!(
            profile = %action.profile_name,
            sample_count = action.sample_count,
            reason = action.reason,
            "scheduler selected profile for training"
        );

        let pid = spawn_training_subprocess(settings, &action.profile_name).await?;
        info!(profile = %action.profile_name, pid, "scheduler started detached training subprocess");
    }

    Ok(planned)
}

fn training_window_allows_launch(now_unix_secs: u64) -> bool {
    let Ok(raw) = std::env::var("TRAINING_SCHEDULER_WINDOW") else {
        return true;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return true;
    }
    let Some((start, end)) = parse_hour_window(raw) else {
        warn!(window = %raw, "invalid TRAINING_SCHEDULER_WINDOW, ignoring");
        return true;
    };
    let hour = local_hour(now_unix_secs);
    if hour_in_window(hour, start, end) {
        true
    } else {
        info!(window = %raw, hour, "scheduler round skipped: outside training window");
        false
    }
}

fn parse_hour_window(raw: &str) -> Option<(u32, u32)> {
    let (start, end) = raw.split_once('-')?;
    let start = start.trim().parse::<u32>().ok()?;
    let end = end.trim().parse::<u32>().ok()?;
    if start > 23 || end > 23 {
        return None;
    }
    Some((start, end))
}

fn hour_in_window(hour: u32, start: u32, end: u32) -> bool {
    if start == end {
        return hour == start;
    }
    if start < end {
        hour >= start && hour < end
    } else {
        hour >= start || hour < end
    }
}

#[cfg(unix)]
fn local_hour(now_unix_secs: u64) -> u32 {
    let timestamp = now_unix_secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::localtime_r(&timestamp, &mut tm) };
    if rc.is_null() {
        return ((now_unix_secs / 3600) % 24) as u32;
    }
    u32::try_from(tm.tm_hour).unwrap_or(0)
}

#[cfg(not(unix))]
fn local_hour(now_unix_secs: u64) -> u32 {
    ((now_unix_secs / 3600) % 24) as u32
}

pub fn start_scheduler_loop(settings: Settings) -> Option<JoinHandle<()>> {
    if !settings.training_scheduler_enabled {
        return None;
    }

    Some(tokio::spawn(async move {
        let interval = Duration::from_secs(settings.training_scheduler_interval_minutes.max(1) * 60);
        loop {
            if let Err(err) = run_scheduler_once(&settings).await {
                warn!(error = %err, "training scheduler round failed");
            }
            tokio::time::sleep(interval).await;
        }
    }))
}
