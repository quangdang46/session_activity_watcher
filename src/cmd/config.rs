use crate::cmd::common::home_dir;
use crate::cmd::watch::StuckAction;
use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const DEFAULT_TIMEOUT_SECS: u64 = 130;

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[arg(long)]
    pub list: bool,

    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<TimeoutSetting>,

    #[arg(long, value_enum)]
    pub on_stuck: Option<StuckAction>,

    #[arg(long)]
    pub reset: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct SawConfig {
    pub timeout: TimeoutSetting,
    pub on_stuck: StuckAction,
}

impl<'de> Deserialize<'de> for SawConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        struct PartialSawConfig {
            timeout: Option<TimeoutSetting>,
            on_stuck: Option<StuckAction>,
        }

        let partial = PartialSawConfig::deserialize(deserializer)?;
        Ok(Self {
            timeout: partial.timeout.unwrap_or_default(),
            on_stuck: partial.on_stuck.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutSetting(u64);

impl TimeoutSetting {
    #[cfg(test)]
    pub fn from_secs(secs: u64) -> Self {
        Self(secs)
    }

    pub fn as_secs(self) -> u64 {
        self.0
    }
}

impl Default for TimeoutSetting {
    fn default() -> Self {
        Self(DEFAULT_TIMEOUT_SECS)
    }
}

impl Display for TimeoutSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_duration_secs(self.0))
    }
}

impl FromStr for TimeoutSetting {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        parse_duration_secs(value).map(Self)
    }
}

impl Serialize for TimeoutSetting {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for TimeoutSetting {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum TimeoutRepr {
            Seconds(u64),
            Text(String),
        }

        match TimeoutRepr::deserialize(deserializer)? {
            TimeoutRepr::Seconds(secs) => Ok(Self(secs)),
            TimeoutRepr::Text(value) => value.parse().map_err(serde::de::Error::custom),
        }
    }
}

pub fn run(args: ConfigArgs) -> Result<()> {
    let path = config_file_path()?;
    let mut config = ensure_config_file(&path)?;
    let mut changed = false;

    if args.reset {
        config = SawConfig::default();
        changed = true;
    }
    if let Some(timeout) = args.timeout {
        config.timeout = timeout;
        changed = true;
    }
    if let Some(on_stuck) = args.on_stuck {
        config.on_stuck = on_stuck;
        changed = true;
    }

    if changed {
        write_config(&path, &config)?;
    }

    if args.list || !changed {
        print!("{}", render_config(&config)?);
    }

    Ok(())
}

pub fn load_user_config() -> Result<SawConfig> {
    let path = config_file_path()?;
    if !path.exists() {
        return Ok(SawConfig::default());
    }
    read_config(&path)
}

pub fn merge_timeout_secs(cli_timeout: Option<TimeoutSetting>, config: &SawConfig) -> u64 {
    cli_timeout.unwrap_or(config.timeout).as_secs()
}

pub fn merge_on_stuck_action(cli_on_stuck: Option<StuckAction>, config: &SawConfig) -> StuckAction {
    cli_on_stuck.unwrap_or(config.on_stuck)
}

fn config_file_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/saw/config.toml"))
}

fn ensure_config_file(path: &Path) -> Result<SawConfig> {
    if path.exists() {
        return read_config(path);
    }

    let config = SawConfig::default();
    write_config(path, &config)?;
    Ok(config)
}

fn read_config(path: &Path) -> Result<SawConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read saw config at {}", path.display()))?;
    let value: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("failed to parse saw config at {}", path.display()))?;

    Ok(SawConfig {
        timeout: value
            .get("timeout")
            .map(|value| value.clone().try_into())
            .transpose()
            .context("failed to parse saw config timeout")?
            .unwrap_or_default(),
        on_stuck: value
            .get("on_stuck")
            .map(|value| value.clone().try_into())
            .transpose()
            .context("failed to parse saw config on_stuck")?
            .unwrap_or_default(),
    })
}

fn write_config(path: &Path, config: &SawConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    fs::write(path, render_config(config)?)
        .with_context(|| format!("failed to write saw config at {}", path.display()))
}

fn render_config(config: &SawConfig) -> Result<String> {
    let mut rendered = toml::to_string_pretty(config).context("failed to render saw config")?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

fn parse_duration_secs(value: &str) -> std::result::Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("duration cannot be empty".into());
    }

    let digits_len = value.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits_len == 0 {
        return Err(format!("duration must start with digits: {value}"));
    }

    let amount: u64 = value[..digits_len]
        .parse()
        .map_err(|_| format!("invalid duration value: {value}"))?;
    let unit = value[digits_len..].trim().to_ascii_lowercase();

    let multiplier = match unit.as_str() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        _ => return Err(format!("unsupported duration unit: {value}")),
    };

    amount
        .checked_mul(multiplier)
        .ok_or_else(|| format!("duration is too large: {value}"))
}

fn format_duration_secs(secs: u64) -> String {
    if secs != 0 && secs.is_multiple_of(60 * 60) {
        format!("{}h", secs / (60 * 60))
    } else if secs != 0 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        config_file_path, load_user_config, merge_on_stuck_action, merge_timeout_secs,
        render_config, run, ConfigArgs, SawConfig, TimeoutSetting,
    };
    use crate::cmd::common::home_env_test_lock;
    use crate::cmd::watch::StuckAction;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn creates_default_config_file_on_first_use() {
        let _lock = home_env_test_lock();
        let home = unique_temp_dir("config-create");
        let original_home = set_home(&home);

        run(ConfigArgs {
            list: false,
            timeout: None,
            on_stuck: None,
            reset: false,
        })
        .unwrap();

        let path = config_file_path().unwrap();
        assert!(path.exists());
        assert_eq!(load_user_config().unwrap(), SawConfig::default());

        restore_home(original_home);
    }

    #[test]
    fn list_render_contains_all_values() {
        let rendered = render_config(&SawConfig::default()).unwrap();

        assert!(rendered.contains("timeout = \"130s\""));
        assert!(rendered.contains("on_stuck = \"warn\""));
    }

    #[test]
    fn updates_individual_values() {
        let _lock = home_env_test_lock();
        let home = unique_temp_dir("config-update");
        let original_home = set_home(&home);

        run(ConfigArgs {
            list: false,
            timeout: Some(TimeoutSetting::from_secs(5 * 60)),
            on_stuck: Some(StuckAction::Kill),
            reset: false,
        })
        .unwrap();

        let config = load_user_config().unwrap();
        assert_eq!(config.timeout.as_secs(), 5 * 60);
        assert_eq!(config.on_stuck, StuckAction::Kill);

        restore_home(original_home);
    }

    #[test]
    fn reset_restores_defaults() {
        let _lock = home_env_test_lock();
        let home = unique_temp_dir("config-reset");
        let original_home = set_home(&home);

        run(ConfigArgs {
            list: false,
            timeout: Some(TimeoutSetting::from_secs(5 * 60)),
            on_stuck: Some(StuckAction::Kill),
            reset: false,
        })
        .unwrap();

        run(ConfigArgs {
            list: false,
            timeout: None,
            on_stuck: None,
            reset: true,
        })
        .unwrap();

        assert_eq!(load_user_config().unwrap(), SawConfig::default());

        restore_home(original_home);
    }

    #[test]
    fn serde_defaults_fill_missing_fields() {
        let _lock = home_env_test_lock();
        let home = unique_temp_dir("config-defaults");
        let original_home = set_home(&home);
        let path = config_file_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "on_stuck = \"kill\"\n").unwrap();

        let config = load_user_config().unwrap();
        assert_eq!(config.timeout, TimeoutSetting::default());
        assert_eq!(config.on_stuck, StuckAction::Kill);

        restore_home(original_home);
    }

    #[test]
    fn deserialize_saw_config_applies_defaults_for_missing_fields() {
        let config: SawConfig = toml::from_str("on_stuck = \"kill\"\n").unwrap();

        assert_eq!(config.timeout, TimeoutSetting::default());
        assert_eq!(config.on_stuck, StuckAction::Kill);
    }

    #[test]
    fn cli_values_override_file_values() {
        let config = SawConfig {
            timeout: TimeoutSetting::from_secs(5 * 60),
            on_stuck: StuckAction::Kill,
        };

        assert_eq!(merge_timeout_secs(None, &config), 5 * 60);
        assert_eq!(
            merge_timeout_secs(Some(TimeoutSetting::from_secs(45)), &config),
            45
        );
        assert_eq!(merge_on_stuck_action(None, &config), StuckAction::Kill);
        assert_eq!(
            merge_on_stuck_action(Some(StuckAction::Warn), &config),
            StuckAction::Warn
        );
    }

    #[test]
    fn timeout_setting_parses_human_durations() {
        assert_eq!("45".parse::<TimeoutSetting>().unwrap().as_secs(), 45);
        assert_eq!("45s".parse::<TimeoutSetting>().unwrap().as_secs(), 45);
        assert_eq!("5m".parse::<TimeoutSetting>().unwrap().as_secs(), 300);
        assert_eq!("2h".parse::<TimeoutSetting>().unwrap().as_secs(), 7_200);
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("saw-{prefix}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn set_home(home: &PathBuf) -> Option<OsString> {
        let original = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        original
    }

    fn restore_home(original_home: Option<OsString>) {
        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
    }
}
