//! "Import cURL": turns a `curl` command line from the clipboard into a
//! request. It tokenizes shell quoting and reads flags; it never runs the
//! command, and `$(...)`, backticks and `$VAR` stay as literal text.

use crate::collection::{Request, slug};
use anyhow::{Result, bail};
use base64::Engine as _;

/// Shell-style word splitting: single quotes are literal, double quotes
/// honour `\"`, `\\`, `\$` and backtick escapes, `$'...'` takes ANSI-C
/// escapes, and a backslash before a newline continues the line.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' => {
                in_word = true;
                if current.is_empty() && chars.peek().is_none() {
                    continue;
                }
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    current.push(inner);
                }
            }
            '"' => {
                in_word = true;
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(escaped @ ('"' | '\\' | '$' | '`')) => current.push(escaped),
                            Some('\n') => {}
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => current.push('\\'),
                        },
                        other => current.push(other),
                    }
                }
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                in_word = true;
                while let Some(inner) = chars.next() {
                    match inner {
                        '\'' => break,
                        '\\' => match chars.next() {
                            Some('n') => current.push('\n'),
                            Some('t') => current.push('\t'),
                            Some('r') => current.push('\r'),
                            Some('\'') => current.push('\''),
                            Some('\\') => current.push('\\'),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => current.push('\\'),
                        },
                        other => current.push(other),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(other) => {
                    in_word = true;
                    current.push(other);
                }
            },
            character if character.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// Flags whose argument is not something we keep.
const SKIPPED_WITH_VALUE: &[&str] = &[
    "-o", "--output", "-m", "--max-time", "--connect-timeout", "-w", "--write-out", "--retry",
    "-x", "--proxy", "-c", "--cookie-jar", "-E", "--cert", "--cacert", "--key", "-T",
    "--upload-file", "--resolve", "--interface", "--max-redirs", "--limit-rate", "-r",
    "--range", "--trace", "--trace-ascii", "--stderr", "--dump-header", "-D", "--config",
    "-K", "--unix-socket", "--abstract-unix-socket", "--proto", "--retry-delay",
    "--retry-max-time", "--happy-eyeballs-timeout-ms", "--expect100-timeout", "--aws-sigv4",
    "--oauth2-bearer", "--tlsuser", "--tlspassword", "-U", "--proxy-user",
];

fn split_option(token: &str) -> (&str, Option<&str>) {
    if token.starts_with("--")
        && let Some((flag, value)) = token.split_once('=')
    {
        return (flag, Some(value));
    }
    (token, None)
}

/// Parses a `curl ...` command into a request. Multiple `-d` values join
/// with `&` like curl does; `-d` without `-X` means POST; `-u` becomes a
/// Basic `Authorization` header; `-G` moves data into the query string.
pub fn parse_curl(text: &str) -> Result<Request> {
    let tokens = tokenize(text);
    let mut tokens = tokens.into_iter().peekable();
    match tokens.next() {
        Some(first) if first == "curl" || first.ends_with("/curl") => {}
        _ => bail!("the clipboard does not start with a curl command"),
    }
    let mut method: Option<String> = None;
    let mut url: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut data: Vec<String> = Vec::new();
    let mut json = false;
    let mut get_with_data = false;
    let mut user: Option<String> = None;
    while let Some(token) = tokens.next() {
        let (flag, inline_value) = split_option(&token);
        let value = |tokens: &mut std::iter::Peekable<std::vec::IntoIter<String>>| -> Result<String> {
            if let Some(value) = inline_value {
                return Ok(value.to_string());
            }
            tokens
                .next()
                .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
        };
        match flag {
            "-X" | "--request" => method = Some(value(&mut tokens)?.to_ascii_uppercase()),
            "-H" | "--header" => {
                let header = value(&mut tokens)?;
                if let Some((name, header_value)) = header.split_once(':') {
                    headers.push((name.trim().to_string(), header_value.trim().to_string()));
                }
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-ascii"
            | "--data-urlencode" => {
                let mut chunk = value(&mut tokens)?;
                // `@file` would read a file in curl; keep it as text, never read it.
                if flag != "--data-raw" && chunk.starts_with('@') {
                    chunk = format!("<file: {}>", &chunk[1..]);
                }
                data.push(chunk);
            }
            "--json" => {
                json = true;
                data.push(value(&mut tokens)?);
            }
            "-u" | "--user" => user = Some(value(&mut tokens)?),
            "--url" => url = Some(value(&mut tokens)?),
            "-A" | "--user-agent" => headers.push(("User-Agent".into(), value(&mut tokens)?)),
            "-e" | "--referer" => headers.push(("Referer".into(), value(&mut tokens)?)),
            "-b" | "--cookie" => {
                let cookie = value(&mut tokens)?;
                if cookie.contains('=') {
                    headers.push(("Cookie".into(), cookie));
                }
            }
            "-I" | "--head" => method = Some("HEAD".into()),
            "-G" | "--get" => get_with_data = true,
            "-F" | "--form" | "--form-string" => {
                let field = value(&mut tokens)?;
                data.push(field);
            }
            flag if SKIPPED_WITH_VALUE.contains(&flag) => {
                if inline_value.is_none() {
                    tokens.next();
                }
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {}
            other => {
                if url.is_none() {
                    url = Some(other.to_string());
                }
            }
        }
    }
    let Some(mut url) = url else {
        bail!("no URL found in the curl command");
    };
    if !url.contains("://") {
        url = format!("https://{url}");
    }
    let mut body = data.join("&");
    if get_with_data && !body.is_empty() {
        let separator = if url.contains('?') { "&" } else { "?" };
        url = format!("{url}{separator}{body}");
        body.clear();
    }
    let method = method.unwrap_or_else(|| {
        if body.is_empty() { "GET" } else { "POST" }.to_string()
    });
    if json {
        if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-type")) {
            headers.push(("Content-Type".into(), "application/json".into()));
        }
        if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("accept")) {
            headers.push(("Accept".into(), "application/json".into()));
        }
    }
    if let Some(user) = user
        && !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
    {
        let encoded = base64::engine::general_purpose::STANDARD.encode(user);
        headers.push(("Authorization".into(), format!("Basic {encoded}")));
    }
    let name = name_for(&method, &url);
    Ok(Request {
        name,
        method,
        url,
        headers,
        body,
    })
}

fn name_for(method: &str, url: &str) -> String {
    let path = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split_once('/').map(|(_, path)| path))
        .unwrap_or_default();
    let path = path.split('?').next().unwrap_or_default();
    let tail = slug(path);
    if tail == "request" {
        format!("{} {}", method.to_ascii_lowercase(), slug(url))
    } else {
        format!("{} {tail}", method.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_handles_quotes_escapes_and_continuations() {
        assert_eq!(
            tokenize("curl -H 'A: b c' --data \"{\\\"k\\\": 1}\" \\\n  https://x.test"),
            vec!["curl", "-H", "A: b c", "--data", "{\"k\": 1}", "https://x.test"]
        );
        assert_eq!(tokenize("a $'x\\ny' ''"), vec!["a", "x\ny", ""]);
        assert_eq!(tokenize("a\\ b c"), vec!["a b", "c"]);
        assert_eq!(tokenize("  "), Vec::<String>::new());
    }

    #[test]
    fn post_with_header_and_body() {
        let request = parse_curl(
            "curl -X POST https://a.test/x -H 'A: b' -H \"Content-Type: application/json\" -d '{\"k\":1}'",
        )
        .expect("parsed");
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "https://a.test/x");
        assert_eq!(request.headers.len(), 2);
        assert_eq!(request.header("a"), Some("b"));
        assert_eq!(request.body, "{\"k\":1}");
        assert_eq!(request.name, "post x");
    }

    #[test]
    fn data_implies_post_and_user_becomes_basic_auth() {
        let request = parse_curl("curl -u u:p --data a=1 --data b=2 'https://a.test/form?x=1'")
            .expect("parsed");
        assert_eq!(request.method, "POST");
        assert_eq!(request.body, "a=1&b=2");
        assert_eq!(request.header("authorization"), Some("Basic dTpw"));
        let get = parse_curl("curl -G -d q=raven https://a.test/search").expect("parsed");
        assert_eq!(get.method, "GET");
        assert_eq!(get.url, "https://a.test/search?q=raven");
        assert_eq!(get.body, "");
        let json = parse_curl("curl --json '{\"a\":1}' a.test/j").expect("parsed");
        assert_eq!(json.method, "POST");
        assert_eq!(json.url, "https://a.test/j");
        assert_eq!(json.header("content-type"), Some("application/json"));
        let head = parse_curl("curl -I https://a.test/").expect("parsed");
        assert_eq!(head.method, "HEAD");
    }

    #[test]
    fn skipped_flags_and_long_form_values() {
        let request = parse_curl(
            "curl --request=PUT -s -L -o out.bin --max-time 5 --header='X: y' https://a.test/p -d @secret.txt",
        )
        .expect("parsed");
        assert_eq!(request.method, "PUT");
        assert_eq!(request.url, "https://a.test/p");
        assert_eq!(request.header("x"), Some("y"));
        assert_eq!(request.body, "<file: secret.txt>", "files are never read");
    }

    #[test]
    fn shell_expansions_stay_literal_and_non_curl_is_refused() {
        let request = parse_curl("curl https://a.test/$(rm -rf /) -H \"T: $TOKEN\" -H 'U: `id`'")
            .expect("parsed");
        assert_eq!(request.url, "https://a.test/$(rm");
        assert_eq!(request.header("t"), Some("$TOKEN"));
        assert_eq!(request.header("u"), Some("`id`"));
        assert!(parse_curl("wget https://a.test").is_err());
        assert!(parse_curl("curl -X POST").is_err());
        assert!(parse_curl("").is_err());
    }
}
