use super::{AppError, product_data_directory};
use needle_platform_codex::{debug_logging_status, disable_debug_logging, enable_debug_logging};
use std::fs;

const MAX_DEBUG_LOG_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) fn run(arguments: Vec<String>) -> Result<(), AppError> {
    if matches!(arguments.as_slice(), [argument] if argument == "--help" || argument == "-h") {
        print_help();
        return Ok(());
    }
    validate_arguments(&arguments)?;
    let action = arguments
        .first()
        .map(String::as_str)
        .ok_or_else(|| AppError::Usage("debug enable|disable|status|latest".to_owned()))?;
    let data_directory = product_data_directory(&arguments)?;
    match action {
        "enable" => {
            let status = enable_debug_logging(&data_directory).map_err(AppError::Runtime)?;
            println!(
                "Needle debug logging enabled.\n\nDirectory: {}\nRetention: Latest 20 worker logs\n\nDebug logs may contain local repository paths and bounded worker evidence.",
                status.directory.display()
            );
        }
        "disable" => {
            let status = disable_debug_logging(&data_directory).map_err(AppError::Runtime)?;
            println!(
                "Needle debug logging disabled.\n\nExisting logs were preserved in {}.",
                status.directory.display()
            );
        }
        "status" => {
            let status = debug_logging_status(&data_directory).map_err(AppError::Runtime)?;
            println!(
                "Needle debug logging\n\nState:      {}\nDirectory:  {}\nLatest log: {}",
                if status.enabled { "Enabled" } else { "Disabled" },
                status.directory.display(),
                status
                    .latest
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "None".to_owned())
            );
        }
        "latest" => {
            let status = debug_logging_status(&data_directory).map_err(AppError::Runtime)?;
            let path = status.latest.ok_or_else(|| {
                AppError::Runtime("no Needle worker debug log is available".to_owned())
            })?;
            let metadata = fs::metadata(&path)?;
            if metadata.len() > MAX_DEBUG_LOG_BYTES {
                return Err(AppError::Runtime(format!(
                    "latest debug log exceeds the 2 MiB display bound: {}",
                    path.display()
                )));
            }
            println!("Needle debug log: {}\n", path.display());
            print!("{}", fs::read_to_string(path)?);
        }
        _ => return Err(AppError::Usage("debug enable|disable|status|latest".to_owned())),
    }
    Ok(())
}

fn validate_arguments(arguments: &[String]) -> Result<(), AppError> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(AppError::Usage("debug enable|disable|status|latest".to_owned()));
    };
    if !matches!(action, "enable" | "disable" | "status" | "latest") {
        return Err(AppError::Usage("debug enable|disable|status|latest".to_owned()));
    }
    let mut index = 1;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--data-dir" => {
                if arguments.get(index + 1).is_none() {
                    return Err(AppError::Usage("--data-dir requires a value".to_owned()));
                }
                index += 2;
            }
            argument => {
                return Err(AppError::Usage(format!("unknown debug argument `{argument}`")));
            }
        }
    }
    Ok(())
}

fn print_help() {
    println!(
        "Usage: needle debug <enable|disable|status|latest> [--data-dir <path>]\n\n  enable   Persist bounded worker protocol logs\n  disable  Stop logging and preserve existing logs\n  status   Show logging state and latest log path\n  latest   Print the latest bounded JSONL log"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_command_surface_is_closed() {
        assert!(validate_arguments(&["enable".to_owned()]).is_ok());
        assert!(
            validate_arguments(&["latest".to_owned(), "--data-dir".to_owned(), "data".to_owned()])
                .is_ok()
        );
        assert!(validate_arguments(&["enable".to_owned(), "--json".to_owned()]).is_err());
        assert!(validate_arguments(&["unknown".to_owned()]).is_err());
    }
}
