//! The Hoard: one auth store for Raven, the Panda and the picker. Profile
//! metadata lives in `~/.config/hulder/hoard/profiles.json`; secrets live in
//! the local Keychain. Other crates reach it through [`apply_to_request`],
//! [`cookies_for_host`] and [`remember_set_cookies`]; nothing here ever
//! writes a secret anywhere but the Keychain.

pub mod borrow;
pub mod model;
pub mod modal;
pub mod store;

pub use model::{Cookie, Profile, ProfileKind, Secret, redaction_set};

use anyhow::{Context as _, Result, anyhow};
use cdp::CdpClient;
use gpui::{App, AppContext as _, Context, Entity, Global, Task, actions};
use std::path::PathBuf;
use std::sync::Arc;
use store::{FakeSecretStore, KeychainSecretStore, SecretStore, load_profiles, save_profiles};
use workspace::Workspace;

actions!(
    hoard,
    [
        /// Opens the Hoard: auth profiles shared by Raven, the Panda and the picker.
        Toggle
    ]
);

/// How the Hoard reaches the Chrome that Hulder itself launched. Registered
/// by `crates/browser`, so the borrow can only ever talk to that instance.
pub trait ChromeBridge: Send + Sync {
    /// Opens `url` in the hardened profile so the user can log in.
    fn open(&self, url: String, cx: &mut App) -> Task<Result<()>>;
    /// The live browser-level connection to the Chrome we launched, if any.
    fn client(&self, cx: &mut App) -> Option<CdpClient>;
}

struct GlobalChromeBridge(Arc<dyn ChromeBridge>);

impl Global for GlobalChromeBridge {}

pub fn set_chrome_bridge(cx: &mut App, bridge: Arc<dyn ChromeBridge>) {
    cx.set_global(GlobalChromeBridge(bridge));
}

pub fn chrome_bridge(cx: &App) -> Option<Arc<dyn ChromeBridge>> {
    cx.try_global::<GlobalChromeBridge>()
        .map(|bridge| bridge.0.clone())
}

pub struct Hoard {
    profiles: Vec<Profile>,
    path: PathBuf,
    secrets: Arc<dyn SecretStore>,
    pub load_error: Option<String>,
}

struct GlobalHoard(Entity<Hoard>);

impl Global for GlobalHoard {}

/// What a matching profile adds to one request.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub profile: String,
    pub headers: Vec<(String, String)>,
    pub keep_session_cookies: bool,
}

impl Hoard {
    pub fn with_store(path: PathBuf, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            profiles: Vec::new(),
            path,
            secrets,
            load_error: None,
        }
    }

    pub fn fake(path: PathBuf) -> (Self, Arc<FakeSecretStore>) {
        let store = Arc::new(FakeSecretStore::default());
        (Self::with_store(path, store.clone()), store)
    }

    pub fn global(cx: &App) -> Entity<Hoard> {
        cx.global::<GlobalHoard>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Hoard>> {
        cx.try_global::<GlobalHoard>().map(|global| global.0.clone())
    }

    pub fn set_global(hoard: Entity<Hoard>, cx: &mut App) {
        cx.set_global(GlobalHoard(hoard));
    }

    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    pub fn matching_profile(&self, scheme: &str, host: &str) -> Option<&Profile> {
        model::matching_profile(&self.profiles, scheme, host)
    }

    /// The fixed set plus every `header` profile's name plus `extra`
    /// (`raven.redact_headers`), lower-cased.
    pub fn redaction_set(&self, extra: &[String]) -> Vec<String> {
        redaction_set(&self.profiles, extra)
    }

    /// Reads `profiles.json` off the main thread and replaces the list.
    pub fn reload(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { load_profiles(&path) })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok(file) => {
                        this.profiles = file.profiles;
                        this.load_error = None;
                    }
                    Err(error) => {
                        log::error!("hoard: {error:#}");
                        this.load_error = Some(format!("{error:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
    }

    fn save(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let path = self.path.clone();
        let file = model::ProfilesFile {
            profiles: self.profiles.clone(),
        };
        cx.background_spawn(async move { save_profiles(&path, &file) })
    }

    /// Stores the secret first, then the metadata; a profile whose secret
    /// failed to store never appears in the list.
    pub fn add_profile(
        &mut self,
        profile: Profile,
        secret: Secret,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if profile.kind != secret.kind() {
            return Task::ready(Err(anyhow!(
                "profile kind {} does not match its secret",
                profile.kind.label()
            )));
        }
        let name = match model::validate_name(&profile.name) {
            Ok(name) => name,
            Err(error) => return Task::ready(Err(error)),
        };
        let write = self.secrets.write(&name, &secret, cx);
        cx.spawn(async move |this, cx| {
            write.await.context("storing the secret in the Keychain")?;
            let save = this.update(cx, |this, cx| {
                this.profiles.retain(|existing| existing.name != name);
                this.profiles.push(Profile {
                    name: name.clone(),
                    header_name: secret.header_name().map(str::to_string),
                    ..profile
                });
                cx.notify();
                this.save(cx)
            })?;
            save.await
        })
    }

    pub fn delete_profile(&mut self, name: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        let name = name.to_string();
        let delete = self.secrets.delete(&name, cx);
        cx.spawn(async move |this, cx| {
            if let Err(error) = delete.await {
                // A Keychain item that is already gone must not keep the
                // profile listed forever.
                log::warn!("hoard: deleting secret for {name}: {error:#}");
            }
            let save = this.update(cx, |this, cx| {
                this.profiles.retain(|existing| existing.name != name);
                cx.notify();
                this.save(cx)
            })?;
            save.await
        })
    }

    pub fn secret_for(&self, name: &str, cx: &mut App) -> Task<Result<Option<Secret>>> {
        self.secrets.read(name, cx)
    }

    pub fn secret_store(&self) -> Arc<dyn SecretStore> {
        self.secrets.clone()
    }

    /// Replaces the jar of a `cookies` profile, dropping expired cookies.
    pub fn set_cookie_jar(&self, name: &str, jar: Vec<Cookie>, cx: &mut App) -> Task<Result<()>> {
        let jar = model::drop_expired(jar, model::now_unix());
        self.secrets.write(name, &Secret::Cookies { jar }, cx)
    }

    pub fn set_keep_session_cookies(
        &mut self,
        name: &str,
        keep: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.name == name) else {
            return Task::ready(Err(anyhow!("no profile named {name}")));
        };
        profile.keep_session_cookies = keep;
        cx.notify();
        self.save(cx)
    }
}

/// Adds the matching profile's headers to `headers` for `scheme://host/path`.
/// Headers the caller already set win over the profile's.
pub fn apply_to_request(
    host: &str,
    scheme: &str,
    path: &str,
    headers: Vec<(String, String)>,
    cx: &mut App,
) -> Task<Result<(Vec<(String, String)>, Option<Applied>)>> {
    let Some(hoard) = Hoard::try_global(cx) else {
        return Task::ready(Ok((headers, None)));
    };
    let (profile, secrets) = {
        let hoard = hoard.read(cx);
        let Some(profile) = hoard.matching_profile(scheme, host).cloned() else {
            return Task::ready(Ok((headers, None)));
        };
        (profile, hoard.secret_store())
    };
    let read = secrets.read(&profile.name, cx);
    let host = host.to_string();
    let scheme = scheme.to_string();
    let path = path.to_string();
    cx.background_spawn(async move {
        let Some(secret) = read.await? else {
            return Err(anyhow!(
                "Hoard profile {} has no secret in the Keychain",
                profile.name
            ));
        };
        let mut headers = headers;
        let mut added = Vec::new();
        for (name, value) in secret.headers_for(&scheme, &host, &path, model::now_unix()) {
            if headers
                .iter()
                .any(|(existing, _)| existing.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            headers.push((name.clone(), value.clone()));
            added.push((name, value));
        }
        Ok((
            headers,
            Some(Applied {
                profile: profile.name,
                headers: added,
                keep_session_cookies: profile.keep_session_cookies,
            }),
        ))
    })
}

/// The unexpired cookies of the matching `cookies` profile for `scheme://host`,
/// with the profile name, for callers that inject a jar rather than a header.
pub fn cookies_for_host(
    host: &str,
    scheme: &str,
    cx: &mut App,
) -> Task<Result<Option<(String, Vec<Cookie>)>>> {
    let Some(hoard) = Hoard::try_global(cx) else {
        return Task::ready(Ok(None));
    };
    let (name, secrets) = {
        let hoard = hoard.read(cx);
        let Some(profile) = hoard
            .matching_profile(scheme, host)
            .filter(|profile| profile.kind == ProfileKind::Cookies)
            .cloned()
        else {
            return Task::ready(Ok(None));
        };
        (profile.name, hoard.secret_store())
    };
    let read = secrets.read(&name, cx);
    let host = host.to_string();
    let scheme = scheme.to_string();
    cx.background_spawn(async move {
        match read.await? {
            Some(Secret::Cookies { jar }) => {
                let now = model::now_unix();
                // Path is left to the browser receiving the jar; it keeps
                // each cookie's own path.
                let cookies: Vec<Cookie> = model::drop_expired(jar, now)
                    .into_iter()
                    .filter(|cookie| cookie.applies(&scheme, &host, &cookie.path, now))
                    .collect();
                Ok(Some((name, cookies)))
            }
            Some(_) => Err(anyhow!("Hoard profile {name} is not a cookies profile")),
            None => Err(anyhow!("Hoard profile {name} has no secret in the Keychain")),
        }
    })
}

/// Folds a response's `Set-Cookie` headers into the jar of `profile` when
/// that profile asked to keep session cookies. Returns whether it did.
pub fn remember_set_cookies(
    profile: &str,
    host: &str,
    set_cookie_values: Vec<String>,
    cx: &mut App,
) -> Task<Result<bool>> {
    let Some(hoard) = Hoard::try_global(cx) else {
        return Task::ready(Ok(false));
    };
    let keep = hoard
        .read(cx)
        .profile(profile)
        .is_some_and(|profile| profile.kind == ProfileKind::Cookies && profile.keep_session_cookies);
    if !keep || set_cookie_values.is_empty() {
        return Task::ready(Ok(false));
    }
    let secrets = hoard.read(cx).secret_store();
    let read = secrets.read(profile, cx);
    let profile = profile.to_string();
    let host = host.to_string();
    cx.spawn(async move |cx| {
        let Some(Secret::Cookies { mut jar }) = read.await? else {
            return Ok(false);
        };
        model::merge_set_cookies(&mut jar, &host, &set_cookie_values);
        let jar = model::drop_expired(jar, model::now_unix());
        let write = cx.update(|cx| secrets.write(&profile, &Secret::Cookies { jar }, cx));
        write.await?;
        Ok(true)
    })
}

pub fn init(cx: &mut App) {
    let hoard = cx.new(|cx| {
        let mut hoard = Hoard::with_store(
            store::default_profiles_path(),
            Arc::new(KeychainSecretStore),
        );
        hoard.reload(cx).detach();
        hoard
    });
    Hoard::set_global(hoard, cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            modal::HoardModal::toggle(workspace, window, cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn cookies_profile(name: &str, hosts: &[&str], keep: bool) -> Profile {
        Profile {
            name: name.into(),
            kind: ProfileKind::Cookies,
            hosts: hosts.iter().map(|host| host.to_string()).collect(),
            created: "2026-10-13".into(),
            header_name: None,
            keep_session_cookies: keep,
        }
    }

    fn cookie(name: &str, domain: &str) -> Cookie {
        Cookie {
            name: name.into(),
            value: "v".into(),
            domain: domain.into(),
            path: "/".into(),
            secure: true,
            http_only: false,
            expires: None,
        }
    }

    #[gpui::test]
    async fn profiles_round_trip_through_the_fake_store_and_disk(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("hoard").join("profiles.json");
        let (hoard, store) = Hoard::fake(path.clone());
        let hoard = cx.new(|_| hoard);
        cx.update(|cx| Hoard::set_global(hoard.clone(), cx));

        let bearer = Profile {
            name: "work".into(),
            kind: ProfileKind::Bearer,
            hosts: vec!["*.example.com".into()],
            created: "2026-10-13".into(),
            header_name: None,
            keep_session_cookies: false,
        };
        hoard
            .update(cx, |hoard, cx| {
                hoard.add_profile(bearer, Secret::Bearer { token: "t0k".into() }, cx)
            })
            .await
            .expect("add");
        let mismatched = hoard
            .update(cx, |hoard, cx| {
                hoard.add_profile(
                    cookies_profile("bad", &["a.test"], false),
                    Secret::Bearer { token: "x".into() },
                    cx,
                )
            })
            .await;
        assert!(mismatched.is_err());
        assert_eq!(store.names(), vec!["work"]);
        let on_disk = std::fs::read_to_string(&path).expect("profiles.json");
        assert!(on_disk.contains("\"work\""));
        assert!(!on_disk.contains("t0k"), "secret leaked into profiles.json");

        let (headers, applied) = cx
            .update(|cx| {
                apply_to_request(
                    "api.example.com",
                    "https",
                    "/",
                    vec![("Accept".into(), "*/*".into())],
                    cx,
                )
            })
            .await
            .expect("apply");
        assert_eq!(
            headers,
            vec![
                ("Accept".to_string(), "*/*".to_string()),
                ("Authorization".to_string(), "Bearer t0k".to_string())
            ]
        );
        assert_eq!(applied.as_ref().map(|applied| applied.profile.as_str()), Some("work"));

        let (headers, applied) = cx
            .update(|cx| apply_to_request("api.example.com", "http", "/", Vec::new(), cx))
            .await
            .expect("apply over http");
        assert!(headers.is_empty() && applied.is_none(), "no profile over plain http");

        let (headers, _) = cx
            .update(|cx| {
                apply_to_request(
                    "api.example.com",
                    "https",
                    "/",
                    vec![("Authorization".into(), "Bearer mine".into())],
                    cx,
                )
            })
            .await
            .expect("apply");
        assert_eq!(headers.len(), 1, "a caller's own header wins");

        let fresh = cx.new(|_| Hoard::fake(path.clone()).0);
        fresh.update(cx, |hoard, cx| hoard.reload(cx)).await;
        assert_eq!(fresh.read_with(cx, |hoard, _| hoard.profiles().len()), 1);

        hoard
            .update(cx, |hoard, cx| hoard.delete_profile("work", cx))
            .await
            .expect("delete");
        assert!(store.names().is_empty());
        assert!(hoard.read_with(cx, |hoard, _| hoard.profiles().is_empty()));
    }

    #[gpui::test]
    async fn cookies_profiles_feed_hosts_and_learn_from_set_cookie(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().expect("tempdir");
        let (hoard, _store) = Hoard::fake(temp.path().join("profiles.json"));
        let hoard = cx.new(|_| hoard);
        cx.update(|cx| Hoard::set_global(hoard.clone(), cx));
        hoard
            .update(cx, |hoard, cx| {
                hoard.add_profile(
                    cookies_profile("site", &["app.test"], true),
                    Secret::Cookies {
                        jar: vec![cookie("session", "app.test"), cookie("other", "elsewhere.test")],
                    },
                    cx,
                )
            })
            .await
            .expect("add");

        let found = cx
            .update(|cx| cookies_for_host("app.test", "https", cx))
            .await
            .expect("cookies");
        let (name, cookies) = found.expect("profile matched");
        assert_eq!(name, "site");
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "session");
        assert!(
            cx.update(|cx| cookies_for_host("app.test", "http", cx))
                .await
                .expect("ok")
                .is_none()
        );

        let (headers, applied) = cx
            .update(|cx| apply_to_request("app.test", "https", "/", Vec::new(), cx))
            .await
            .expect("apply");
        assert_eq!(headers, vec![("Cookie".to_string(), "session=v".to_string())]);
        assert!(applied.expect("applied").keep_session_cookies);

        let remembered = cx
            .update(|cx| {
                remember_set_cookies(
                    "site",
                    "app.test",
                    vec!["session=renewed; Path=/".to_string()],
                    cx,
                )
            })
            .await
            .expect("remember");
        assert!(remembered);
        let (headers, _) = cx
            .update(|cx| apply_to_request("app.test", "https", "/", Vec::new(), cx))
            .await
            .expect("apply");
        assert_eq!(headers, vec![("Cookie".to_string(), "session=renewed".to_string())]);
    }
}
