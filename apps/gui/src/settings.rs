use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use thiserror::Error;

const SETTINGS_DIRECTORY: &str = "memelith";
const STORAGE_FILE: &str = "storage-root";
const TELEGRAM_ENABLED_FILE: &str = "telegram-enabled";
const TELEGRAM_TOKEN_FILE: &str = "telegram-token";
const VLM_BASE_URL_FILE: &str = "vlm-base-url";
const VLM_API_KEY_FILE: &str = "vlm-api-key";
const VLM_MODEL_FILE: &str = "vlm-model";
const VLM_REASONING_EFFORT_FILE: &str = "vlm-reasoning-effort";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelegramSettings {
    pub enabled: bool,
    pub token: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct VlmSettings {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

pub fn load_vlm_settings() -> Result<VlmSettings, SettingsError> {
    load_vlm_settings_from(&settings_directory()?)
}

fn load_vlm_settings_from(directory: &Path) -> Result<VlmSettings, SettingsError> {
    let read = |name: &str| -> Result<String, SettingsError> {
        match fs::read_to_string(directory.join(name)) {
            Ok(value) => Ok(value.trim().to_owned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    };
    let reasoning = read(VLM_REASONING_EFFORT_FILE)?;
    Ok(VlmSettings {
        base_url: read(VLM_BASE_URL_FILE)?,
        api_key: read(VLM_API_KEY_FILE)?,
        model: read(VLM_MODEL_FILE)?,
        reasoning_effort: (!reasoning.is_empty()).then_some(reasoning),
    })
}

pub fn save_vlm_settings(settings: &VlmSettings) -> Result<(), SettingsError> {
    save_vlm_settings_to(&settings_directory()?, settings)
}

fn save_vlm_settings_to(directory: &Path, settings: &VlmSettings) -> Result<(), SettingsError> {
    use std::io::Write;
    fs::create_dir_all(directory)?;
    fs::write(
        directory.join(VLM_BASE_URL_FILE),
        format!("{}\n", settings.base_url.trim()),
    )?;
    let key_path = directory.join(VLM_API_KEY_FILE);
    // Restrict the file before writing a secret, including existing files with broad modes.
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut key_file = options.open(&key_path)?;
    #[cfg(unix)]
    key_file.set_permissions(fs::Permissions::from_mode(0o600))?;
    key_file.set_len(0)?;
    writeln!(key_file, "{}", settings.api_key.trim())?;
    fs::write(
        directory.join(VLM_MODEL_FILE),
        format!("{}\n", settings.model.trim()),
    )?;
    fs::write(
        directory.join(VLM_REASONING_EFFORT_FILE),
        format!("{}\n", settings.reasoning_effort.as_deref().unwrap_or("")),
    )?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("the operating system did not provide a configuration directory")]
    ConfigurationDirectoryUnavailable,

    #[error("storage path is not valid UTF-8: {0}")]
    NonUtf8StoragePath(PathBuf),

    #[error("failed to access application settings: {0}")]
    Io(#[from] io::Error),
}

pub fn load_storage_root() -> Result<Option<PathBuf>, SettingsError> {
    load_storage_root_from(&settings_file()?)
}

pub fn save_storage_root(storage_root: &Path) -> Result<(), SettingsError> {
    save_storage_root_to(&settings_file()?, storage_root)
}

pub fn load_telegram_settings() -> Result<TelegramSettings, SettingsError> {
    let directory = settings_directory()?;
    let enabled = match fs::read_to_string(directory.join(TELEGRAM_ENABLED_FILE)) {
        Ok(raw) => match raw.trim() {
            "1" | "true" => true,
            "0" | "false" => false,
            value => {
                return Err(SettingsError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("saved Telegram enabled value `{value}` is invalid"),
                )));
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let token = match fs::read_to_string(directory.join(TELEGRAM_TOKEN_FILE)) {
        Ok(raw) => raw.trim().to_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    Ok(TelegramSettings { enabled, token })
}

pub fn save_telegram_settings(settings: &TelegramSettings) -> Result<(), SettingsError> {
    let directory = settings_directory()?;
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join(TELEGRAM_ENABLED_FILE),
        if settings.enabled {
            "true\n"
        } else {
            "false\n"
        },
    )?;
    let token_path = directory.join(TELEGRAM_TOKEN_FILE);
    fs::write(&token_path, format!("{}\n", settings.token.trim()))?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&token_path)?.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(token_path, permissions)?;
    }
    Ok(())
}

fn settings_file() -> Result<PathBuf, SettingsError> {
    Ok(settings_directory()?.join(STORAGE_FILE))
}

fn settings_directory() -> Result<PathBuf, SettingsError> {
    let configuration =
        dirs::config_dir().ok_or(SettingsError::ConfigurationDirectoryUnavailable)?;
    Ok(configuration.join(SETTINGS_DIRECTORY))
}

fn load_storage_root_from(path: &Path) -> Result<Option<PathBuf>, SettingsError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SettingsError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "saved storage path is empty",
        )));
    }
    Ok(Some(PathBuf::from(trimmed)))
}

fn save_storage_root_to(path: &Path, storage_root: &Path) -> Result<(), SettingsError> {
    let parent = path.parent().ok_or_else(|| {
        SettingsError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "settings file has no parent directory",
        ))
    })?;
    fs::create_dir_all(parent)?;
    let storage_root = storage_root
        .to_str()
        .ok_or_else(|| SettingsError::NonUtf8StoragePath(storage_root.to_path_buf()))?;
    fs::write(path, format!("{storage_root}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{VlmSettings, load_storage_root_from, save_storage_root_to};

    #[test]
    fn round_trips_the_selected_storage_root() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("config/storage-root");
        let storage = directory.path().join("Meme Library");

        assert_eq!(load_storage_root_from(&settings).unwrap(), None);
        save_storage_root_to(&settings, &storage).unwrap();
        assert_eq!(load_storage_root_from(&settings).unwrap(), Some(storage));
    }

    #[test]
    fn rejects_an_empty_saved_storage_root() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("storage-root");
        std::fs::write(&settings, "  \n").unwrap();

        let error = load_storage_root_from(&settings).unwrap_err();
        assert!(error.to_string().contains("saved storage path is empty"));
    }

    #[test]
    fn vlm_settings_defaults_are_explicit() {
        assert_eq!(VlmSettings::default().reasoning_effort, None);
    }

    #[test]
    fn vlm_configuration_round_trips_without_altering_existing_settings() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("config");
        assert_eq!(
            super::load_vlm_settings_from(&directory).unwrap(),
            VlmSettings::default()
        );
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join(super::STORAGE_FILE), "existing-library\n").unwrap();
        std::fs::write(
            directory.join(super::TELEGRAM_TOKEN_FILE),
            "existing-token\n",
        )
        .unwrap();
        let mut settings = VlmSettings {
            base_url: "https://example.test/v1".to_owned(),
            api_key: "local-test-key".to_owned(),
            model: "vision-model".to_owned(),
            reasoning_effort: Some("high".to_owned()),
        };
        super::save_vlm_settings_to(&directory, &settings).unwrap();
        assert_eq!(super::load_vlm_settings_from(&directory).unwrap(), settings);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key = directory.join(super::VLM_API_KEY_FILE);
            assert_eq!(
                std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        settings.reasoning_effort = None;
        settings.api_key = "short".to_owned();
        super::save_vlm_settings_to(&directory, &settings).unwrap();
        assert_eq!(super::load_vlm_settings_from(&directory).unwrap(), settings);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(directory.join(super::VLM_API_KEY_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::read_to_string(directory.join(super::STORAGE_FILE)).unwrap(),
            "existing-library\n"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join(super::TELEGRAM_TOKEN_FILE)).unwrap(),
            "existing-token\n"
        );
    }
}
