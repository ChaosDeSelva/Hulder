use anyhow::{Context as _, Result, bail};
use serde_json::Value;
use std::{
    fs::File,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    num::NonZeroU16,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

fn factory_port(setting: &str, field: &str, fallback: u16) -> Result<u16> {
    let override_port = std::env::var(setting).ok();
    let root = std::env::var_os("VIKING_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::home_dir().map(|home| home.join("Library/Application Support/Viking Factory"))
        });
    let config = root
        .filter(|_| std::env::var_os("FACTORY_LAUNCHER").is_none())
        .map(|dir| dir.join("config.json"));
    resolve_port(override_port.as_deref(), config.as_deref(), field, fallback)
}

fn resolve_port(
    value: Option<&str>,
    config: Option<&Path>,
    field: &str,
    fallback: u16,
) -> Result<u16> {
    if let Some(value) = value.filter(|s| !s.is_empty()) {
        return Ok(value
            .parse::<NonZeroU16>()
            .with_context(|| format!("Invalid {field} override"))?
            .get());
    }
    let Some(config) = config else {
        return Ok(fallback);
    };
    let file = match File::open(config) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(fallback),
        Err(error) => return Err(error).context("Cannot open the Viking Factory port settings"),
    };
    let settings: Value =
        serde_json::from_reader(file).context("Cannot read the Viking Factory port settings")?;
    Ok(
        serde_json::from_value::<NonZeroU16>(settings[field].clone())
            .with_context(|| format!("Invalid {field} in the Viking Factory settings"))?
            .get(),
    )
}

pub fn console_url(task_id: Option<&str>) -> Result<String> {
    Ok(console_url_at(
        factory_port("HEIMDALL_PORT", "heimdall_port", 4320)?,
        task_id,
    ))
}

fn console_url_at(port: u16, task_id: Option<&str>) -> String {
    match task_id.filter(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    }) {
        Some(id) => format!("http://127.0.0.1:{port}/?task={id}"),
        None => format!("http://127.0.0.1:{port}/"),
    }
}

pub fn read_token(path: &Path) -> Result<String> {
    let mut token = String::new();
    File::open(path)
        .context("Cannot open the selected Mimir token file")?
        .take(4097)
        .read_to_string(&mut token)
        .context("Cannot read Mimir credentials")?;
    let token = token.trim();
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        bail!("The selected file does not contain a valid Mimir access token");
    }
    Ok(token.to_owned())
}

pub fn request(token_path: &Path, endpoint: &str) -> Result<Value> {
    let port = if let Ok(port) = std::env::var("HULDER_FACTORY_PORT") {
        port.parse::<NonZeroU16>()
            .context("HULDER_FACTORY_PORT must be a local TCP port")?
            .get()
    } else {
        factory_port("MIMIR_PORT", "mimir_port", 7700)?
    };
    request_at(
        SocketAddr::from(([127, 0, 0, 1], port)),
        token_path,
        endpoint,
    )
}

fn request_at(address: SocketAddr, token_path: &Path, endpoint: &str) -> Result<Value> {
    if !address.ip().is_loopback()
        || !(endpoint == "/v1/snapshot"
            || endpoint.strip_prefix("/v1/tasks/").is_some_and(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            }))
    {
        bail!("Factory reads are restricted to local snapshot and task endpoints");
    }
    let token = read_token(token_path)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream =
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).with_context(|| {
            format!("Mimir is unavailable at {address}. Start the local factory and refresh")
        })?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "GET {endpoint} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("Mimir did not respond within five seconds");
        }
        stream.set_read_timeout(Some(remaining))?;
        let mut buffer = [0; 8192];
        let count = stream
            .read(&mut buffer)
            .context("Unable to read the local Mimir response")?;
        if count == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..count]);
        if response.len() > RESPONSE_LIMIT {
            bail!("Mimir response exceeds the native view's 2 MiB limit");
        }
    }
    parse_response(&response)
}

fn parse_response(response: &[u8]) -> Result<Value> {
    let split = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .context("Invalid Mimir HTTP response")?;
    let headers = std::str::from_utf8(&response[..split])?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("Missing response status")?;
    match status {
        "200" => {}
        "401" | "403" => {
            bail!("Mimir rejected the credentials. Select its current auth-token file")
        }
        "404" => bail!("The selected task is no longer available"),
        _ => bail!("Mimir returned HTTP {status}; the last successful snapshot is retained"),
    }
    if headers
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding:"))
    {
        bail!("Unsupported Mimir response framing");
    }
    let body = &response[split + 4..];
    if let Some(length) = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then_some(value.trim())
    }) {
        if length.parse::<usize>()? != body.len() {
            bail!("Incomplete Mimir response");
        }
    }
    serde_json::from_slice(body).context("Mimir did not return valid JSON")
}

pub fn items(value: &Value, key: &str) -> Vec<Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}
pub fn field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("—")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn console_links_keep_task_context_and_reject_query_injection() {
        assert_eq!(
            console_url_at(44320, Some("tsk_demo")),
            "http://127.0.0.1:44320/?task=tsk_demo"
        );
        assert_eq!(
            console_url_at(44320, Some("tsk_bad&token=secret")),
            "http://127.0.0.1:44320/"
        );
        assert_eq!(console_url_at(4320, None), "http://127.0.0.1:4320/");
    }

    #[test]
    fn resolves_only_valid_ports_from_managed_settings_and_overrides() {
        let config = tempfile::NamedTempFile::new().expect("temporary config");
        std::fs::write(config.path(), r#"{"mimir_port":47700}"#).expect("write config");
        let resolve = |port| resolve_port(port, Some(config.path()), "mimir_port", 7700);
        assert_eq!(resolve(None).expect("managed port"), 47700);
        assert_eq!(resolve(Some("7701")).expect("explicit port"), 7701);
        assert!(resolve(Some("0")).is_err());
        std::fs::write(config.path(), r#"{"mimir_port":"localhost"}"#).expect("replace config");
        assert!(resolve(None).is_err());
        assert_eq!(
            resolve_port(None, None, "mimir_port", 7700).expect("source default"),
            7700
        );
    }

    #[test]
    fn rejects_authentication_and_partial_responses() {
        assert!(parse_response(b"HTTP/1.1 401 Unauthorized\r\n\r\nprivate").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{}").is_err());
        assert!(
            parse_response(b"HTTP/1.1 302 Found\r\nLocation: https://elsewhere\r\n\r\n").is_err()
        );
    }
    #[test]
    fn accepts_bounded_snapshot_response() {
        let value = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n{\"tasks\":[]}")
            .expect("valid response");
        assert!(items(&value, "tasks").is_empty());
    }
    #[test]
    fn refuses_remote_hosts_and_endpoint_injection_before_credentials() {
        let path = Path::new("/missing-token");
        assert!(
            request_at(
                "192.0.2.1:7700".parse().expect("address"),
                path,
                "/v1/snapshot"
            )
            .expect_err("remote denied")
            .to_string()
            .contains("restricted")
        );
        assert!(
            request_at(
                "127.0.0.1:7700".parse().expect("address"),
                path,
                "/v1/tasks/../controls"
            )
            .expect_err("path denied")
            .to_string()
            .contains("restricted")
        );
    }
}
