use anyhow::{Result, ensure};
use futures::future::BoxFuture;
use http_client::{
    AsyncBody, HttpClient, RedirectPolicy, Request, RequestTimeout, Response, Url,
    http::HeaderValue,
};
use std::{sync::Arc, time::Duration};

pub(super) struct CatalogClient {
    inner: Arc<dyn HttpClient>,
}

fn allowed(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && match url.host_str() {
            Some("api.zed.dev") => {
                url.path() == "/extensions" || url.path().starts_with("/extensions/")
            }
            Some("zed-extensions.nyc3.digitaloceanspaces.com") => {
                url.path().starts_with("/extensions/")
            }
            _ => false,
        }
}

impl CatalogClient {
    pub(super) fn new(inner: Arc<dyn HttpClient>) -> Self {
        Self { inner }
    }
}

impl HttpClient for CatalogClient {
    fn send(&self, request: Request<AsyncBody>) -> BoxFuture<'static, Result<Response<AsyncBody>>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            ensure!(
                request.method() == http_client::http::Method::GET,
                "Extension catalogue only permits GET requests"
            );
            let mut url = Url::parse(&request.uri().to_string())?;
            for _ in 0..5 {
                ensure!(
                    allowed(&url),
                    "Extension catalogue destination is not permitted"
                );
                // Rebuild each hop so application credentials cannot reach the catalogue or CDN.
                let request = Request::builder()
                    .uri(url.as_str())
                    .extension(RedirectPolicy::NoFollow)
                    .extension(RequestTimeout(Duration::from_secs(60)))
                    .body(AsyncBody::empty())?;
                let response = inner.send(request).await?;
                if !response.status().is_redirection() {
                    return Ok(response);
                }
                let location = response
                    .headers()
                    .get(http_client::http::header::LOCATION)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Extension catalogue redirect has no destination")
                    })?
                    .to_str()?;
                url = url.join(location)?;
            }
            anyhow::bail!("Too many extension catalogue redirects")
        })
    }
    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }
    fn proxy(&self) -> Option<&Url> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::FakeHttpClient;

    #[test]
    fn catalogue_destinations_are_scoped() -> Result<()> {
        for url in [
            "https://api.zed.dev/extensions",
            "https://api.zed.dev/extensions/lua/download",
            "https://zed-extensions.nyc3.digitaloceanspaces.com/extensions/lua/archive.tar.gz",
        ] {
            assert!(allowed(&Url::parse(url)?));
        }
        for url in [
            "http://api.zed.dev/extensions",
            "https://api.zed.dev/telemetry",
            "https://api.zed.dev/extensions-evil",
            "https://api.zed.dev:8443/extensions",
            "https://name:secret@api.zed.dev/extensions",
            "https://127.0.0.1/extensions",
            "https://api.zed.dev.evil.test/extensions",
        ] {
            assert!(!allowed(&Url::parse(url)?));
        }
        Ok(())
    }

    #[test]
    fn redirects_strip_credentials_and_use_explicit_policy() -> Result<()> {
        futures::executor::block_on(async {
            let inner = FakeHttpClient::create(|request| async move {
                assert!(!request.headers().contains_key("authorization"));
                assert!(!request.headers().contains_key("cookie"));
                assert!(matches!(
                    request.extensions().get::<RedirectPolicy>(),
                    Some(RedirectPolicy::NoFollow)
                ));
                if request.uri().host() == Some("api.zed.dev") {
                    Ok(Response::builder().status(307).header("location", "https://zed-extensions.nyc3.digitaloceanspaces.com/extensions/lua/archive.tar.gz").body(AsyncBody::empty())?)
                } else {
                    Ok(Response::builder().status(200).body(AsyncBody::empty())?)
                }
            });
            let client = CatalogClient::new(inner);
            let response = client
                .send(
                    Request::builder()
                        .uri("https://api.zed.dev/extensions/lua/download")
                        .header("authorization", "Bearer synthetic-test")
                        .header("cookie", "synthetic-test")
                        .body(AsyncBody::empty())?,
                )
                .await?;
            assert_eq!(response.status(), 200);
            Ok(())
        })
    }

    #[test]
    fn rejects_writes_and_redirects_outside_catalogue() -> Result<()> {
        futures::executor::block_on(async {
            let inner = FakeHttpClient::create(|_| async move {
                Ok(Response::builder()
                    .status(307)
                    .header("location", "http://127.0.0.1/private")
                    .body(AsyncBody::empty())?)
            });
            let client = CatalogClient::new(inner);
            for method in ["GET", "POST"] {
                let result = client
                    .send(
                        Request::builder()
                            .method(method)
                            .uri("https://api.zed.dev/extensions")
                            .body(AsyncBody::empty())?,
                    )
                    .await;
                assert!(result.is_err());
            }
            Ok(())
        })
    }
}
