use super::{
    AppError, VERSION, managed_codex_executable, product_data_directory, resolve_codex,
    validate_model_value, validate_reasoning,
};
use dialoguer::{Select, theme::ColorfulTheme};
use needle_core::{
    CodexHost, CodexRole, CommandPolicy, Digest, EvidenceFailurePolicy, FallbackPolicy,
    FilesystemPolicy, NeedKey, NetworkPolicy, ReasoningLevel, RepairPolicy, RoleProfileBudget,
    RoleProfileDefinition, RoleProfileDefinitionInput, RoleProfileId, ServiceTier, TestPolicy,
    ToolPolicy,
};
use needle_platform_codex::{CodexWorker, HookConfig, IsolationReport};
use needle_runtime::{
    ActivationScope, ActivationStatus, RuntimeSettings, RuntimeStore, StoreError,
    capture_git_snapshot,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::env;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_PROFILE_ID: &str = "explorer.default";
const DEFAULT_REASONING: &str = "medium";
const DEFAULT_TIMEOUT_SECONDS: u64 = 180;
const CODEX_APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const MODEL_LIST_LIMIT: u64 = 100;
const MAX_MODEL_LIST_PAGES: usize = 20;
pub(crate) const DEFAULT_MAX_COST_MICROUSD: u64 = 1_000_000;

#[derive(Clone, Debug, Eq, PartialEq)]
struct AvailableCodexModel {
    model: String,
    display_name: String,
    description: String,
    is_default: bool,
}

pub(crate) fn run_enable(arguments: Vec<String>) -> Result<(), AppError> {
    if help_requested(&arguments) {
        println!(
            "Usage: needle enable [--global] [--repository <path>] [--worker-model <model>] [--worker-reasoning <level>] [--worker-timeout-seconds <seconds>] [--codex <path>] [--data-dir <path>] [--json] [--no-color]"
        );
        return Ok(());
    }
    validate_arguments(&arguments, Action::Enable)?;
    let json_output = has_flag(&arguments, "--json");
    let global = has_flag(&arguments, "--global");
    let repository = activation_context(&arguments, global)?;
    let data_directory = product_data_directory(&arguments)?;
    let store = RuntimeStore::new(data_directory.join("needle.sqlite3"));
    store.initialize().map_err(runtime_error)?;
    let settings = match store.settings() {
        Ok(settings) => settings,
        Err(StoreError::MissingSetting(_)) => initialize_first_run(&store, &arguments)?,
        Err(error) => return Err(runtime_error(error)),
    };
    let isolation =
        CodexWorker::verify_isolation(&settings.codex_executable).map_err(AppError::Runtime)?;
    if !isolation.verified() {
        return Err(AppError::Runtime(format!(
            "Codex {} is not in the exact validated set or lacks required isolation flags; Needle remains disabled",
            isolation.codex_version
        )));
    }
    let profile_id = ensure_default_profile(&store, &settings, DEFAULT_MAX_COST_MICROUSD)?;
    let codex_installations = detect_codex_installations();
    let integrations = configure_integrations(&codex_installations)?;
    let activation = if global {
        store.set_global_activation(true, Some(&profile_id))
    } else {
        store.set_repository_activation(&repository, true, Some(&profile_id))
    }
    .map_err(runtime_error)?;
    let status = store.activation_status(&repository).map_err(runtime_error)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "enabled",
                "repository": repository,
                "activation": activation,
                "effective": status,
                "codex_version": isolation.codex_version,
                "runtime_mode": "on_demand",
                "transport": "surface_specific",
                "codex_installations": codex_installations,
                "integrations": integrations,
            }))?
        );
    } else {
        println!(
            "{}",
            render_enable_success(
                &EnableSummary {
                    repository: &repository,
                    global,
                    worker_model: &settings.worker_model,
                    profile_id: profile_id.as_str(),
                    codex_version: &isolation.codex_version,
                    installations: &codex_installations,
                    integrations: &integrations,
                },
                status_colors_enabled(&arguments),
            )
        );
    }
    Ok(())
}

struct EnableSummary<'a> {
    repository: &'a std::path::Path,
    global: bool,
    worker_model: &'a str,
    profile_id: &'a str,
    codex_version: &'a str,
    installations: &'a CodexInstallations,
    integrations: &'a CodexIntegrationStatus,
}

fn render_enable_success(summary: &EnableSummary<'_>, use_color: bool) -> String {
    let title = paint("Needle enabled.", AnsiColor::Green, use_color);
    let scope = if summary.global { "Global" } else { "Repository" };
    let cli = cli_summary(&summary.installations.cli, summary.integrations, use_color);
    let desktop = desktop_summary(&summary.installations.desktop, summary.integrations, use_color);
    let mut next_steps = Vec::new();
    if summary.installations.desktop.found {
        let changed =
            summary.integrations.desktop_skill.as_ref().is_some_and(|skill| skill.changed);
        next_steps.push(if changed {
            "Restart Codex Desktop, open the repository, and write a normal request."
        } else {
            "Open a new Codex Desktop task in the repository and write a normal request."
        });
    }
    if summary.installations.cli.found {
        let review = summary
            .integrations
            .cli_hooks
            .as_ref()
            .is_some_and(|hooks| hooks.trust_review_required);
        next_steps.push(if review {
            "For Codex CLI, run `/hooks`, review the Needle commands, and trust them once."
        } else {
            "Codex CLI hooks are registered; review them with `/hooks` if the CLI requests it."
        });
    }
    if next_steps.is_empty() {
        next_steps.push(
            "No user-facing Codex client was detected. Install Codex Desktop or Codex CLI, then run `needle enable` again.",
        );
    }
    let next_step = next_steps.join("\n");
    let migration = if summary.integrations.stale_cli_hooks_removed {
        "\nMigration:      Removed obsolete Needle hooks because Codex CLI is not installed."
    } else {
        ""
    };

    format!(
        "{title}\n\nRepository:     {}\nScope:          {scope}\nWorker model:   {}\nProfile:        {}\nCodex runtime:  {} - Verified\nCodex CLI:      {cli}\nCodex Desktop:  {desktop}\nMode:           On demand{migration}\n\n{next_step}",
        display_path(summary.repository),
        summary.worker_model,
        summary.profile_id,
        summary.codex_version,
    )
}

pub(crate) fn run_disable(arguments: Vec<String>) -> Result<(), AppError> {
    if help_requested(&arguments) {
        println!(
            "Usage: needle disable [--global] [--repository <path>] [--data-dir <path>] [--json] [--no-color]"
        );
        return Ok(());
    }
    validate_arguments(&arguments, Action::Disable)?;
    let json_output = has_flag(&arguments, "--json");
    let use_color = !json_output && status_colors_enabled(&arguments);
    let global = has_flag(&arguments, "--global");
    let repository = activation_context(&arguments, global)?;
    let data_directory = product_data_directory(&arguments)?;
    let desktop_skill = crate::codex_skill::remove_managed().map_err(AppError::Runtime)?;
    let database = data_directory.join("needle.sqlite3");
    if !database.is_file() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "status": "disabled",
                    "initialized": false,
                    "repository": repository,
                    "desktop_skill": desktop_skill,
                }))?
            );
        } else {
            println!(
                "{}\n\nRepository:     {}\nSetup:          Not completed\nCodex Desktop:  {}\n\nNeedle has no activation data to disable.{}",
                paint("Needle is disabled.", AnsiColor::Yellow, use_color),
                display_path(&repository),
                desktop_skill_removal_summary(&desktop_skill),
                desktop_restart_after_removal(&desktop_skill),
            );
        }
        return Ok(());
    }
    let store = RuntimeStore::new(database);
    let activation = if global {
        store.set_global_activation(false, None)
    } else {
        store.set_repository_activation(&repository, false, None)
    }
    .map_err(runtime_error)?;
    let status = store.activation_status(&repository).map_err(runtime_error)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "disabled",
                "repository": repository,
                "activation": activation,
                "effective": status,
                "desktop_skill": desktop_skill,
            }))?
        );
    } else {
        let scope = if global { "Global" } else { "Repository" };
        println!(
            "{}\n\nRepository:     {}\nScope:          {scope}\nCodex Desktop:  {}\n\nSettings, Codex CLI hooks, and cached context were kept.{}",
            paint("Needle disabled.", AnsiColor::Yellow, use_color),
            display_path(&repository),
            desktop_skill_removal_summary(&desktop_skill),
            desktop_restart_after_removal(&desktop_skill),
        );
    }
    Ok(())
}

fn desktop_skill_removal_summary(removal: &crate::codex_skill::SkillRemoval) -> &'static str {
    if removal.removed {
        "Needle skill removed"
    } else if removal.unmanaged_preserved {
        "Unmanaged skill preserved"
    } else {
        "Needle skill not installed"
    }
}

fn desktop_restart_after_removal(removal: &crate::codex_skill::SkillRemoval) -> &'static str {
    if removal.removed {
        "\n\nRestart Codex Desktop so new tasks stop loading the removed Needle skill."
    } else {
        ""
    }
}

pub(crate) fn run_status(arguments: Vec<String>) -> Result<(), AppError> {
    if help_requested(&arguments) {
        println!(
            "Usage: needle status [--repository <path>] [--data-dir <path>] [--json] [--no-color]"
        );
        return Ok(());
    }
    validate_arguments(&arguments, Action::Status)?;
    let json_output = has_flag(&arguments, "--json");
    let use_color = !json_output && status_colors_enabled(&arguments);
    let repository = repository_root(&arguments)?;
    let needle_status = detect_needle_status();
    let codex_installations = detect_codex_installations();
    let integrations = inspect_integrations();
    let data_directory = product_data_directory(&arguments)?;
    let database = data_directory.join("needle.sqlite3");
    if !database.is_file() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "installed": true,
                    "initialized": false,
                    "enabled": false,
                    "repository": repository,
                    "database": database,
                    "needle": needle_status,
                    "codex_installations": codex_installations,
                    "integrations": integrations,
                }))?
            );
        } else {
            println!(
                "{}",
                render_uninitialized_status(
                    &repository,
                    &needle_status,
                    &codex_installations,
                    &integrations,
                    use_color,
                )
            );
        }
        return Ok(());
    }
    let store = RuntimeStore::new(database);
    let activation = store.activation_status(&repository).map_err(runtime_error)?;
    let settings = store.settings().ok();
    let isolation = settings
        .as_ref()
        .and_then(|settings| CodexWorker::verify_isolation(&settings.codex_executable).ok());
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "installed": true,
                "initialized": settings.is_some(),
                "enabled": activation.enabled,
                "repository": repository,
                "activation": activation,
                "codex": isolation.map(|report| json!({
                    "version": report.codex_version,
                    "supported": report.supported,
                    "required_flags_present": report.required_flags_present,
                    "isolation_verified": report.verified(),
                })),
                "needle": needle_status,
                "codex_installations": codex_installations,
                "integrations": integrations,
                "runtime_mode": "on_demand",
                "transport": "surface_specific",
            }))?
        );
    } else {
        println!(
            "{}",
            if settings.is_some() {
                render_initialized_status(
                    &repository,
                    &activation,
                    isolation.as_ref(),
                    &needle_status,
                    &codex_installations,
                    &integrations,
                    use_color,
                )
            } else {
                render_uninitialized_status(
                    &repository,
                    &needle_status,
                    &codex_installations,
                    &integrations,
                    use_color,
                )
            }
        );
    }
    Ok(())
}

fn render_uninitialized_status(
    repository: &std::path::Path,
    needle: &NeedleStatus,
    codex: &CodexInstallations,
    integrations: &CodexIntegrationStatus,
    use_color: bool,
) -> String {
    let disabled = paint("Needle is disabled.", AnsiColor::Yellow, use_color);
    format!(
        "{disabled}\n\nRepository:      {}\nSetup:           Not completed\nNeedle:          {}\nCodex CLI:       {}\nCodex Desktop:   {}\nWorker runtime:  {}\n\nRun `needle enable` to set up and activate Needle for this repository.",
        display_path(repository),
        needle.human_summary(use_color),
        cli_summary(&codex.cli, integrations, use_color),
        desktop_summary(&codex.desktop, integrations, use_color),
        codex.managed.human_summary(use_color, true),
    )
}

fn render_initialized_status(
    repository: &std::path::Path,
    activation: &ActivationStatus,
    isolation: Option<&IsolationReport>,
    needle: &NeedleStatus,
    codex_installations: &CodexInstallations,
    integrations: &CodexIntegrationStatus,
    use_color: bool,
) -> String {
    let state = if activation.enabled {
        paint("Enabled", AnsiColor::Green, use_color)
    } else {
        paint("Disabled", AnsiColor::Yellow, use_color)
    };
    let scope = match activation.effective_scope.as_ref() {
        Some(ActivationScope::Global) => "Global",
        Some(ActivationScope::Repository { .. }) => "Repository",
        None => "None",
    };
    let profile = activation
        .role_profile_id
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "None".to_owned());
    let codex = isolation
        .map(|report| {
            let compatibility = if report.verified() { "verified" } else { "unsupported" };
            format!("{} ({compatibility})", report.codex_version)
        })
        .unwrap_or_else(|| "Unavailable".to_owned());
    let integration_ready = (codex_installations.cli.found
        && integrations.cli_hooks.as_ref().is_some_and(|hooks| hooks.registered))
        || (codex_installations.desktop.found
            && integrations.desktop_skill.as_ref().is_some_and(|skill| skill.current));
    let next_step = if activation.enabled && integration_ready {
        "Needle will be available to new Codex tasks and CLI sessions."
    } else if activation.enabled {
        "Needle is enabled, but no detected Codex client integration is ready. Run `needle enable` again."
    } else {
        "Run `needle enable` to activate Needle for this repository."
    };

    format!(
        "Needle status\n\nState:              {state}\nRepository:         {}\nScope:              {scope}\nProfile:            {profile}\nNeedle:             {}\nCodex CLI:          {}\nCodex Desktop:      {}\nWorker runtime:     {}\nConfigured worker:  {codex}\nMode:               On demand\n\n{next_step}",
        display_path(repository),
        needle.human_summary(use_color),
        cli_summary(&codex_installations.cli, integrations, use_color),
        desktop_summary(&codex_installations.desktop, integrations, use_color),
        codex_installations.managed.human_summary(use_color, true),
    )
}

#[derive(Clone, Debug, Serialize)]
struct CodexInstallations {
    cli: DetectedInstallation,
    desktop: DetectedInstallation,
    managed: DetectedInstallation,
}

#[derive(Clone, Debug, Serialize)]
struct CodexIntegrationStatus {
    cli_hooks: Option<crate::codex_hooks::HookRegistration>,
    cli_error: Option<String>,
    desktop_skill: Option<crate::codex_skill::SkillInstallation>,
    desktop_error: Option<String>,
    stale_cli_hooks_removed: bool,
}

fn configure_integrations(
    installations: &CodexInstallations,
) -> Result<CodexIntegrationStatus, AppError> {
    let desktop_skill = if installations.desktop.found {
        crate::codex_skill::ensure_installed()
    } else {
        crate::codex_skill::inspect()
    }
    .map_err(AppError::Runtime)?;
    let cli_hooks = if installations.cli.found {
        crate::codex_hooks::ensure_registered()
    } else {
        crate::codex_hooks::remove_registered()
    }
    .map_err(AppError::Runtime)?;
    let stale_cli_hooks_removed = !installations.cli.found && cli_hooks.removed;
    Ok(CodexIntegrationStatus {
        cli_hooks: Some(cli_hooks),
        cli_error: None,
        desktop_skill: Some(desktop_skill),
        desktop_error: None,
        stale_cli_hooks_removed,
    })
}

fn inspect_integrations() -> CodexIntegrationStatus {
    let (cli_hooks, cli_error) = match crate::codex_hooks::inspect() {
        Ok(status) => (Some(status), None),
        Err(error) => (None, Some(error)),
    };
    let (desktop_skill, desktop_error) = match crate::codex_skill::inspect() {
        Ok(status) => (Some(status), None),
        Err(error) => (None, Some(error)),
    };
    CodexIntegrationStatus {
        cli_hooks,
        cli_error,
        desktop_skill,
        desktop_error,
        stale_cli_hooks_removed: false,
    }
}

fn cli_summary(
    installation: &DetectedInstallation,
    integrations: &CodexIntegrationStatus,
    use_color: bool,
) -> String {
    let base = installation.human_summary(use_color, false);
    if !installation.found {
        return base;
    }
    let integration = if integrations.cli_error.is_some() {
        paint("Hook check failed", AnsiColor::Red, use_color)
    } else if integrations.cli_hooks.as_ref().is_some_and(|hooks| hooks.registered) {
        paint("Hooks registered", AnsiColor::Green, use_color)
    } else {
        paint("Hooks not configured", AnsiColor::Yellow, use_color)
    };
    format!("{base} - {integration}")
}

fn desktop_summary(
    installation: &DetectedInstallation,
    integrations: &CodexIntegrationStatus,
    use_color: bool,
) -> String {
    let base = installation.human_summary(use_color, false);
    if !installation.found {
        return base;
    }
    let integration = if integrations.desktop_error.is_some() {
        paint("Skill check failed", AnsiColor::Red, use_color)
    } else {
        match integrations.desktop_skill.as_ref() {
            Some(skill) if skill.current => {
                paint("Needle skill ready", AnsiColor::Green, use_color)
            }
            Some(skill) if skill.installed && !skill.managed => {
                paint("Skill path conflict", AnsiColor::Red, use_color)
            }
            Some(skill) if skill.installed => {
                paint("Needle skill update required", AnsiColor::Yellow, use_color)
            }
            _ => paint("Needle skill not installed", AnsiColor::Yellow, use_color),
        }
    };
    format!("{base} - {integration}")
}

#[derive(Clone, Debug, Serialize)]
struct NeedleStatus {
    version: String,
    update: UpdateAvailability,
}

#[derive(Clone, Debug, Serialize)]
struct DetectedInstallation {
    found: bool,
    supported_on_platform: bool,
    version: Option<String>,
    update: UpdateAvailability,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum UpdateAvailability {
    NotApplicable,
    UpToDate,
    Available,
    ManagedByNeedle,
    NoPublishedRelease,
    Unknown,
}

impl NeedleStatus {
    fn human_summary(&self, use_color: bool) -> String {
        format!(
            "{} - {}",
            paint(&self.version, AnsiColor::Green, use_color),
            update_summary(&self.update, use_color)
        )
    }
}

impl DetectedInstallation {
    fn not_found() -> Self {
        Self {
            found: false,
            supported_on_platform: true,
            version: None,
            update: UpdateAvailability::NotApplicable,
        }
    }

    fn found(version: Option<String>) -> Self {
        Self {
            found: true,
            supported_on_platform: true,
            version,
            update: UpdateAvailability::Unknown,
        }
    }

    fn human_summary(&self, use_color: bool, required: bool) -> String {
        if !self.supported_on_platform {
            return paint(
                &format!("Not available on {}", current_platform_name()),
                AnsiColor::Gray,
                use_color,
            );
        }
        let base = match &self.version {
            Some(version) => paint(&format!("Found ({version})"), AnsiColor::Green, use_color),
            None if self.found => "Found".to_owned(),
            None => paint(
                "Not found",
                if required { AnsiColor::Red } else { AnsiColor::Gray },
                use_color,
            ),
        };
        if matches!(self.update, UpdateAvailability::NotApplicable) {
            base
        } else {
            format!("{base} - {}", update_summary(&self.update, use_color))
        }
    }
}

fn update_summary(update: &UpdateAvailability, use_color: bool) -> String {
    match update {
        UpdateAvailability::NotApplicable => String::new(),
        UpdateAvailability::UpToDate => paint("Up to date", AnsiColor::Green, use_color),
        UpdateAvailability::Available => paint("Update available", AnsiColor::Yellow, use_color),
        UpdateAvailability::ManagedByNeedle => {
            paint("Ready and validated by Needle", AnsiColor::Green, use_color)
        }
        UpdateAvailability::NoPublishedRelease => {
            paint("Development build", AnsiColor::Gray, use_color)
        }
        UpdateAvailability::Unknown => {
            paint("Update check unavailable", AnsiColor::Gray, use_color)
        }
    }
}

fn detect_codex_installations() -> CodexInstallations {
    let mut cli = detect_codex_cli();
    let mut desktop = detect_codex_desktop();
    let mut managed = detect_managed_codex();
    if cli.found {
        cli.update = match fetch_latest_release_version("openai/codex") {
            ReleaseLookup::Version(latest) => {
                update_against_latest(cli.version.as_deref(), Some(&latest))
            }
            ReleaseLookup::NotPublished | ReleaseLookup::Unavailable => UpdateAvailability::Unknown,
        };
    }
    if desktop.found {
        desktop.update = detect_desktop_update_availability(desktop.version.as_deref());
    }
    if managed.found {
        managed.update = UpdateAvailability::ManagedByNeedle;
    }
    CodexInstallations { cli, desktop, managed }
}

fn detect_needle_status() -> NeedleStatus {
    let update = match fetch_latest_release_version("IASolutionOrg/Needle") {
        ReleaseLookup::Version(latest) => update_against_latest(Some(VERSION), Some(&latest)),
        ReleaseLookup::NotPublished => UpdateAvailability::NoPublishedRelease,
        ReleaseLookup::Unavailable => UpdateAvailability::Unknown,
    };
    NeedleStatus { version: VERSION.to_owned(), update }
}

fn update_against_latest(current: Option<&str>, latest: Option<&str>) -> UpdateAvailability {
    match (current, latest) {
        (Some(current), Some(latest)) => match compare_versions(current, latest) {
            Some(Ordering::Less) => UpdateAvailability::Available,
            Some(_) => UpdateAvailability::UpToDate,
            None => UpdateAvailability::Unknown,
        },
        _ => UpdateAvailability::Unknown,
    }
}

fn compare_versions(left: &str, right: &str) -> Option<Ordering> {
    fn parts(version: &str) -> Option<Vec<u64>> {
        version.split('.').map(|part| part.parse::<u64>().ok()).collect()
    }
    let mut left = parts(left)?;
    let mut right = parts(right)?;
    let width = left.len().max(right.len());
    left.resize(width, 0);
    right.resize(width, 0);
    Some(left.cmp(&right))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReleaseLookup {
    Version(String),
    NotPublished,
    Unavailable,
}

fn fetch_latest_release_version(repository: &str) -> ReleaseLookup {
    let curl = if cfg!(windows) { "curl.exe" } else { "curl" };
    let url = format!("https://api.github.com/repos/{repository}/releases/latest");
    let output = Command::new(curl)
        .args([
            "--silent",
            "--show-error",
            "--location",
            "--connect-timeout",
            "2",
            "--max-time",
            "4",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "User-Agent: needle-status",
            "--write-out",
            "\n%{http_code}",
        ])
        .arg(url)
        .output()
        .ok();
    let Some(output) = output else {
        return ReleaseLookup::Unavailable;
    };
    if !output.status.success() {
        return ReleaseLookup::Unavailable;
    }
    parse_release_response(&output.stdout)
}

fn parse_release_response(bytes: &[u8]) -> ReleaseLookup {
    let Ok(response) = std::str::from_utf8(bytes) else {
        return ReleaseLookup::Unavailable;
    };
    let Some((body, status)) = response.rsplit_once('\n') else {
        return ReleaseLookup::Unavailable;
    };
    match status.trim() {
        "200" => {
            let Ok(document) = serde_json::from_str::<serde_json::Value>(body) else {
                return ReleaseLookup::Unavailable;
            };
            let Some(tag) = document.get("tag_name").and_then(serde_json::Value::as_str) else {
                return ReleaseLookup::Unavailable;
            };
            let version =
                tag.strip_prefix("rust-v").or_else(|| tag.strip_prefix('v')).unwrap_or(tag);
            ReleaseLookup::Version(version.to_owned())
        }
        "404" => ReleaseLookup::NotPublished,
        _ => ReleaseLookup::Unavailable,
    }
}

fn detect_managed_codex() -> DetectedInstallation {
    let Some(path) = managed_codex_executable() else {
        return DetectedInstallation::not_found();
    };
    let Ok(output) = Command::new(path).arg("--version").output() else {
        return DetectedInstallation::not_found();
    };
    let version = parse_command_version(&String::from_utf8_lossy(&output.stdout));
    DetectedInstallation::found(version)
}

#[cfg(windows)]
fn detect_codex_cli() -> DetectedInstallation {
    let Ok(located) = Command::new("where.exe").arg("codex").output() else {
        return DetectedInstallation::not_found();
    };
    if !located.status.success() {
        return DetectedInstallation::not_found();
    }
    let Some(path) = String::from_utf8_lossy(&located.stdout)
        .lines()
        .map(str::trim)
        .find(|path| !path.is_empty())
        .map(PathBuf::from)
    else {
        return DetectedInstallation::not_found();
    };
    let extension = path.extension().and_then(|value| value.to_str()).unwrap_or_default();
    let output = if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
        Command::new("cmd.exe").args(["/d", "/c"]).arg(&path).arg("--version").output()
    } else {
        Command::new(&path).arg("--version").output()
    };
    let Ok(output) = output else {
        return DetectedInstallation::not_found();
    };
    if !output.status.success() {
        return DetectedInstallation::not_found();
    }
    let version = parse_command_version(&String::from_utf8_lossy(&output.stdout));
    DetectedInstallation::found(version)
}

#[cfg(not(windows))]
fn detect_codex_cli() -> DetectedInstallation {
    let Ok(output) = Command::new("codex").arg("--version").output() else {
        return DetectedInstallation::not_found();
    };
    if !output.status.success() {
        return DetectedInstallation::not_found();
    }
    let version = parse_command_version(&String::from_utf8_lossy(&output.stdout));
    DetectedInstallation::found(version)
}

fn parse_command_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|part| part.bytes().next().is_some_and(|byte| byte.is_ascii_digit()))
        .map(ToOwned::to_owned)
}

#[cfg(windows)]
fn detect_codex_desktop() -> DetectedInstallation {
    const APPX_PACKAGES_KEY: &str = r"HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";
    let output = Command::new("reg.exe")
        .args(["query", APPX_PACKAGES_KEY, "/f", "OpenAI.Codex_", "/k"])
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains(r"\OpenAI.Codex_") {
            return DetectedInstallation::found(parse_windows_codex_package_version(&stdout));
        }
    }

    for key in [
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths\Codex.exe",
        r"HKLM\Software\Microsoft\Windows\CurrentVersion\App Paths\Codex.exe",
    ] {
        if Command::new("reg.exe")
            .args(["query", key, "/ve"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return DetectedInstallation::found(None);
        }
    }

    let mut candidates = Vec::new();
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        candidates.push(local.join("Programs").join("Codex").join("Codex.exe"));
        candidates.push(local.join("Programs").join("OpenAI Codex").join("Codex.exe"));
        candidates.push(local.join("Codex").join("Codex.exe"));
    }
    if let Some(program_files) = env::var_os("PROGRAMFILES") {
        let program_files = PathBuf::from(program_files);
        candidates.push(program_files.join("Codex").join("Codex.exe"));
        candidates.push(program_files.join("OpenAI").join("Codex").join("Codex.exe"));
    }
    if candidates.into_iter().any(|path| path.is_file()) {
        DetectedInstallation::found(None)
    } else {
        DetectedInstallation::not_found()
    }
}

#[cfg(windows)]
fn detect_desktop_update_availability(current_version: Option<&str>) -> UpdateAvailability {
    const UPDATE_MANIFEST_URL: &str =
        "https://persistent.oaistatic.com/codex-app-prod/windows-store-update.json";
    const CHATGPT_STORE_PRODUCT_ID: &str = "9PLM9XGG6VKS";
    const PACKAGE_IDENTITY: &str = "OpenAI.Codex";
    let curl = "curl.exe";
    let Ok(output) = Command::new(curl)
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--connect-timeout",
            "2",
            "--max-time",
            "4",
            UPDATE_MANIFEST_URL,
        ])
        .output()
    else {
        return UpdateAvailability::Unknown;
    };
    if !output.status.success() {
        return UpdateAvailability::Unknown;
    }
    parse_windows_desktop_update_manifest(
        &output.stdout,
        current_version,
        CHATGPT_STORE_PRODUCT_ID,
        PACKAGE_IDENTITY,
    )
}

#[cfg(windows)]
fn parse_windows_desktop_update_manifest(
    bytes: &[u8],
    current_version: Option<&str>,
    expected_product_id: &str,
    expected_package_identity: &str,
) -> UpdateAvailability {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Manifest {
        schema_version: u64,
        build_version: String,
        store_product_id: String,
        package_identity: String,
    }

    let Ok(manifest) = serde_json::from_slice::<Manifest>(bytes) else {
        return UpdateAvailability::Unknown;
    };
    if manifest.schema_version != 1
        || manifest.store_product_id != expected_product_id
        || manifest.package_identity != expected_package_identity
    {
        return UpdateAvailability::Unknown;
    }
    update_against_latest(current_version, Some(&manifest.build_version))
}

#[cfg(windows)]
fn parse_windows_codex_package_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let package = line.trim().rsplit('\u{5c}').next()?;
        package.strip_prefix("OpenAI.Codex_")?.split('_').next().map(ToOwned::to_owned)
    })
}

#[cfg(target_os = "macos")]
fn detect_codex_desktop() -> DetectedInstallation {
    let mut candidates = vec![
        std::path::PathBuf::from("/Applications/ChatGPT.app"),
        std::path::PathBuf::from("/Applications/Codex.app"),
    ];
    if let Some(home) = env::var_os("HOME") {
        let applications = std::path::PathBuf::from(home).join("Applications");
        candidates.push(applications.join("ChatGPT.app"));
        candidates.push(applications.join("Codex.app"));
    }
    let Some(application) = candidates.into_iter().find(|path| path.is_dir()) else {
        return DetectedInstallation::not_found();
    };
    let plist = application.join("Contents").join("Info.plist");
    let version = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(plist)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            (!version.is_empty()).then_some(version)
        });
    DetectedInstallation::found(version)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn detect_codex_desktop() -> DetectedInstallation {
    DetectedInstallation {
        found: false,
        supported_on_platform: false,
        version: None,
        update: UpdateAvailability::NotApplicable,
    }
}

#[cfg(not(windows))]
fn detect_desktop_update_availability(_current_version: Option<&str>) -> UpdateAvailability {
    UpdateAvailability::Unknown
}

fn current_platform_name() -> &'static str {
    if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        std::env::consts::OS
    }
}

fn display_path(path: &std::path::Path) -> String {
    let rendered = path.display().to_string();
    rendered
        .strip_prefix(r"\\?\UNC\")
        .map(|path| format!(r"\\{path}"))
        .or_else(|| rendered.strip_prefix(r"\\?\").map(ToOwned::to_owned))
        .unwrap_or(rendered)
}

#[derive(Clone, Copy)]
enum AnsiColor {
    Green,
    Yellow,
    Red,
    Gray,
}

fn paint(text: &str, color: AnsiColor, enabled: bool) -> String {
    if !enabled {
        return text.to_owned();
    }
    let code = match color {
        AnsiColor::Green => 32,
        AnsiColor::Yellow => 33,
        AnsiColor::Red => 31,
        AnsiColor::Gray => 90,
    };
    format!("\u{1b}[{code}m{text}\u{1b}[0m")
}

fn status_colors_enabled(arguments: &[String]) -> bool {
    !has_flag(arguments, "--no-color")
        && env::var_os("NO_COLOR").is_none()
        && io::stdout().is_terminal()
}

pub(crate) fn run_ui(arguments: Vec<String>) -> Result<(), AppError> {
    if help_requested(&arguments) {
        println!("Usage: needle ui [--repository <path>] [--data-dir <path>]");
        return Ok(());
    }
    validate_arguments(&arguments, Action::Ui)?;
    let data_directory = product_data_directory(&arguments)?;
    let repository = repository_root(&arguments)?;
    crate::server::run(data_directory, repository, true).map_err(AppError::Runtime)
}

fn initialize_first_run(
    store: &RuntimeStore,
    arguments: &[String],
) -> Result<RuntimeSettings, AppError> {
    let codex = resolve_codex(option_value(arguments, "--codex"))?;
    let codex_executable = codex.to_string_lossy().into_owned();
    let isolation = CodexWorker::verify_isolation(&codex_executable).map_err(AppError::Runtime)?;
    if !isolation.verified() {
        return Err(AppError::Runtime(format!(
            "Codex {} is not in the exact validated set or lacks required isolation flags; no onboarding settings were written",
            isolation.codex_version
        )));
    }
    let worker_model = match option_value(arguments, "--worker-model") {
        Some(model) => model,
        None => select_worker_model(&codex_executable)?,
    };
    let worker_reasoning = option_value(arguments, "--worker-reasoning")
        .unwrap_or_else(|| DEFAULT_REASONING.to_owned());
    validate_model_value(&worker_model, "worker model")?;
    validate_reasoning(&worker_reasoning)?;
    let worker_timeout_seconds = option_value(arguments, "--worker-timeout-seconds")
        .map(|value| parse_positive_u64(&value, "--worker-timeout-seconds"))
        .transpose()?
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    if worker_timeout_seconds > 180 {
        return Err(AppError::Usage(
            "--worker-timeout-seconds must be between 1 and 180".to_owned(),
        ));
    }
    let settings = RuntimeSettings {
        codex_executable,
        worker_model,
        worker_reasoning,
        worker_timeout_seconds,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        trusted_test_execution: false,
        multi_need_policy: needle_core::MultiNeedPolicy::default(),
    };
    store.initialize_defaults(&settings).map_err(runtime_error)?;
    Ok(settings)
}

fn select_worker_model(codex_executable: &str) -> Result<String, AppError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(AppError::Usage(
            "first enable requires --worker-model when stdin is not interactive".to_owned(),
        ));
    }

    let models = discover_codex_models(codex_executable)?;
    if models.is_empty() {
        return Err(AppError::Runtime(
            "Codex returned no available models; sign in to Codex and retry, or provide --worker-model"
                .to_owned(),
        ));
    }

    let default_index = models.iter().position(|model| model.is_default).unwrap_or(0);
    let items = model_picker_items(&models);
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a Codex model")
        .items(&items)
        .default(default_index)
        .interact_opt()
        .map_err(|error| AppError::Runtime(format!("model selection failed: {error}")))?
        .ok_or_else(|| AppError::Usage("model selection was cancelled".to_owned()))?;
    Ok(models[selection].model.clone())
}

fn model_picker_items(models: &[AvailableCodexModel]) -> Vec<String> {
    models
        .iter()
        .map(|model| {
            let recommended = if model.is_default { " - recommended" } else { "" };
            let description = if model.description.is_empty() {
                String::new()
            } else {
                format!(" - {}", model.description)
            };
            format!("{} ({}){recommended}{description}", model.display_name, model.model)
        })
        .collect()
}

fn discover_codex_models(codex_executable: &str) -> Result<Vec<AvailableCodexModel>, AppError> {
    let mut child = Command::new(codex_executable)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            AppError::Runtime(format!(
                "cannot start the Codex runtime to list available models: {error}"
            ))
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::Runtime("cannot open the Codex app-server input".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Runtime("cannot open the Codex app-server output".to_owned()))?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    let result = (|| {
        write_app_server_message(
            &mut stdin,
            &json!({
                "method": "initialize",
                "id": 1,
                "params": {
                    "clientInfo": {
                        "name": "needle",
                        "title": "Needle",
                        "version": VERSION
                    }
                }
            }),
        )?;
        read_app_server_response(&receiver, 1)?;
        write_app_server_message(&mut stdin, &json!({ "method": "initialized", "params": {} }))?;

        let mut models = Vec::new();
        let mut cursor: Option<String> = None;
        for page in 0..MAX_MODEL_LIST_PAGES {
            let request_id = page as u64 + 2;
            write_app_server_message(
                &mut stdin,
                &json!({
                    "method": "model/list",
                    "id": request_id,
                    "params": {
                        "cursor": cursor,
                        "limit": MODEL_LIST_LIMIT,
                        "includeHidden": false
                    }
                }),
            )?;
            let response = read_app_server_response(&receiver, request_id)?;
            models.extend(parse_codex_models(&response)?);
            cursor = response.get("nextCursor").and_then(Value::as_str).map(str::to_owned);
            if cursor.is_none() {
                return Ok(models);
            }
        }
        Err(AppError::Runtime(format!(
            "Codex returned more than {MAX_MODEL_LIST_PAGES} pages of models"
        )))
    })();

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    result
}

fn write_app_server_message(writer: &mut impl Write, message: &Value) -> Result<(), AppError> {
    writeln!(writer, "{message}")?;
    writer.flush()?;
    Ok(())
}

fn read_app_server_response(
    receiver: &Receiver<io::Result<String>>,
    request_id: u64,
) -> Result<Value, AppError> {
    let deadline = Instant::now() + CODEX_APP_SERVER_TIMEOUT;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(model_list_timeout());
        };
        let line = match receiver.recv_timeout(remaining) {
            Ok(line) => line?,
            Err(RecvTimeoutError::Timeout) => return Err(model_list_timeout()),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(AppError::Runtime(
                    "Codex stopped before returning the available models".to_owned(),
                ));
            }
        };
        let message: Value = serde_json::from_str(&line).map_err(|error| {
            AppError::Runtime(format!("Codex returned an invalid app-server response: {error}"))
        })?;
        if message.get("id").and_then(Value::as_u64) != Some(request_id) {
            continue;
        }
        if let Some(error) = message.get("error") {
            let detail =
                error.get("message").and_then(Value::as_str).unwrap_or("unknown app-server error");
            return Err(AppError::Runtime(format!(
                "Codex could not list the available models: {detail}"
            )));
        }
        return message.get("result").cloned().ok_or_else(|| {
            AppError::Runtime("Codex returned a response without a result".to_owned())
        });
    }
}

fn model_list_timeout() -> AppError {
    AppError::Runtime(
        "timed out while asking Codex for the available models; retry or provide --worker-model"
            .to_owned(),
    )
}

fn parse_codex_models(result: &Value) -> Result<Vec<AvailableCodexModel>, AppError> {
    let entries = result
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Runtime("Codex returned a model list without data".to_owned()))?;
    entries
        .iter()
        .map(|entry| {
            let model = entry.get("model").and_then(Value::as_str).ok_or_else(|| {
                AppError::Runtime("Codex returned a model without an identifier".to_owned())
            })?;
            Ok(AvailableCodexModel {
                model: model.to_owned(),
                display_name: entry
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(model)
                    .to_owned(),
                description: entry
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                is_default: entry.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

pub(crate) fn ensure_default_profile(
    store: &RuntimeStore,
    settings: &RuntimeSettings,
    max_cost_microusd: u64,
) -> Result<RoleProfileId, AppError> {
    let profile_id = RoleProfileId::new(DEFAULT_PROFILE_ID)
        .map_err(|error| AppError::Runtime(error.to_string()))?;
    let definition = default_profile_definition(profile_id.clone(), settings, max_cost_microusd)?;
    match store.role_profile_state(&profile_id) {
        Ok(state) => {
            if state.active_revision.is_some() {
                return Ok(profile_id);
            }
            let stored = store
                .read_role_profile_revision(&profile_id, state.latest_revision)
                .map_err(runtime_error)?;
            if stored.definition.definition_digest != definition.definition_digest {
                return Err(AppError::Runtime(format!(
                    "role profile `{DEFAULT_PROFILE_ID}` exists as an incompatible inactive draft; review it in the control plane before enabling Needle"
                )));
            }
            store
                .activate_role_profile(&profile_id, state.latest_revision, state.state_digest)
                .map_err(runtime_error)?;
        }
        Err(StoreError::RoleProfileNotFound(_)) => {
            let revision = store.create_role_profile(definition).map_err(runtime_error)?;
            let state = store.role_profile_state(&profile_id).map_err(runtime_error)?;
            store
                .activate_role_profile(&profile_id, revision.revision, state.state_digest)
                .map_err(runtime_error)?;
        }
        Err(error) => return Err(runtime_error(error)),
    }
    Ok(profile_id)
}

fn default_profile_definition(
    profile_id: RoleProfileId,
    settings: &RuntimeSettings,
    max_cost_microusd: u64,
) -> Result<RoleProfileDefinition, AppError> {
    let reasoning = match settings.worker_reasoning.as_str() {
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        "xhigh" => ReasoningLevel::Xhigh,
        value => {
            return Err(AppError::Runtime(format!(
                "stored worker reasoning `{value}` cannot be represented by an explorer role profile"
            )));
        }
    };
    let prompt_profile_digest = HookConfig::default().profile()?.definition_digest;
    RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id,
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: settings.worker_model.clone(),
        reasoning,
        service_tier: ServiceTier::Default,
        timeout_seconds: settings.worker_timeout_seconds,
        budget: RoleProfileBudget { max_turns: 2, max_output_tokens: 1200, max_cost_microusd },
        prompt_profile_digest,
        output_contract_digest: Digest::blake3(needle_core::ARTIFACT_RESULT_SCHEMA_ID),
        tool_policy: ToolPolicy::ReadOnly,
        command_policy: CommandPolicy::ReadOnly,
        filesystem_policy: FilesystemPolicy::ReadOnlyCheckout,
        network_policy: NetworkPolicy::Denied,
        test_policy: TestPolicy::Disabled,
        repair_policy: RepairPolicy::None,
        fallback_policy: FallbackPolicy::Native,
        concurrency: 1,
        route_assignments: vec![
            NeedKey::new("locate.implementation").expect("built-in route is valid"),
            NeedKey::new("tests.relevant").expect("built-in route is valid"),
            NeedKey::new("trace.state-flow").expect("built-in route is valid"),
        ],
    })
    .map_err(|error| AppError::Runtime(error.to_string()))
}

fn repository_root(arguments: &[String]) -> Result<PathBuf, AppError> {
    let candidate =
        option_value(arguments, "--repository").map(PathBuf::from).unwrap_or(env::current_dir()?);
    capture_git_snapshot(&candidate).map(|(root, _)| root).map_err(|error| {
        AppError::Runtime(format!(
            "cannot resolve a trusted Git repository from {}: {error}",
            candidate.display()
        ))
    })
}

fn activation_context(arguments: &[String], global: bool) -> Result<PathBuf, AppError> {
    if !global {
        return repository_root(arguments);
    }
    let candidate =
        option_value(arguments, "--repository").map(PathBuf::from).unwrap_or(env::current_dir()?);
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
        AppError::Runtime(format!(
            "cannot resolve activation context {}: {error}",
            candidate.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(AppError::Runtime(format!(
            "activation context is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn parse_positive_u64(value: &str, name: &str) -> Result<u64, AppError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|error| AppError::Usage(format!("invalid {name}: {error}")))?;
    if parsed == 0 {
        return Err(AppError::Usage(format!("{name} must be positive")));
    }
    Ok(parsed)
}

fn validate_arguments(arguments: &[String], action: Action) -> Result<(), AppError> {
    let value_options: &[&str] = match action {
        Action::Enable => &[
            "--data-dir",
            "--repository",
            "--codex",
            "--worker-model",
            "--worker-reasoning",
            "--worker-timeout-seconds",
        ],
        Action::Disable | Action::Status | Action::Ui => &["--data-dir", "--repository"],
    };
    let allow_global = matches!(action, Action::Enable | Action::Disable);
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        if value_options.contains(&argument) {
            if arguments.get(index + 1).is_none() {
                return Err(AppError::Usage(format!("{argument} requires a value")));
            }
            index += 2;
        } else if (argument == "--global" && allow_global)
            || ((argument == "--json" || argument == "--no-color")
                && matches!(action, Action::Enable | Action::Disable | Action::Status))
        {
            index += 1;
        } else {
            return Err(AppError::Usage(format!(
                "unknown {} argument `{argument}`",
                action.as_str()
            )));
        }
    }
    Ok(())
}

fn has_flag(arguments: &[String], name: &str) -> bool {
    arguments.iter().any(|argument| argument == name)
}

fn help_requested(arguments: &[String]) -> bool {
    matches!(arguments, [argument] if argument == "--help" || argument == "-h")
}

fn option_value(arguments: &[String], name: &str) -> Option<String> {
    arguments.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].clone())
}

fn runtime_error(error: StoreError) -> AppError {
    AppError::Runtime(error.to_string())
}

#[derive(Clone, Copy)]
enum Action {
    Enable,
    Disable,
    Status,
    Ui,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Status => "status",
            Self::Ui => "ui",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn integration_status(cli_registered: bool, desktop_ready: bool) -> CodexIntegrationStatus {
        CodexIntegrationStatus {
            cli_hooks: Some(crate::codex_hooks::HookRegistration {
                path: PathBuf::from(r"C:\Users\user\.codex\hooks.json"),
                registered: cli_registered,
                changed: cli_registered,
                removed: false,
                trust_review_required: cli_registered,
            }),
            cli_error: None,
            desktop_skill: Some(crate::codex_skill::SkillInstallation {
                path: PathBuf::from(r"C:\Users\user\.agents\skills\needle\SKILL.md"),
                installed: desktop_ready,
                managed: desktop_ready,
                current: desktop_ready,
                changed: desktop_ready,
            }),
            desktop_error: None,
            stale_cli_hooks_removed: false,
        }
    }

    #[test]
    fn activation_argument_surface_is_closed() {
        assert!(validate_arguments(&["--global".to_owned()], Action::Enable).is_ok());
        assert!(validate_arguments(&["--global".to_owned()], Action::Status).is_err());
        assert!(validate_arguments(&["--json".to_owned()], Action::Status).is_ok());
        assert!(validate_arguments(&["--no-color".to_owned()], Action::Status).is_ok());
        assert!(validate_arguments(&["--json".to_owned()], Action::Enable).is_ok());
        assert!(validate_arguments(&["--no-color".to_owned()], Action::Enable).is_ok());
        assert!(validate_arguments(&["--json".to_owned()], Action::Disable).is_ok());
        assert!(
            validate_arguments(
                &["--max-cost-microusd".to_owned(), "1000000".to_owned()],
                Action::Enable
            )
            .is_err()
        );
        assert!(validate_arguments(&["--network".to_owned()], Action::Enable).is_err());
        assert!(validate_arguments(&["--repository".to_owned()], Action::Disable).is_err());
    }

    #[test]
    fn enable_success_is_human_readable_and_actionable() {
        let installations = CodexInstallations {
            cli: DetectedInstallation::found(Some("0.144.0".to_owned())),
            desktop: DetectedInstallation::found(Some("26.727.6591.0".to_owned())),
            managed: DetectedInstallation::found(Some("0.144.0".to_owned())),
        };
        let integrations = integration_status(true, true);
        let output = render_enable_success(
            &EnableSummary {
                repository: std::path::Path::new(r"\\?\C:\work\needle"),
                global: false,
                worker_model: "gpt-5.6-terra",
                profile_id: DEFAULT_PROFILE_ID,
                codex_version: "0.144.0",
                installations: &installations,
                integrations: &integrations,
            },
            false,
        );

        assert!(output.starts_with("Needle enabled."));
        assert!(output.contains(r"Repository:     C:\work\needle"));
        assert!(output.contains("Worker model:   gpt-5.6-terra"));
        assert!(output.contains("Codex runtime:  0.144.0 - Verified"));
        assert!(output.contains("Codex CLI:      Found (0.144.0)"));
        assert!(output.contains("Hooks registered"));
        assert!(output.contains("Codex Desktop:  Found (26.727.6591.0)"));
        assert!(output.contains("Needle skill ready"));
        assert!(output.contains("run `/hooks`"));
        assert!(!output.trim_start().starts_with('{'));
    }

    #[test]
    fn uninitialized_status_is_human_readable() {
        let needle = NeedleStatus {
            version: "0.1.0".to_owned(),
            update: UpdateAvailability::NoPublishedRelease,
        };
        let mut installations = CodexInstallations {
            cli: DetectedInstallation::not_found(),
            desktop: DetectedInstallation::found(Some("26.727.6591.0".to_owned())),
            managed: DetectedInstallation::found(Some("0.144.0".to_owned())),
        };
        installations.desktop.update = UpdateAvailability::UpToDate;
        installations.managed.update = UpdateAvailability::ManagedByNeedle;
        let integrations = integration_status(false, true);
        let output = render_uninitialized_status(
            std::path::Path::new(r"\\?\C:\work\needle"),
            &needle,
            &installations,
            &integrations,
            false,
        );
        assert!(output.contains("Needle is disabled."));
        assert!(output.contains(r"Repository:      C:\work\needle"));
        assert!(output.contains("Setup:           Not completed"));
        assert!(output.contains("Needle:          0.1.0 - Development build"));
        assert!(output.contains("Codex CLI:       Not found"));
        assert!(
            output.contains(
                "Codex Desktop:   Found (26.727.6591.0) - Up to date - Needle skill ready"
            )
        );
        assert!(
            output.contains("Worker runtime:  Found (0.144.0) - Ready and validated by Needle")
        );
        assert!(!output.trim_start().starts_with('{'));
    }

    #[test]
    fn command_version_parser_ignores_product_name() {
        assert_eq!(parse_command_version("codex-cli 0.144.0\n"), Some("0.144.0".to_owned()));
        assert_eq!(parse_command_version("not a version"), None);
    }

    #[test]
    fn codex_model_list_is_parsed_for_the_picker() {
        let models = parse_codex_models(&json!({
            "data": [
                {
                    "model": "gpt-5.6-sol",
                    "displayName": "GPT-5.6-Sol",
                    "description": "Latest frontier agentic coding model.",
                    "isDefault": true
                },
                {
                    "model": "gpt-5.6-terra",
                    "displayName": "GPT-5.6-Terra",
                    "description": "Balanced agentic coding model for everyday work.",
                    "isDefault": false
                }
            ]
        }))
        .unwrap();

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].model, "gpt-5.6-sol");
        assert_eq!(models[0].display_name, "GPT-5.6-Sol");
        assert!(models[0].is_default);
        assert_eq!(models[1].model, "gpt-5.6-terra");
    }

    #[test]
    fn model_picker_items_explain_each_choice() {
        let items = model_picker_items(&[
            AvailableCodexModel {
                model: "gpt-5.6-sol".to_owned(),
                display_name: "GPT-5.6-Sol".to_owned(),
                description: "Latest frontier agentic coding model.".to_owned(),
                is_default: true,
            },
            AvailableCodexModel {
                model: "gpt-5.6-terra".to_owned(),
                display_name: "GPT-5.6-Terra".to_owned(),
                description: String::new(),
                is_default: false,
            },
        ]);

        assert_eq!(
            items[0],
            "GPT-5.6-Sol (gpt-5.6-sol) - recommended - Latest frontier agentic coding model."
        );
        assert_eq!(items[1], "GPT-5.6-Terra (gpt-5.6-terra)");
    }

    #[test]
    fn release_lookup_and_version_comparison_are_strict() {
        assert_eq!(
            parse_release_response(b"{\"tag_name\":\"rust-v0.146.0\"}\n200"),
            ReleaseLookup::Version("0.146.0".to_owned())
        );
        assert_eq!(parse_release_response(b"{}\n404"), ReleaseLookup::NotPublished);
        assert_eq!(compare_versions("0.144.0", "0.146.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("0.146", "0.146.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("preview", "0.146.0"), None);
    }

    #[test]
    fn update_available_is_colored_only_when_requested() {
        let installation = DetectedInstallation {
            found: true,
            supported_on_platform: true,
            version: Some("0.144.0".to_owned()),
            update: UpdateAvailability::Available,
        };
        let plain = installation.human_summary(false, false);
        let colored = installation.human_summary(true, false);
        assert_eq!(plain, "Found (0.144.0) - Update available");
        assert!(colored.contains("\u{1b}[32m"));
        assert!(colored.contains("\u{1b}[33m"));
        assert!(!plain.contains('\u{1b}'));
    }

    #[cfg(windows)]
    #[test]
    fn windows_desktop_version_is_parsed_from_package_key() {
        let output =
            r"HKEY_CURRENT_USER\Software\Classes\OpenAI.Codex_26.727.6591.0_x64__2p2nqsd0c76g0";
        assert_eq!(parse_windows_codex_package_version(output), Some("26.727.6591.0".to_owned()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_desktop_uses_the_apps_update_manifest() {
        let manifest = br#"{
            "schemaVersion": 1,
            "buildVersion": "26.730.7989.0",
            "storeProductId": "9PLM9XGG6VKS",
            "packageIdentity": "OpenAI.Codex"
        }"#;
        assert!(matches!(
            parse_windows_desktop_update_manifest(
                manifest,
                Some("26.727.6591.0"),
                "9PLM9XGG6VKS",
                "OpenAI.Codex"
            ),
            UpdateAvailability::Available
        ));
        assert!(matches!(
            parse_windows_desktop_update_manifest(
                manifest,
                Some("26.730.7989.0"),
                "9PLM9XGG6VKS",
                "OpenAI.Codex"
            ),
            UpdateAvailability::UpToDate
        ));
        assert!(matches!(
            parse_windows_desktop_update_manifest(
                manifest,
                Some("26.727.6591.0"),
                "unexpected-product",
                "OpenAI.Codex"
            ),
            UpdateAvailability::Unknown
        ));
    }

    #[test]
    fn unsupported_desktop_platform_is_explicit() {
        let installation = DetectedInstallation {
            found: false,
            supported_on_platform: false,
            version: None,
            update: UpdateAvailability::NotApplicable,
        };
        assert!(installation.human_summary(false, false).starts_with("Not available on "));
    }

    #[test]
    fn default_profile_is_read_only_and_bounded() {
        let profile = default_profile_definition(
            RoleProfileId::new(DEFAULT_PROFILE_ID).unwrap(),
            &RuntimeSettings {
                codex_executable: "codex".to_owned(),
                worker_model: "gpt-5-mini".to_owned(),
                worker_reasoning: "medium".to_owned(),
                worker_timeout_seconds: 120,
                evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
                trusted_test_execution: false,
                multi_need_policy: needle_core::MultiNeedPolicy::default(),
            },
            1000,
        )
        .unwrap();
        assert_eq!(profile.tool_policy, ToolPolicy::ReadOnly);
        assert_eq!(profile.network_policy, NetworkPolicy::Denied);
        assert_eq!(profile.test_policy, TestPolicy::Disabled);
        assert_eq!(profile.fallback_policy, FallbackPolicy::Native);
        assert_eq!(profile.budget.max_cost_microusd, 1000);
    }

    #[test]
    fn global_activation_context_does_not_require_git() {
        let root = std::env::temp_dir()
            .join(format!("needle-global-context-{}", crate::server::test_nonce()));
        std::fs::create_dir_all(&root).unwrap();
        let context = activation_context(
            &["--global".to_owned(), "--repository".to_owned(), root.display().to_string()],
            true,
        )
        .unwrap();
        assert_eq!(context, std::fs::canonicalize(&root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
}
