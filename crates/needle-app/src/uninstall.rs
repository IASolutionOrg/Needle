use super::{
    AppError, managed_codex_candidate, managed_codex_package_is_complete, product_data_directory,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const UNINSTALL_SCRIPT: &str = include_str!("../../../packaging/windows/uninstall.ps1");

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Eq, PartialEq)]
struct ManagedInstallation {
    root: PathBuf,
    executable: PathBuf,
    runtime: PathBuf,
    uninstaller: PathBuf,
}

pub(crate) fn run(arguments: Vec<String>) -> Result<(), AppError> {
    if matches!(arguments.as_slice(), [argument] if argument == "--help" || argument == "-h") {
        println!("Usage: needle uninstall");
        println!(
            "\nRemoves the managed Windows installation, user PATH entry, Codex CLI hooks, and managed Codex Desktop skill. Product data is preserved."
        );
        return Ok(());
    }
    if let Some(argument) = arguments.first() {
        return Err(AppError::Usage(format!("unknown uninstall argument `{argument}`")));
    }

    #[cfg(not(windows))]
    {
        return Err(AppError::Runtime(
            "uninstall is currently supported only for managed Windows installations".to_owned(),
        ));
    }

    #[cfg(windows)]
    run_windows()
}

#[cfg(windows)]
fn run_windows() -> Result<(), AppError> {
    let installation = managed_installation(&env::current_exe().map_err(AppError::Io)?)?;
    let data_directory = product_data_directory(&[])?;
    validate_cleanup(&installation)?;

    let hook_removal = crate::codex_hooks::remove_registered().map_err(|error| {
        AppError::Runtime(format!("cannot remove managed Codex CLI hooks: {error}"))
    })?;
    let skill_removal = crate::codex_skill::remove_managed().map_err(|error| {
        AppError::Runtime(format!("cannot remove managed Codex Desktop skill: {error}"))
    })?;
    let path_removed = remove_installation_from_user_path(&installation.root)?;

    schedule_cleanup(&installation)?;

    println!("Needle uninstall scheduled.");
    println!(
        "Codex CLI hooks:      {}",
        if hook_removal.removed { "Removed" } else { "Not installed" }
    );
    println!(
        "Codex Desktop skill:  {}",
        if skill_removal.removed {
            "Removed"
        } else if skill_removal.unmanaged_preserved {
            "Unmanaged skill preserved"
        } else {
            "Not installed"
        }
    );
    println!(
        "User PATH:            {}",
        if path_removed { "Updated" } else { "Entry not present" }
    );
    println!("Installation:         {}", installation.root.display());
    println!("Preserved product data: {}", data_directory.display());
    println!(
        "\nThe managed executable, runtime, uninstaller, and matching user PATH entry will be removed after this process exits. Unrelated files are preserved."
    );
    Ok(())
}

fn managed_installation(executable: &Path) -> Result<ManagedInstallation, AppError> {
    let executable = fs::canonicalize(executable).map_err(|error| {
        AppError::Runtime(format!("cannot resolve the Needle executable: {error}"))
    })?;
    let expected_name = if cfg!(windows) { "needle.exe" } else { "needle" };
    if !executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(expected_name))
    {
        return Err(AppError::Runtime(format!(
            "{} is not a managed Needle executable",
            executable.display()
        )));
    }
    let root = executable
        .parent()
        .ok_or_else(|| {
            AppError::Runtime("the Needle executable has no parent directory".to_owned())
        })?
        .to_path_buf();
    let codex = managed_codex_candidate(&executable).ok_or_else(|| {
        AppError::Runtime("the managed Codex runtime path cannot be resolved".to_owned())
    })?;
    if !managed_codex_package_is_complete(&codex) {
        return Err(AppError::Runtime(
            "the current executable is not inside a complete managed Needle installation; no files were removed"
                .to_owned(),
        ));
    }
    let runtime = fs::canonicalize(root.join("runtime")).map_err(|error| {
        AppError::Runtime(format!("cannot resolve the managed runtime directory: {error}"))
    })?;
    if runtime.parent() != Some(root.as_path()) {
        return Err(AppError::Runtime(
            "the managed runtime escapes the Needle installation directory; no files were removed"
                .to_owned(),
        ));
    }
    let uninstaller = root.join("uninstall.ps1");
    let installed_script = fs::read_to_string(&uninstaller).map_err(|error| {
        AppError::Runtime(format!(
            "the managed uninstaller is unavailable at {}: {error}",
            uninstaller.display()
        ))
    })?;
    if installed_script != UNINSTALL_SCRIPT {
        return Err(AppError::Runtime(format!(
            "{} is not the unmodified managed Needle uninstaller; no files were removed",
            uninstaller.display()
        )));
    }
    Ok(ManagedInstallation { root, executable, runtime, uninstaller })
}

#[cfg(windows)]
fn uninstaller_command(installation: &ManagedInstallation) -> Result<Command, AppError> {
    let parent_process_id = i32::try_from(std::process::id()).map_err(|_| {
        AppError::Runtime("the current Windows process id cannot be monitored safely".to_owned())
    })?;
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(powershell_path(&installation.uninstaller))
        .arg("-ParentProcessId")
        .arg(parent_process_id.to_string())
        .arg("-InstallDirectory")
        .arg(powershell_path(&installation.root))
        .arg("-Executable")
        .arg(powershell_path(&installation.executable))
        .arg("-RuntimeDirectory")
        .arg(powershell_path(&installation.runtime))
        .creation_flags(CREATE_NO_WINDOW);
    Ok(command)
}

#[cfg(windows)]
fn validate_cleanup(installation: &ManagedInstallation) -> Result<(), AppError> {
    let output =
        uninstaller_command(installation)?.arg("-ValidateOnly").output().map_err(|error| {
            AppError::Runtime(format!("cannot validate the managed Windows uninstaller: {error}"))
        })?;
    if output.status.success() {
        return Ok(());
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    Err(AppError::Runtime(format!(
        "managed Windows uninstaller validation failed: {}",
        diagnostic.trim()
    )))
}

#[cfg(windows)]
fn schedule_cleanup(installation: &ManagedInstallation) -> Result<(), AppError> {
    uninstaller_command(installation)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            AppError::Runtime(format!("cannot start the managed Windows uninstaller: {error}"))
        })?;
    Ok(())
}

#[cfg(windows)]
fn powershell_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
    }
}

#[cfg(windows)]
fn remove_installation_from_user_path(installation_root: &Path) -> Result<bool, AppError> {
    const READ_USER_PATH: &str = "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); [Console]::Out.Write([Environment]::GetEnvironmentVariable('Path','User'))";
    const WRITE_USER_PATH: &str =
        "[Environment]::SetEnvironmentVariable('Path',$env:NEEDLE_UNINSTALL_USER_PATH,'User')";

    let read_path = || -> Result<String, AppError> {
        let output = Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", READ_USER_PATH])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| AppError::Runtime(format!("cannot read the user PATH: {error}")))?;
        if !output.status.success() {
            return Err(AppError::Runtime(format!(
                "cannot read the user PATH: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        String::from_utf8(output.stdout).map_err(|error| {
            AppError::Runtime(format!("the user PATH is not valid UTF-8: {error}"))
        })
    };

    let current = read_path()?;
    let updated = path_without_installation(&current, &powershell_path(installation_root));
    if updated == current {
        return Ok(false);
    }
    let output = Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", WRITE_USER_PATH])
        .env("NEEDLE_UNINSTALL_USER_PATH", &updated)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| AppError::Runtime(format!("cannot update the user PATH: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Runtime(format!(
            "cannot update the user PATH: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if read_path()? != updated {
        return Err(AppError::Runtime(
            "the user PATH did not retain the Needle removal".to_owned(),
        ));
    }
    Ok(true)
}

fn path_without_installation(user_path: &str, installation_root: &str) -> String {
    let target = normalized_path_entry(installation_root);
    user_path
        .split(';')
        .filter(|entry| !normalized_path_entry(entry).eq_ignore_ascii_case(&target))
        .collect::<Vec<_>>()
        .join(";")
}

fn normalized_path_entry(value: &str) -> String {
    value.trim().trim_matches('"').trim_end_matches(['\\', '/']).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        env::temp_dir().join(format!("needle-uninstall-{name}-{nonce}"))
    }

    fn managed_layout(name: &str) -> ManagedInstallation {
        let root = temporary_root(name);
        let executable = root.join(if cfg!(windows) { "needle.exe" } else { "needle" });
        let runtime = root.join("runtime");
        let required = [
            executable.clone(),
            runtime.join(if cfg!(windows) { "bin/codex.exe" } else { "bin/codex" }),
            runtime.join("codex-package.json"),
            runtime.join("bin/codex-code-mode-host.exe"),
            runtime.join("codex-path/rg.exe"),
            runtime.join("codex-resources/codex-command-runner.exe"),
            runtime.join("codex-resources/codex-windows-sandbox-setup.exe"),
        ];
        for path in required {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, []).unwrap();
        }
        fs::write(root.join("uninstall.ps1"), UNINSTALL_SCRIPT).unwrap();
        managed_installation(&executable).unwrap()
    }

    #[test]
    fn managed_layout_requires_the_bundled_uninstaller() {
        let installation = managed_layout("managed");
        fs::write(&installation.uninstaller, "Write-Host 'not managed'").unwrap();

        let error = managed_installation(&installation.executable).unwrap_err().to_string();

        assert!(error.contains("not the unmodified managed Needle uninstaller"));
        fs::remove_dir_all(installation.root).unwrap();
    }

    #[test]
    fn development_layout_is_rejected_without_removal() {
        let root = temporary_root("development");
        fs::create_dir_all(&root).unwrap();
        let executable = root.join(if cfg!(windows) { "needle.exe" } else { "needle" });
        fs::write(&executable, []).unwrap();

        let error = managed_installation(&executable).unwrap_err().to_string();

        assert!(error.contains("not inside a complete managed Needle installation"));
        assert!(executable.is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_removal_is_exact_case_insensitive_and_preserves_other_entries() {
        let current = r#"C:\Tools;; C:\Users\user\Programs\Needle\ ;"C:\Keep Me";c:\users\user\programs\needle;C:\Needle-Other;"#;

        let updated = path_without_installation(current, r"C:\Users\user\Programs\Needle");

        assert_eq!(updated, r#"C:\Tools;;"C:\Keep Me";C:\Needle-Other;"#);
    }

    #[cfg(windows)]
    #[test]
    fn windows_uninstaller_removes_only_managed_files() {
        let installation = managed_layout("script");
        let unrelated = installation.root.join("keep-me.txt");
        fs::write(&unrelated, "user content").unwrap();

        let status = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(powershell_path(&installation.uninstaller))
            .arg("-ParentProcessId")
            .arg(i32::MAX.to_string())
            .arg("-InstallDirectory")
            .arg(powershell_path(&installation.root))
            .arg("-Executable")
            .arg(powershell_path(&installation.executable))
            .arg("-RuntimeDirectory")
            .arg(powershell_path(&installation.runtime))
            .status()
            .unwrap();

        assert!(status.success());
        assert!(!installation.executable.exists());
        assert!(!installation.runtime.exists());
        assert!(!installation.uninstaller.exists());
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "user content");
        fs::remove_dir_all(installation.root).unwrap();
    }
}
