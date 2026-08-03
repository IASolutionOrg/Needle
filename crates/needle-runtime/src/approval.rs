use needle_core::{
    ApprovalDecision, ApprovalDecisionSource, ApprovalRequest, CommandClassification,
    CommandExecutionEvidence, Digest, ReadOnlyCommandPolicy, RequestedPermissions, TestCommand,
    TestCommandPolicy, TestPlan,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const COMMAND_METACHARACTERS: &[char] = &['|', '&', ';', '>', '<', '`', '\n', '\r'];
const MAX_READ_ONLY_COMMAND_BYTES: usize = 4096;
const MAX_READ_ONLY_ARGUMENTS: usize = 64;
const MAX_GET_CONTENT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ApprovalContext {
    pub route: String,
    pub repository_id: Digest,
    pub checkout_root: PathBuf,
    pub target_root: PathBuf,
    pub temp_root: PathBuf,
    pub test_plan: Option<TestPlan>,
    pub test_execution_available: bool,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ApprovalError {
    #[error("command is not a direct analyzable argv")]
    Unparseable,
    #[error("command working directory is outside the isolated checkout")]
    CwdOutsideCheckout,
    #[error("test policy execution budget is exhausted")]
    BudgetExhausted,
    #[error("test evidence does not match the declared argv")]
    EvidenceArgvMismatch,
    #[error("test evidence reports an unsuccessful exit")]
    EvidenceExitFailure,
    #[error("test evidence does not identify the required test")]
    EvidenceIdentifier,
    #[error("test evidence does not prove at least one test ran")]
    EvidenceNoTests,
    #[error("test plan is not a canonical safe focused-test command")]
    InvalidTestPlan,
}

#[derive(Debug, Default)]
pub struct ApprovalBroker {
    test_policies: Vec<TestCommandPolicy>,
    read_only_policies: Vec<ReadOnlyCommandPolicy>,
    executions: BTreeMap<String, u32>,
}

impl ApprovalBroker {
    pub fn new(policies: Vec<TestCommandPolicy>) -> Self {
        Self {
            test_policies: policies,
            read_only_policies: Vec::new(),
            executions: BTreeMap::new(),
        }
    }

    pub fn with_read_only_policies(mut self, policies: Vec<ReadOnlyCommandPolicy>) -> Self {
        self.read_only_policies = policies;
        self
    }

    pub fn classify_command(
        &mut self,
        command_display: Option<&str>,
        command_action: Option<&str>,
        cwd: &Path,
        permissions: &RequestedPermissions,
        context: &ApprovalContext,
    ) -> (Vec<String>, CommandClassification) {
        if context.test_plan.as_ref().is_some_and(|plan| plan.test_command().is_err()) {
            return (Vec::new(), CommandClassification::RejectedPolicyMismatch);
        }
        if permissions.network {
            return (Vec::new(), CommandClassification::RejectedNetwork);
        }
        if !path_is_within(cwd, &context.checkout_root) {
            return (Vec::new(), CommandClassification::RejectedPolicyMismatch);
        }
        if permissions
            .write_paths
            .iter()
            .any(|path| !write_path_allowed(path, &context.target_root, &context.temp_root))
        {
            return (Vec::new(), CommandClassification::RejectedFileChange);
        }
        if let Some(plan) = context.test_plan.as_ref()
            && !context.test_execution_available
            && same_canonical_path(cwd, &context.checkout_root.join(&plan.cwd_relative))
            && [command_display, command_action].into_iter().flatten().any(|command| {
                parse_test_command_argv(command, &plan.argv).is_ok_and(|argv| argv == plan.argv)
            })
        {
            return (plan.argv.clone(), CommandClassification::RejectedPolicyMismatch);
        }
        let (Some(command_display), Some(command_action)) = (command_display, command_action)
        else {
            return (Vec::new(), CommandClassification::PendingUser);
        };
        if let Some(plan) = context.test_plan.as_ref() {
            let parsed_test = (
                parse_test_command_argv(command_display, &plan.argv),
                parse_test_command_argv(command_action, &plan.argv),
            );
            if let (Ok(display_argv), Ok(argv)) = parsed_test {
                if display_argv != argv {
                    return (Vec::new(), CommandClassification::PendingUser);
                }
                let expected_cwd = context.checkout_root.join(&plan.cwd_relative);
                if argv == plan.argv
                    && same_canonical_path(cwd, &expected_cwd)
                    && let Some(policy) = self.test_policies.iter().find(|policy| {
                        policy.trusted
                            && policy.repository_id == context.repository_id
                            && argv.first() == Some(&policy.executable)
                            && argv.starts_with(&policy.argv_prefix)
                    })
                {
                    let count = self.executions.entry(policy.id.clone()).or_default();
                    if *count >= policy.maximum_executions_per_worker {
                        return (argv, CommandClassification::RejectedPolicyMismatch);
                    }
                    *count += 1;
                    return (
                        argv,
                        CommandClassification::AutoApprovedTest { policy_id: policy.id.clone() },
                    );
                }
            }
        }

        let (Ok(display_argv), Ok(argv)) = (
            parse_read_only_command_argv(command_display),
            parse_read_only_command_argv(command_action),
        ) else {
            return (
                parse_direct_argv(command_action).unwrap_or_default(),
                CommandClassification::PendingUser,
            );
        };
        if display_argv != argv
            || !permissions.write_paths.is_empty()
            || !read_permissions_are_within(permissions, &context.checkout_root)
            || !read_only_argv_is_allowed(&argv, cwd, &context.checkout_root)
        {
            return (argv, CommandClassification::PendingUser);
        }
        let Some(policy) = self
            .read_only_policies
            .iter()
            .find(|policy| policy.trusted && policy.repository_id == context.repository_id)
        else {
            return (argv, CommandClassification::PendingUser);
        };
        let count = self.executions.entry(policy.id.clone()).or_default();
        if *count >= policy.maximum_executions_per_worker {
            return (argv, CommandClassification::RejectedPolicyMismatch);
        }
        *count += 1;
        (argv, CommandClassification::AutoApprovedReadOnly { policy_id: policy.id.clone() })
    }

    pub fn automatic_decision(
        classification: &CommandClassification,
    ) -> Option<(ApprovalDecision, ApprovalDecisionSource)> {
        match classification {
            CommandClassification::AutoApprovedTest { .. }
            | CommandClassification::AutoApprovedReadOnly { .. } => {
                Some((ApprovalDecision::Accept, ApprovalDecisionSource::AutoPolicy))
            }
            CommandClassification::RejectedFileChange
            | CommandClassification::RejectedNetwork
            | CommandClassification::RejectedUnparseable
            | CommandClassification::RejectedPolicyMismatch
            | CommandClassification::Expired => {
                Some((ApprovalDecision::Decline, ApprovalDecisionSource::Runtime))
            }
            CommandClassification::PendingUser => None,
        }
    }
}

fn same_canonical_path(left: &Path, right: &Path) -> bool {
    matches!(
        (fs::canonicalize(left), fs::canonicalize(right)),
        (Ok(left), Ok(right)) if left == right
    )
}

pub fn validate_test_evidence(
    plan: &TestPlan,
    evidence: &CommandExecutionEvidence,
) -> Result<(), ApprovalError> {
    let plan_command = plan.test_command().map_err(|_| ApprovalError::InvalidTestPlan)?;
    let evidence_command =
        TestCommand::from_canonical_parts(&evidence.runner, &evidence.argv, &plan.test_identifier)
            .map_err(|_| ApprovalError::EvidenceArgvMismatch)?;
    if plan_command != evidence_command {
        return Err(ApprovalError::EvidenceArgvMismatch);
    }
    if evidence.exit_status != Some(0) || evidence.infrastructure_failure.is_some() {
        return Err(ApprovalError::EvidenceExitFailure);
    }
    if evidence.test_identifier.as_deref() != Some(plan.test_identifier.as_str())
        || !test_output_observes_identifier(&evidence.output_preview, &plan.test_identifier)
    {
        return Err(ApprovalError::EvidenceIdentifier);
    }
    if evidence.tests_executed.unwrap_or(0) == 0 {
        return Err(ApprovalError::EvidenceNoTests);
    }
    Ok(())
}

pub fn parse_direct_argv(command: &str) -> Result<Vec<String>, ApprovalError> {
    let command = command.trim();
    if command.is_empty()
        || command.chars().any(|character| {
            COMMAND_METACHARACTERS.contains(&character)
                || character.is_control()
                || matches!(character, '"' | '\'' | '$')
        })
    {
        return Err(ApprovalError::Unparseable);
    }
    let argv = command.split_ascii_whitespace().map(str::to_owned).collect::<Vec<_>>();
    if argv.is_empty()
        || argv.iter().any(|argument| {
            argument.contains('=')
                || matches!(
                    argument.to_ascii_lowercase().as_str(),
                    "cmd" | "cmd.exe" | "powershell" | "powershell.exe" | "pwsh" | "sh" | "bash"
                )
        })
    {
        return Err(ApprovalError::Unparseable);
    }
    Ok(argv)
}

pub fn parse_test_command_argv(
    command: &str,
    expected_argv: &[String],
) -> Result<Vec<String>, ApprovalError> {
    if let Ok(argv) = parse_direct_argv(command) {
        return Ok(argv);
    }
    let expected_command = shell_safe_command(expected_argv).ok_or(ApprovalError::Unparseable)?;
    let (executable, arguments) =
        split_shell_executable(command).ok_or(ApprovalError::Unparseable)?;
    let shell =
        executable.replace('\\', "/").rsplit('/').next().unwrap_or_default().to_ascii_lowercase();
    let (flags, quote, payload) =
        split_shell_payload(arguments, &expected_command).ok_or(ApprovalError::Unparseable)?;
    let flags = flags.split_ascii_whitespace().map(str::to_ascii_lowercase).collect::<Vec<_>>();
    let supported = match shell.as_str() {
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => {
            matches!(quote, None | Some('\'') | Some('"')) && valid_powershell_flags(&flags)
        }
        "cmd" | "cmd.exe" => {
            matches!(quote, None | Some('"'))
                && (flags_equal(&flags, &["/c"]) || flags_equal(&flags, &["/d", "/s", "/c"]))
        }
        "sh" | "sh.exe" | "bash" | "bash.exe" | "zsh" | "zsh.exe" => {
            matches!(quote, Some('\'') | Some('"'))
                && (flags_equal(&flags, &["-c"])
                    || flags_equal(&flags, &["-lc"])
                    || flags_equal(&flags, &["-l", "-c"]))
        }
        _ => false,
    };
    if supported && payload == expected_command {
        Ok(expected_argv.to_vec())
    } else {
        Err(ApprovalError::Unparseable)
    }
}

pub fn parse_read_only_command_argv(command: &str) -> Result<Vec<String>, ApprovalError> {
    if let Ok(argv) = parse_bounded_argv(command)
        && read_only_executable(&argv).is_some()
    {
        return Ok(argv);
    }
    let (executable, arguments) =
        split_shell_executable(command).ok_or(ApprovalError::Unparseable)?;
    let shell =
        executable.replace('\\', "/").rsplit('/').next().unwrap_or_default().to_ascii_lowercase();
    let (flags, payload) =
        split_read_only_shell_payload(arguments).ok_or(ApprovalError::Unparseable)?;
    let flags = flags.split_ascii_whitespace().map(str::to_ascii_lowercase).collect::<Vec<_>>();
    let supported = match shell.as_str() {
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => valid_powershell_flags(&flags),
        "cmd" | "cmd.exe" => {
            flags_equal(&flags, &["/c"]) || flags_equal(&flags, &["/d", "/s", "/c"])
        }
        "sh" | "sh.exe" | "bash" | "bash.exe" | "zsh" | "zsh.exe" => {
            flags_equal(&flags, &["-c"])
                || flags_equal(&flags, &["-lc"])
                || flags_equal(&flags, &["-l", "-c"])
        }
        _ => false,
    };
    let argv = parse_bounded_argv(payload)?;
    if supported && read_only_executable(&argv).is_some() {
        Ok(argv)
    } else {
        Err(ApprovalError::Unparseable)
    }
}

fn parse_bounded_argv(command: &str) -> Result<Vec<String>, ApprovalError> {
    let command = command.trim();
    if command.is_empty() || command.len() > MAX_READ_ONLY_COMMAND_BYTES {
        return Err(ApprovalError::Unparseable);
    }
    let mut argv = Vec::new();
    let mut argument = String::new();
    let mut quote = None;
    for character in command.chars() {
        match quote {
            Some(active) if character == active => quote = None,
            Some('"') if matches!(character, '$' | '`' | '%') => {
                return Err(ApprovalError::Unparseable);
            }
            Some(_) => argument.push(character),
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if character.is_ascii_whitespace() => {
                if !argument.is_empty() {
                    argv.push(std::mem::take(&mut argument));
                }
            }
            None if COMMAND_METACHARACTERS.contains(&character)
                || character.is_control()
                || matches!(character, '$' | '%' | '(' | ')' | '{' | '}') =>
            {
                return Err(ApprovalError::Unparseable);
            }
            None => argument.push(character),
        }
        if argv.len() >= MAX_READ_ONLY_ARGUMENTS {
            return Err(ApprovalError::Unparseable);
        }
    }
    if quote.is_some() {
        return Err(ApprovalError::Unparseable);
    }
    if !argument.is_empty() {
        argv.push(argument);
    }
    if argv.is_empty() || argv.len() > MAX_READ_ONLY_ARGUMENTS {
        return Err(ApprovalError::Unparseable);
    }
    Ok(argv)
}

fn split_read_only_shell_payload(arguments: &str) -> Option<(&str, &str)> {
    let quote_start = arguments.find(['\'', '"'])?;
    let flags = arguments[..quote_start].trim();
    let quoted = arguments[quote_start..].trim();
    let quote = quoted.chars().next()?;
    if flags.is_empty() || quoted.len() < 2 || !quoted.ends_with(quote) {
        return None;
    }
    let payload = &quoted[1..quoted.len() - 1];
    (!payload.is_empty() && !payload.contains(quote)).then_some((flags, payload))
}

fn read_only_executable(argv: &[String]) -> Option<&str> {
    let executable = argv.first()?.replace('\\', "/");
    let executable = executable.rsplit('/').next()?.to_ascii_lowercase();
    match executable.as_str() {
        "rg" | "rg.exe" => Some("rg"),
        "get-content" => Some("get-content"),
        _ => None,
    }
}

fn read_only_argv_is_allowed(argv: &[String], cwd: &Path, checkout_root: &Path) -> bool {
    match read_only_executable(argv) {
        Some("rg") => rg_argv_is_allowed(argv, cwd, checkout_root),
        Some("get-content") => get_content_argv_is_allowed(argv, cwd, checkout_root),
        _ => false,
    }
}

fn rg_argv_is_allowed(argv: &[String], cwd: &Path, checkout_root: &Path) -> bool {
    let mut index = 1;
    let mut pattern_declared = false;
    let mut files_mode = false;
    let mut paths = Vec::new();
    while index < argv.len() {
        let argument = &argv[index];
        if argument == "--" {
            index += 1;
            while index < argv.len() {
                if files_mode || pattern_declared {
                    paths.push(argv[index].as_str());
                } else {
                    pattern_declared = true;
                }
                index += 1;
            }
            break;
        }
        let normalized = argument.to_ascii_lowercase();
        if normalized == "--files" {
            files_mode = true;
            index += 1;
            continue;
        }
        if matches!(argument.as_str(), "-n" | "-S" | "-s" | "-i" | "-F" | "-w" | "-x")
            || matches!(
                normalized.as_str(),
                "--line-number"
                    | "--smart-case"
                    | "--ignore-case"
                    | "--case-sensitive"
                    | "--fixed-strings"
                    | "--word-regexp"
                    | "--line-regexp"
                    | "--heading"
                    | "--no-heading"
                    | "--stats"
            )
            || normalized == "--color=never"
            || is_safe_combined_rg_flag(argument)
        {
            index += 1;
            continue;
        }
        let takes_value =
            matches!(argument.as_str(), "-e" | "-g" | "-t" | "-T" | "-A" | "-B" | "-C" | "-m")
                || matches!(
                    normalized.as_str(),
                    "--regexp"
                        | "--glob"
                        | "--type"
                        | "--type-not"
                        | "--after-context"
                        | "--before-context"
                        | "--context"
                        | "--max-count"
                        | "--max-columns"
                );
        if takes_value {
            index += 1;
            let Some(value) = argv.get(index) else {
                return false;
            };
            if argument == "-e" || normalized == "--regexp" {
                pattern_declared = true;
            } else if (matches!(argument.as_str(), "-A" | "-B" | "-C" | "-m")
                || matches!(
                    normalized.as_str(),
                    "--after-context"
                        | "--before-context"
                        | "--context"
                        | "--max-count"
                        | "--max-columns"
                ))
                && !bounded_count(value)
            {
                return false;
            }
            index += 1;
            continue;
        }
        if let Some((flag, value)) = normalized.split_once('=')
            && matches!(
                flag,
                "--regexp"
                    | "--glob"
                    | "--type"
                    | "--type-not"
                    | "--after-context"
                    | "--before-context"
                    | "--context"
                    | "--max-count"
                    | "--max-columns"
            )
        {
            if flag == "--regexp" {
                pattern_declared = true;
            } else if matches!(
                flag,
                "--after-context"
                    | "--before-context"
                    | "--context"
                    | "--max-count"
                    | "--max-columns"
            ) && !bounded_count(value)
            {
                return false;
            }
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            return false;
        }
        if files_mode || pattern_declared {
            paths.push(argument);
        } else {
            pattern_declared = true;
        }
        index += 1;
    }
    if !files_mode && !pattern_declared {
        return false;
    }
    paths.iter().all(|path| read_path_is_within(path, cwd, checkout_root, false))
}

fn is_safe_combined_rg_flag(value: &str) -> bool {
    value.strip_prefix('-').is_some_and(|flags| {
        flags.len() > 1 && flags.bytes().all(|flag| b"nsiFSwx".contains(&flag))
    })
}

fn get_content_argv_is_allowed(argv: &[String], cwd: &Path, checkout_root: &Path) -> bool {
    let mut index = 1;
    let mut path = None;
    while index < argv.len() {
        let argument = &argv[index];
        let normalized = argument.to_ascii_lowercase();
        match normalized.as_str() {
            "-literalpath" => {
                index += 1;
                if path.is_some() || index >= argv.len() {
                    return false;
                }
                path = Some(argv[index].as_str());
            }
            "-totalcount" | "-tail" | "-readcount" => {
                index += 1;
                if !argv.get(index).is_some_and(|value| bounded_count(value)) {
                    return false;
                }
            }
            "-encoding" => {
                index += 1;
                if !argv.get(index).is_some_and(|value| {
                    matches!(value.to_ascii_lowercase().as_str(), "utf8" | "utf8bom" | "ascii")
                }) {
                    return false;
                }
            }
            "-raw" => {}
            _ if argument.starts_with('-') || path.is_some() => return false,
            _ => path = Some(argument),
        }
        index += 1;
    }
    path.is_some_and(|path| read_path_is_within(path, cwd, checkout_root, true))
}

fn bounded_count(value: &str) -> bool {
    value.parse::<u32>().is_ok_and(|value| value <= 4096)
}

fn read_path_is_within(path: &str, cwd: &Path, checkout_root: &Path, bounded_file: bool) -> bool {
    if path.is_empty()
        || path.contains(['*', '?', '[', ']'])
        || Path::new(path).components().any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    let candidate =
        if Path::new(path).is_absolute() { PathBuf::from(path) } else { cwd.join(path) };
    let (Ok(candidate), Ok(root)) = (fs::canonicalize(candidate), fs::canonicalize(checkout_root))
    else {
        return false;
    };
    if !candidate.starts_with(root) {
        return false;
    }
    !bounded_file
        || fs::metadata(candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= MAX_GET_CONTENT_BYTES)
}

fn read_permissions_are_within(permissions: &RequestedPermissions, checkout_root: &Path) -> bool {
    permissions
        .read_paths
        .iter()
        .all(|path| read_path_is_within(path, checkout_root, checkout_root, false))
}

fn shell_safe_command(argv: &[String]) -> Option<String> {
    if argv.is_empty()
        || argv.iter().any(|argument| {
            argument.is_empty()
                || !argument.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "_./:-".contains(character)
                })
        })
    {
        return None;
    }
    let command = argv.join(" ");
    (parse_direct_argv(&command).ok().as_deref() == Some(argv)).then_some(command)
}

fn split_shell_executable(command: &str) -> Option<(&str, &str)> {
    let command = command.trim();
    if let Some(command) = command.strip_prefix('"') {
        let closing = command.find('"')?;
        let executable = &command[..closing];
        let arguments = command[closing + 1..].trim_start();
        (!executable.is_empty() && !arguments.is_empty()).then_some((executable, arguments))
    } else {
        let split = command.find(char::is_whitespace)?;
        let executable = &command[..split];
        let arguments = command[split..].trim_start();
        (!executable.is_empty() && !arguments.is_empty()).then_some((executable, arguments))
    }
}

fn split_shell_payload<'a>(
    arguments: &'a str,
    expected_command: &str,
) -> Option<(&'a str, Option<char>, &'a str)> {
    let Some(quote_start) = arguments.find(['\'', '"']) else {
        let prefix = arguments.strip_suffix(expected_command)?;
        if !prefix.chars().last().is_some_and(char::is_whitespace) {
            return None;
        }
        let flags = prefix.trim_end();
        if flags.is_empty() {
            return None;
        }
        return Some((flags, None, &arguments[arguments.len() - expected_command.len()..]));
    };
    let flags = arguments[..quote_start].trim();
    let quoted = arguments[quote_start..].trim();
    let quote = quoted.chars().next()?;
    if quoted.len() < 2 || !quoted.ends_with(quote) {
        return None;
    }
    let payload = &quoted[1..quoted.len() - 1];
    if flags.is_empty() || payload.contains(quote) {
        return None;
    }
    Some((flags, Some(quote), payload))
}

fn valid_powershell_flags(flags: &[String]) -> bool {
    let Some((command, setup)) = flags.split_last() else {
        return false;
    };
    command == "-command"
        && setup.len() <= 3
        && setup
            .iter()
            .all(|flag| matches!(flag.as_str(), "-nologo" | "-noprofile" | "-noninteractive"))
        && setup.iter().enumerate().all(|(index, flag)| !setup[..index].contains(flag))
}

fn flags_equal(flags: &[String], expected: &[&str]) -> bool {
    flags.len() == expected.len()
        && flags.iter().zip(expected).all(|(actual, expected)| actual == expected)
}

pub fn command_evidence_from_output(
    approval: &ApprovalRequest,
    snapshot_digest: Digest,
    runner_version: Option<String>,
    expected_test_identifier: Option<&str>,
    exit_status: Option<i32>,
    duration_ms: u64,
    output: &[u8],
) -> CommandExecutionEvidence {
    let preview = bounded_preview(output, 4096);
    let tests_executed = parse_cargo_tests_executed(&preview);
    let test_identifier = expected_test_identifier
        .filter(|identifier| test_output_observes_identifier(&preview, identifier))
        .map(str::to_owned);
    CommandExecutionEvidence {
        id: format!(
            "command-evidence-{}",
            Digest::blake3(format!("{}\n{}\n", approval.id, approval.item_id)).to_hex()
        ),
        approval_id: approval.id.clone(),
        argv: approval.argv.clone(),
        cwd: approval.cwd.clone(),
        source_snapshot_digest: snapshot_digest,
        runner: approval.argv.first().cloned().unwrap_or_default(),
        runner_version,
        exit_status,
        duration_ms,
        output_digest: Digest::blake3(output),
        output_preview: preview,
        test_identifier,
        tests_executed,
        infrastructure_failure: None,
    }
}

fn test_output_observes_identifier(output: &str, identifier: &str) -> bool {
    let suffix = identifier.rsplit("::").next().unwrap_or(identifier);
    !suffix.is_empty()
        && output.lines().any(|line| {
            let Some(observation) = line.trim().strip_prefix("test ") else {
                return false;
            };
            let observed_identifier =
                observation.split_once(" ... ").map_or(observation, |(name, _)| name).trim();
            observed_identifier != "result:"
                && (observed_identifier == identifier || observed_identifier == suffix)
        })
}

fn parse_cargo_tests_executed(output: &str) -> Option<u32> {
    output.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix("test result: ok. ")?;
        let count = rest.split_whitespace().next()?;
        count.parse().ok()
    })
}

fn bounded_preview(output: &[u8], maximum: usize) -> String {
    let text = String::from_utf8_lossy(output);
    let mut end = text.len().min(maximum);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = fs::canonicalize(path).ok();
    let root = fs::canonicalize(root).ok();
    matches!((path, root), (Some(path), Some(root)) if path.starts_with(&root))
}

fn write_path_allowed(value: &str, target_root: &Path, temp_root: &Path) -> bool {
    let path = Path::new(value);
    if path.components().any(|component| {
        matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
    }) && !path.is_absolute()
    {
        return false;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        return false;
    };
    absolute.starts_with(target_root) || absolute.starts_with(temp_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(item_id: &str) -> ApprovalRequest {
        let argv = vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--test".to_owned(),
            "integration".to_owned(),
            "misc::glob_always_case_insensitive".to_owned(),
            "--".to_owned(),
            "--exact".to_owned(),
        ];
        let permissions = RequestedPermissions::default();
        ApprovalRequest {
            id: "shared-protocol-approval".to_owned(),
            protocol_request_id: serde_json::json!(1),
            protocol_approval_id: Some("shared-protocol-approval".to_owned()),
            thread_id: "thread".to_owned(),
            turn_id: "turn".to_owned(),
            item_id: item_id.to_owned(),
            argv: argv.clone(),
            command_display: Some(argv.join(" ")),
            cwd: ".".to_owned(),
            reason: None,
            requested_permissions: permissions.clone(),
            route: "tests.relevant".to_owned(),
            repository_id: Digest::blake3("repo"),
            repository_root: ".".to_owned(),
            expires_unix_ms: u64::MAX,
            classification: CommandClassification::AutoApprovedTest {
                policy_id: "cargo-test-direct-v1".to_owned(),
            },
            payload_digest: ApprovalRequest::compute_payload_digest(&argv, ".", &permissions)
                .unwrap(),
            decision: Some(ApprovalDecision::Accept),
            decision_source: Some(ApprovalDecisionSource::AutoPolicy),
            decided_unix_ms: Some(1),
        }
    }

    fn context(root: &Path) -> ApprovalContext {
        ApprovalContext {
            route: "tests.relevant".to_owned(),
            repository_id: Digest::blake3("repo"),
            checkout_root: root.join("checkout"),
            target_root: root.join("target"),
            temp_root: root.join("tmp"),
            test_plan: Some(TestPlan {
                runner: "cargo".to_owned(),
                argv: vec![
                    "cargo".to_owned(),
                    "test".to_owned(),
                    "--test".to_owned(),
                    "integration".to_owned(),
                    "misc::glob_always_case_insensitive".to_owned(),
                    "--".to_owned(),
                    "--exact".to_owned(),
                ],
                cwd_relative: ".".to_owned(),
                test_identifier: "misc::glob_always_case_insensitive".to_owned(),
                requires_approval: true,
                execution_evidence_id: None,
            }),
            test_execution_available: true,
        }
    }

    fn read_only_broker(repository_id: Digest) -> ApprovalBroker {
        ApprovalBroker::new(vec![TestCommandPolicy::cargo_test(repository_id)])
            .with_read_only_policies(vec![ReadOnlyCommandPolicy::repository_inspection(
                repository_id,
            )])
    }

    fn prepare_read_fixture(root: &Path) {
        for child in ["checkout/src", "target", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        fs::write(root.join("checkout/src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    }

    #[test]
    fn only_exact_direct_cargo_test_is_auto_approved_twice() {
        let root = std::env::temp_dir().join(format!("needle-approval-{}", std::process::id()));
        for child in ["checkout", "target", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        let context = context(&root);
        let policy = TestCommandPolicy::cargo_test(context.repository_id);
        let mut broker = ApprovalBroker::new(vec![policy]);
        let command = "cargo test --test integration misc::glob_always_case_insensitive -- --exact";
        for _ in 0..2 {
            let (_, classification) = broker.classify_command(
                Some(command),
                Some(command),
                &context.checkout_root,
                &RequestedPermissions::default(),
                &context,
            );
            assert!(matches!(classification, CommandClassification::AutoApprovedTest { .. }));
        }
        let (_, classification) = broker.classify_command(
            Some(command),
            Some(command),
            &context.checkout_root,
            &RequestedPermissions::default(),
            &context,
        );
        assert_eq!(classification, CommandClassification::RejectedPolicyMismatch);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exact_declared_test_is_declined_when_execution_is_unavailable() {
        let root = std::env::temp_dir()
            .join(format!("needle-approval-unavailable-{}", std::process::id()));
        for child in ["checkout", "target", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        let mut context = context(&root);
        context.test_execution_available = false;
        let command = context.test_plan.as_ref().unwrap().argv.join(" ");
        let (argv, classification) =
            ApprovalBroker::new(vec![TestCommandPolicy::cargo_test(context.repository_id)])
                .classify_command(
                    Some(&command),
                    Some(&command),
                    &context.checkout_root,
                    &RequestedPermissions::default(),
                    &context,
                );
        assert_eq!(argv, context.test_plan.as_ref().unwrap().argv);
        assert_eq!(classification, CommandClassification::RejectedPolicyMismatch);
        assert_eq!(
            ApprovalBroker::automatic_decision(&classification),
            Some((ApprovalDecision::Decline, ApprovalDecisionSource::Runtime))
        );
        let (_, classification) = ApprovalBroker::new(Vec::new()).classify_command(
            Some(&command),
            None,
            &context.checkout_root,
            &RequestedPermissions::default(),
            &context,
        );
        assert_eq!(classification, CommandClassification::RejectedPolicyMismatch);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn non_canonical_declared_test_plan_is_rejected_before_command_matching() {
        let root = std::env::temp_dir()
            .join(format!("needle-approval-non-canonical-{}", std::process::id()));
        for child in ["checkout", "target", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        let mut context = context(&root);
        context.test_plan.as_mut().unwrap().argv.remove(0);
        let mut broker =
            ApprovalBroker::new(vec![TestCommandPolicy::cargo_test(context.repository_id)]);
        let (_, classification) = broker.classify_command(
            Some("cargo test --test integration misc::glob_always_case_insensitive -- --exact"),
            Some("cargo test --test integration misc::glob_always_case_insensitive -- --exact"),
            &context.checkout_root,
            &RequestedPermissions::default(),
            &context,
        );
        assert_eq!(classification, CommandClassification::RejectedPolicyMismatch);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn command_evidence_identity_is_scoped_to_the_execution_item() {
        let first = command_evidence_from_output(
            &approval("item-1"),
            Digest::blake3("snapshot"),
            None,
            None,
            Some(0),
            1,
            b"test result: ok. 1 passed",
        );
        let replay = command_evidence_from_output(
            &approval("item-1"),
            Digest::blake3("snapshot"),
            None,
            None,
            Some(0),
            1,
            b"test result: ok. 1 passed",
        );
        let second = command_evidence_from_output(
            &approval("item-2"),
            Digest::blake3("snapshot"),
            None,
            None,
            Some(0),
            1,
            b"test result: ok. 1 passed",
        );
        assert_eq!(first.id, replay.id);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn command_evidence_binds_a_qualified_identifier_observed_as_a_suffix() {
        let plan = context(&std::env::temp_dir()).test_plan.unwrap();
        let evidence = command_evidence_from_output(
            &approval("suffix-item"),
            Digest::blake3("snapshot"),
            Some("cargo 1".to_owned()),
            Some(&plan.test_identifier),
            Some(0),
            1,
            b"test glob_always_case_insensitive ... ok\ntest result: ok. 1 passed; 0 failed",
        );

        assert_eq!(evidence.test_identifier.as_deref(), Some(plan.test_identifier.as_str()));
        assert_eq!(validate_test_evidence(&plan, &evidence), Ok(()));
        assert!(!test_output_observes_identifier("test result: ok. 1 passed; 0 failed", "test"));
        assert!(!test_output_observes_identifier(
            "test another::glob_always_case_insensitive_extra ... ok",
            &plan.test_identifier
        ));
    }

    #[test]
    fn canonical_test_shell_wrappers_are_analyzed_as_the_declared_argv() {
        let expected = context(&std::env::temp_dir()).test_plan.unwrap().argv;
        let direct = expected.join(" ");
        let commands = [
            format!(
                "\"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" -Command '{direct}'"
            ),
            format!("pwsh -NoLogo -NoProfile -NonInteractive -Command \"{direct}\""),
            format!("powershell.exe -Command {direct}"),
            format!("cmd.exe /d /s /c \"{direct}\""),
            format!("cmd.exe /c {direct}"),
            format!("/bin/sh -c '{direct}'"),
            format!("/bin/bash -lc '{direct}'"),
            format!("/bin/zsh -l -c \"{direct}\""),
        ];
        for command in commands {
            assert_eq!(parse_test_command_argv(&command, &expected), Ok(expected.clone()));
        }
    }

    #[test]
    fn canonical_test_shell_wrappers_can_be_auto_approved() {
        let root =
            std::env::temp_dir().join(format!("needle-approval-wrapper-{}", std::process::id()));
        for child in ["checkout", "target", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        let context = context(&root);
        let direct = context.test_plan.as_ref().unwrap().argv.join(" ");
        for command in [
            format!("powershell.exe -Command '{direct}'"),
            format!("cmd.exe /d /s /c \"{direct}\""),
            format!("/bin/bash -lc '{direct}'"),
        ] {
            let mut broker =
                ApprovalBroker::new(vec![TestCommandPolicy::cargo_test(context.repository_id)]);
            let (argv, classification) = broker.classify_command(
                Some(&command),
                Some(&direct),
                &context.checkout_root,
                &RequestedPermissions::default(),
                &context,
            );
            assert_eq!(argv, context.test_plan.as_ref().unwrap().argv);
            assert!(matches!(classification, CommandClassification::AutoApprovedTest { .. }));
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn direct_and_wrapped_bounded_repository_reads_are_auto_approved() {
        let root =
            std::env::temp_dir().join(format!("needle-read-approval-{}", std::process::id()));
        prepare_read_fixture(&root);
        let context = context(&root);
        let mut broker = read_only_broker(context.repository_id);
        let cases = [
            (
                "rg -n 'answer|missing' src",
                "powershell.exe -NoProfile -Command \"rg -n 'answer|missing' src\"",
                vec!["rg", "-n", "answer|missing", "src"],
            ),
            (
                "Get-Content -LiteralPath src/lib.rs -TotalCount 64",
                "pwsh -NoLogo -NonInteractive -Command 'Get-Content -LiteralPath src/lib.rs -TotalCount 64'",
                vec!["Get-Content", "-LiteralPath", "src/lib.rs", "-TotalCount", "64"],
            ),
            ("rg --files src", "/bin/sh -c 'rg --files src'", vec!["rg", "--files", "src"]),
        ];
        for (action, display, expected) in cases {
            let (argv, classification) = broker.classify_command(
                Some(display),
                Some(action),
                &context.checkout_root,
                &RequestedPermissions::default(),
                &context,
            );
            assert_eq!(argv, expected);
            assert!(matches!(classification, CommandClassification::AutoApprovedReadOnly { .. }));
            assert_eq!(
                ApprovalBroker::automatic_decision(&classification),
                Some((ApprovalDecision::Accept, ApprovalDecisionSource::AutoPolicy))
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_only_policy_rejects_execution_escape_and_mutation_features() {
        let root = std::env::temp_dir().join(format!("needle-read-reject-{}", std::process::id()));
        prepare_read_fixture(&root);
        let context = context(&root);
        for command in [
            "rg -n answer src | tee output",
            "rg -n answer src; whoami",
            "rg --pre malicious answer src",
            "rg -f patterns.txt src",
            "Get-Content ../secret",
            "Get-Content -Path src/*.rs",
            "Get-Content src/lib.rs > copy",
            "powershell.exe -Command '$p=\"src/lib.rs\"; Get-Content $p'",
            "cmd.exe /c \"rg -n answer src && whoami\"",
            "/bin/sh -c 'rg -n answer src; id'",
        ] {
            let (argv, classification) = read_only_broker(context.repository_id).classify_command(
                Some(command),
                Some(command),
                &context.checkout_root,
                &RequestedPermissions::default(),
                &context,
            );
            assert!(
                !matches!(classification, CommandClassification::AutoApprovedReadOnly { .. }),
                "{command} unexpectedly approved as {argv:?}"
            );
        }
        let permissions = RequestedPermissions {
            read_paths: vec![root.join("outside").to_string_lossy().into_owned()],
            ..RequestedPermissions::default()
        };
        let (_, classification) = read_only_broker(context.repository_id).classify_command(
            Some("rg -n answer src"),
            Some("rg -n answer src"),
            &context.checkout_root,
            &permissions,
            &context,
        );
        assert_eq!(classification, CommandClassification::PendingUser);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_only_display_and_action_must_match_and_budget_is_bounded() {
        let root = std::env::temp_dir().join(format!("needle-read-budget-{}", std::process::id()));
        prepare_read_fixture(&root);
        let context = context(&root);
        let mut broker = read_only_broker(context.repository_id);
        let (_, classification) = broker.classify_command(
            Some("powershell.exe -Command 'rg -n answer src'"),
            Some("rg -n different src"),
            &context.checkout_root,
            &RequestedPermissions::default(),
            &context,
        );
        assert_eq!(classification, CommandClassification::PendingUser);
        for _ in 0..16 {
            let (_, classification) = broker.classify_command(
                Some("rg -n answer src"),
                Some("rg -n answer src"),
                &context.checkout_root,
                &RequestedPermissions::default(),
                &context,
            );
            assert!(matches!(classification, CommandClassification::AutoApprovedReadOnly { .. }));
        }
        let (_, classification) = broker.classify_command(
            Some("rg -n answer src"),
            Some("rg -n answer src"),
            &context.checkout_root,
            &RequestedPermissions::default(),
            &context,
        );
        assert_eq!(classification, CommandClassification::RejectedPolicyMismatch);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn wrappers_chaining_environment_globs_and_wrong_test_are_never_auto_approved() {
        let expected = context(&std::env::temp_dir()).test_plan.unwrap().argv;
        let direct = expected.join(" ");
        for command in [
            format!("cmd.exe /c \"{direct} && whoami\""),
            format!("powershell.exe -Command '{direct}; whoami'"),
            format!("powershell.exe -ExecutionPolicy Bypass -Command '{direct}'"),
            format!("/bin/bash -lc '{direct} | tee output'"),
            format!("/bin/sh -c 'CARGO_TARGET_DIR=x {direct}'"),
            format!("/bin/sh -c '{direct}' && whoami"),
            format!("/bin/sh -c {direct}"),
            "cargo test wrong_test".to_owned(),
        ] {
            assert_ne!(parse_test_command_argv(&command, &expected), Ok(expected.clone()));
        }
        let glob = ["cargo", "test", "*"].map(str::to_owned);
        assert!(parse_test_command_argv("/bin/sh -c 'cargo test *'", &glob).is_err());
        let escaped = ["cargo", "test", r"module\test"].map(str::to_owned);
        assert!(parse_test_command_argv("/bin/sh -c 'cargo test module\\test'", &escaped).is_err());
    }

    #[test]
    fn declared_test_cwd_must_match_exactly() {
        let root = std::env::temp_dir().join(format!("needle-approval-cwd-{}", std::process::id()));
        for child in ["checkout", "checkout/nested", "target", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        let context = context(&root);
        let mut broker =
            ApprovalBroker::new(vec![TestCommandPolicy::cargo_test(context.repository_id)]);
        let (_, classification) = broker.classify_command(
            Some("cargo test --test integration misc::glob_always_case_insensitive -- --exact"),
            Some("cargo test --test integration misc::glob_always_case_insensitive -- --exact"),
            &context.checkout_root.join("nested"),
            &RequestedPermissions::default(),
            &context,
        );
        assert_eq!(classification, CommandClassification::PendingUser);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_app_server_command_action_never_auto_approves() {
        let root =
            std::env::temp_dir().join(format!("needle-approval-action-{}", std::process::id()));
        for child in ["checkout", "target", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        let context = context(&root);
        let mut broker =
            ApprovalBroker::new(vec![TestCommandPolicy::cargo_test(context.repository_id)]);
        let (argv, classification) = broker.classify_command(
            Some("cargo test --test integration misc::glob_always_case_insensitive -- --exact"),
            None,
            &context.checkout_root,
            &RequestedPermissions::default(),
            &context,
        );
        assert!(argv.is_empty());
        assert_eq!(classification, CommandClassification::PendingUser);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn displayed_wrapper_and_structured_action_must_resolve_to_the_same_test() {
        let root =
            std::env::temp_dir().join(format!("needle-approval-mismatch-{}", std::process::id()));
        for child in ["checkout", "target", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        let context = context(&root);
        let direct = context.test_plan.as_ref().unwrap().argv.join(" ");
        let mut broker =
            ApprovalBroker::new(vec![TestCommandPolicy::cargo_test(context.repository_id)]);
        let (argv, classification) = broker.classify_command(
            Some(&format!("powershell.exe -Command '{direct}'")),
            Some("cargo test wrong_test"),
            &context.checkout_root,
            &RequestedPermissions::default(),
            &context,
        );
        assert!(argv.is_empty());
        assert_eq!(classification, CommandClassification::PendingUser);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn network_request_is_rejected_even_without_analyzable_command_action() {
        let root =
            std::env::temp_dir().join(format!("needle-approval-network-{}", std::process::id()));
        for child in ["checkout", "target", "tmp"] {
            fs::create_dir_all(root.join(child)).unwrap();
        }
        let context = context(&root);
        let mut broker =
            ApprovalBroker::new(vec![TestCommandPolicy::cargo_test(context.repository_id)]);
        let permissions = RequestedPermissions { network: true, ..RequestedPermissions::default() };
        let (argv, classification) =
            broker.classify_command(None, None, &context.checkout_root, &permissions, &context);
        assert!(argv.is_empty());
        assert_eq!(classification, CommandClassification::RejectedNetwork);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn semantic_validator_requires_identifier_count_and_success() {
        let root = std::env::temp_dir();
        let context = context(&root);
        let plan = context.test_plan.unwrap();
        let evidence = CommandExecutionEvidence {
            id: "evidence".to_owned(),
            approval_id: "approval".to_owned(),
            argv: plan.argv.clone(),
            cwd: ".".to_owned(),
            source_snapshot_digest: Digest::blake3("snapshot"),
            runner: "cargo".to_owned(),
            runner_version: Some("cargo 1".to_owned()),
            exit_status: Some(0),
            duration_ms: 1,
            output_digest: Digest::blake3("output"),
            output_preview: "test misc::glob_always_case_insensitive ... ok\ntest result: ok. 1 passed; 0 failed".to_owned(),
            test_identifier: Some("misc::glob_always_case_insensitive".to_owned()),
            tests_executed: Some(1),
            infrastructure_failure: None,
        };
        assert_eq!(validate_test_evidence(&plan, &evidence), Ok(()));
    }
}
