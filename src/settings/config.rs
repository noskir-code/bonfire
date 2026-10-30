use std::{collections::HashMap, env, fs, path::PathBuf};

const DEFAULT_RUNTIME_FILE: &str = "configs/runtime.conf";

/// Path of the runtime file, overridable with `BONFIRE_CONFIG`.
fn runtime_config_path() -> PathBuf {
    env::var("BONFIRE_CONFIG")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_RUNTIME_FILE))
}

/// Parses `KEY=VALUE` lines. Empty lines and `#` comments are ignored,
/// surrounding quotes are stripped from values.
fn parse_runtime_file(content: &str) -> Result<HashMap<String, String>, String> {
    let mut values = HashMap::new();
    for (index, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((raw_key, raw_value)) = line.split_once('=') else {
            return Err(format!(
                "Invalid runtime config line {}: expected KEY=VALUE",
                index + 1
            ));
        };

        let key = raw_key.trim();
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        {
            return Err(format!(
                "Invalid runtime config key '{}' on line {}",
                key,
                index + 1
            ));
        }

        let mut value = raw_value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }

        if !value.trim().is_empty() {
            values.insert(key.to_string(), value.trim().to_string());
        }
    }

    Ok(values)
}

/// Environment variables take priority over values from the runtime file.
struct Settings {
    file_values: HashMap<String, String>,
}

impl Settings {
    fn load() -> Result<Self, String> {
        let path = runtime_config_path();
        let file_values = match fs::read_to_string(&path) {
            Ok(content) => parse_runtime_file(&content)
                .map_err(|err| format!("{} ({})", err, path.display()))?,
            Err(_) => HashMap::new(),
        };
        Ok(Self { file_values })
    }

    fn get(&self, name: &str) -> Option<String> {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| self.file_values.get(name).cloned())
    }

    fn parse<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.get(name) {
            Some(raw) => raw
                .parse()
                .map_err(|_| format!("{name} has an invalid value: '{raw}'")),
            None => Ok(default),
        }
    }

    fn flag(&self, name: &str, default: bool) -> bool {
        self.get(name)
            .map(|raw| {
                matches!(
                    raw.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(default)
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub database_url: Option<String>,
    pub db_max_connections: u32,
    pub max_request_body_mb: usize,
    pub log_level: String,
    pub secure_cookies: bool,
    pub session_inactivity_days: i64,
    /// How long "stay signed in" lasts without visiting.
    pub remember_me_days: i32,
}

impl Config {
    pub fn from_runtime() -> Result<Self, String> {
        let settings = Settings::load()?;

        Ok(Self {
            host: settings
                .get("APP_HOST")
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            port: settings.parse("APP_PORT", 8080)?,
            database_url: settings.get("DATABASE_URL"),
            db_max_connections: settings.parse("DB_MAX_CONNECTIONS", 20)?,
            max_request_body_mb: settings.parse("MAX_REQUEST_BODY_MB", 20)?,
            log_level: settings
                .get("LOG_LEVEL")
                .unwrap_or_else(|| "info".to_string()),
            secure_cookies: settings.flag("COOKIE_SECURE", !cfg!(debug_assertions)),
            session_inactivity_days: settings.parse("SESSION_INACTIVITY_DAYS", 7)?,
            remember_me_days: settings.parse("REMEMBER_ME_DAYS", 30)?,
        })
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
