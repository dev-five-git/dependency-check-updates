//! Explicit endpoint authorization. Never evaluate Gradle credential code.
use crate::project::error;
use dependency_check_updates_core::DcuError;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(rename = "schemaVersion")]
    version: u32,
    repositories: Vec<Repository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Repository {
    url: String,
    username_env: Option<String>,
    password_env: Option<String>,
    token_env: Option<String>,
}

pub(crate) fn normalize(url: &str) -> Result<String, DcuError> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| error("maven config", "invalid repository URL"))?;
    let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if (parsed.scheme() != "https" && !(local && parsed.scheme() == "http"))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.host_str().is_none()
    {
        return Err(error(
            "maven config",
            "repository URLs require HTTPS (HTTP only for loopback), no embedded credentials, query or fragment",
        ));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

pub(crate) fn load(path: &Path) -> Result<HashMap<String, HeaderMap>, DcuError> {
    let metadata = std::fs::metadata(path).map_err(|source| DcuError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.len() > 1024 * 1024 {
        return Err(error("maven config", "configuration exceeds size limit"));
    }
    let text = std::fs::read_to_string(path).map_err(|source| DcuError::Io {
        path: path.to_owned(),
        source,
    })?;
    parse(&text, |name| std::env::var(name).ok())
}

fn parse(
    text: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<HashMap<String, HeaderMap>, DcuError> {
    use base64::Engine;
    let config: Config = serde_json::from_str(text).map_err(|_| error("maven config", "invalid configuration; credentials must use environment-variable names, not literal values"))?;
    if config.version != 1 || config.repositories.len() > 128 {
        return Err(error(
            "maven config",
            "unsupported schema or repository count",
        ));
    }
    let secret = |name: &str| {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(error(
                "maven config",
                "invalid credential environment-variable name",
            ));
        }
        env(name).filter(|s| !s.is_empty()).ok_or_else(|| {
            error(
                "maven config",
                format!("missing credential environment variable {name}"),
            )
        })
    };
    let mut result = HashMap::new();
    for repo in config.repositories {
        let url = normalize(&repo.url)?;
        let mut headers = HeaderMap::new();
        let auth = match (repo.token_env, repo.username_env, repo.password_env) {
            (Some(token), None, None) => Some(format!("Bearer {}", secret(&token)?)),
            (None, Some(user), Some(password)) => Some(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!(
                    "{}:{}",
                    secret(&user)?,
                    secret(&password)?
                ))
            )),
            (None, None, None) => None,
            _ => {
                return Err(error(
                    "maven config",
                    "use tokenEnv OR both usernameEnv/passwordEnv",
                ));
            }
        };
        if let Some(auth) = auth {
            let mut value = HeaderValue::from_str(&auth)
                .map_err(|_| error("maven config", "invalid credential header"))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        if result.insert(url, headers).is_some() {
            return Err(error("maven config", "duplicate repository URL"));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_auth_validation_never_prints_secrets() {
        let config = r#"{"schemaVersion":1,"repositories":[{"url":"https://repo.example/maven/","tokenEnv":"TOKEN"}]}"#;
        let result = parse(config, |_| Some("secret-token".into())).unwrap();
        assert!(result["https://repo.example/maven"][AUTHORIZATION].is_sensitive());
        assert_eq!(
            result["https://repo.example/maven"][AUTHORIZATION],
            "Bearer secret-token"
        );
        assert!(parse(config, |_| None).is_err());
        for url in [
            "http://repo.example/maven",
            "https://user:password@repo.example/maven",
            "https://repo.example/maven?token=secret",
        ] {
            assert!(
                !normalize(url)
                    .unwrap_err()
                    .to_string()
                    .contains("password@")
            );
        }
        assert!(normalize("http://127.0.0.1:1234/maven").is_ok());
        assert!(
            parse(&config.replace("TOKEN", "bad\u{a}name"), |_| Some(
                "secret".into()
            ))
            .is_err()
        );
    }
}
