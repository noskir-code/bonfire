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

/// How the connection to the mail server is protected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpSecurity {
    /// Plain connection upgraded to TLS, usually port 587.
    StartTls,
    /// TLS from the start, usually port 465.
    Tls,
    /// No encryption, only for a relay on the same machine.
    None,
}

#[derive(Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Sender, like `Bonfire <noreply@example.com>`.
    pub from: String,
    /// Development mail catcher, see `SmtpConfig::mailpit`.
    pub is_mailpit: bool,
}

// Written by hand so the password never ends up in logs.
impl std::fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("from", &self.from)
            .field("is_mailpit", &self.is_mailpit)
            .finish()
    }
}

impl SmtpConfig {
    /// Mailpit (https://mailpit.axllent.org) with its default ports: it catches
    /// every email and shows them in a web inbox, nothing reaches real people.
    pub fn mailpit() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 1025,
            security: SmtpSecurity::None,
            username: None,
            password: None,
            from: "Bonfire <noreply@bonfire.localhost>".to_string(),
            is_mailpit: true,
        }
    }

    /// `None` when neither `SMTP_HOST` nor `SMTP_PROVIDER` is set: in debug builds
    /// emails then go to Mailpit if it runs, else in the log (see `mail.rs`);
    /// release builds can't send emails.
    fn from_settings(settings: &Settings) -> Result<Option<Self>, String> {
        let preset = match settings.get("SMTP_PROVIDER").map(|name| name.to_ascii_lowercase()) {
            None => None,
            Some(name) if name == "mailpit" => Some(Self::mailpit()),
            Some(other) => {
                return Err(format!("SMTP_PROVIDER has an invalid value: '{other}', known: mailpit"));
            }
        };
        let Some(host) = settings
            .get("SMTP_HOST")
            .or_else(|| preset.as_ref().map(|preset| preset.host.clone()))
        else {
            return Ok(None);
        };
        // Values set in the configuration win over the preset.
        let default_security = match &preset {
            Some(preset) => preset.security,
            None => SmtpSecurity::StartTls,
        };
        let security = match settings.get("SMTP_SECURITY").map(|value| value.to_ascii_lowercase()) {
            None => default_security,
            Some(value) => match value.as_str() {
                "starttls" => SmtpSecurity::StartTls,
                "tls" | "ssl" => SmtpSecurity::Tls,
                "none" => SmtpSecurity::None,
                other => return Err(format!("SMTP_SECURITY has an invalid value: '{other}'")),
            },
        };
        let default_port = match (&preset, security) {
            (Some(preset), _) => preset.port,
            (None, SmtpSecurity::StartTls) => 587,
            (None, SmtpSecurity::Tls) => 465,
            (None, SmtpSecurity::None) => 25,
        };
        let from = match (settings.get("SMTP_FROM"), &preset) {
            (Some(from), _) => from,
            (None, Some(preset)) => preset.from.clone(),
            (None, None) => return Err("SMTP_FROM is required when SMTP_HOST is set".to_string()),
        };
        Ok(Some(Self {
            host,
            port: settings.parse("SMTP_PORT", default_port)?,
            security,
            username: settings.get("SMTP_USERNAME"),
            password: settings.get("SMTP_PASSWORD"),
            from,
            is_mailpit: preset.is_some(),
        }))
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
    /// Address of the site as users reach it, used in links sent by email.
    /// Never taken from the request, which an attacker controls.
    pub public_base_url: String,
    pub smtp: Option<SmtpConfig>,
    /// Fediverse (ActivityPub): other servers can find and follow local users.
    pub federation: bool,
    /// Lets federation reach `http://` and private addresses. Only to test
    /// two servers on one machine: on a real server it lets anyone make
    /// Bonfire send requests inside its network.
    pub federation_allow_insecure: bool,
}

impl Config {
    pub fn from_runtime() -> Result<Self, String> {
        let settings = Settings::load()?;

        let host = settings
            .get("APP_HOST")
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = settings.parse("APP_PORT", 8080)?;
        let public_base_url = settings
            .get("PUBLIC_BASE_URL")
            .unwrap_or_else(|| format!("http://{host}:{port}"))
            .trim_end_matches('/')
            .to_string();

        Ok(Self {
            public_base_url,
            smtp: SmtpConfig::from_settings(&settings)?,
            host,
            port,
            database_url: settings.get("DATABASE_URL"),
            db_max_connections: settings.parse("DB_MAX_CONNECTIONS", 20)?,
            max_request_body_mb: settings.parse("MAX_REQUEST_BODY_MB", 20)?,
            log_level: settings
                .get("LOG_LEVEL")
                .unwrap_or_else(|| "info".to_string()),
            secure_cookies: settings.flag("COOKIE_SECURE", !cfg!(debug_assertions)),
            session_inactivity_days: settings.parse("SESSION_INACTIVITY_DAYS", 7)?,
            remember_me_days: settings.parse("REMEMBER_ME_DAYS", 30)?,
            federation: settings.flag("FEDERATION", true),
            federation_allow_insecure: settings.flag("FEDERATION_ALLOW_INSECURE", false),
        })
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
