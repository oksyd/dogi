use std::sync::{Arc, Mutex};
use std::time::Duration;

use dogi_core::{DogiError, Result};
use dogi_ui::{
    NetworkConnectionTestResult, NetworkProxyDraft, NetworkProxyMode, NetworkProxyPreferences,
    NetworkProxyProtocol,
};
use gio::prelude::ProxyResolverExt;
use keyring::Entry;
use ureq::tls::{RootCerts, TlsConfig};
use ureq::{Proxy, ProxyProtocol};

use crate::config::application::ApplicationConfigStore;

const PROXY_CREDENTIAL_SERVICE: &str = "io.github.oksyd.dogi.network-proxy";
const PROXY_CREDENTIAL_ACCOUNT: &str = "default";
const NETWORK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct NetworkService {
    store: ApplicationConfigStore,
    operation_lock: Arc<Mutex<()>>,
}

impl NetworkService {
    pub(crate) fn new(store: ApplicationConfigStore) -> Self {
        Self {
            store,
            operation_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn load_preferences(&self) -> Result<NetworkProxyPreferences> {
        self.store
            .load_network_proxy()
            .map_err(|error| DogiError::Config(error.to_string()))
    }

    pub(crate) fn default_preferences(&self) -> NetworkProxyPreferences {
        self.store.default_network_proxy()
    }

    pub(crate) fn save(&self, draft: NetworkProxyDraft) -> Result<NetworkProxyPreferences> {
        let _guard = self
            .operation_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = self
            .store
            .load_network_proxy()
            .map_err(|error| DogiError::Config(error.to_string()))?;
        let mut preferences = normalize(draft.preferences)?;
        let credential_change = if preferences.authentication_enabled {
            if draft.password.is_empty() {
                if !preferences.password_saved {
                    return Err(DogiError::InvalidArgument(
                        "enter the proxy password before saving".to_owned(),
                    ));
                }
                CredentialChange::Preserve
            } else {
                preferences.password_saved = true;
                CredentialChange::Set(draft.password)
            }
        } else {
            let remove = preferences.password_saved || original.password_saved;
            preferences.password_saved = false;
            if remove {
                CredentialChange::Remove
            } else {
                CredentialChange::Preserve
            }
        };
        self.store
            .save_network_proxy(&preferences)
            .map_err(|error| DogiError::Config(error.to_string()))?;
        let credential_result = match credential_change {
            CredentialChange::Preserve => Ok(()),
            CredentialChange::Set(password) => credential_entry().and_then(|entry| {
                entry
                    .set_password(&password)
                    .map_err(credential_write_error)
            }),
            CredentialChange::Remove => remove_saved_password(),
        };
        if let Err(error) = credential_result {
            if let Err(rollback_error) = self.store.save_network_proxy(&original) {
                return Err(DogiError::Config(format!(
                    "{error}; application settings could not be restored: {rollback_error}"
                )));
            }
            return Err(error);
        }
        Ok(preferences)
    }

    pub(crate) fn policy(&self) -> Result<NetworkPolicy> {
        let _guard = self
            .operation_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let preferences = self
            .store
            .load_network_proxy()
            .map_err(|error| DogiError::Config(error.to_string()))?;
        policy_from_preferences(preferences, None)
    }

    pub(crate) fn test(&self, draft: NetworkProxyDraft) -> Result<NetworkConnectionTestResult> {
        let _guard = self
            .operation_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let preferences = normalize(draft.preferences)?;
        let password = (!draft.password.is_empty()).then_some(draft.password);
        let policy = policy_from_preferences(preferences, password)?;
        crate::update::github::test_connection(&policy)
            .map(|route| NetworkConnectionTestResult { route })
            .map_err(DogiError::BackendUnavailable)
    }
}

pub(crate) struct NetworkPolicy {
    proxy: ProxyPolicy,
}

enum ProxyPolicy {
    System,
    Direct,
    Manual(Proxy),
}

enum CredentialChange {
    Preserve,
    Set(String),
    Remove,
}

impl NetworkPolicy {
    pub(crate) fn agent_for(&self, url: &str) -> std::result::Result<RoutedAgent, String> {
        let (proxy, route) = match &self.proxy {
            ProxyPolicy::System => resolve_system_proxy(url)?,
            ProxyPolicy::Direct => (None, String::new()),
            ProxyPolicy::Manual(proxy) => (Some(proxy.clone()), proxy_route(proxy)),
        };
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(NETWORK_TIMEOUT))
            .tls_config(tls)
            .proxy(proxy)
            .build();
        Ok(RoutedAgent {
            agent: config.new_agent(),
            route,
        })
    }
}

pub(crate) struct RoutedAgent {
    pub(crate) agent: ureq::Agent,
    pub(crate) route: String,
}

fn normalize(mut preferences: NetworkProxyPreferences) -> Result<NetworkProxyPreferences> {
    preferences.host = preferences.host.trim().to_owned();
    preferences.username = preferences.username.trim().to_owned();
    if preferences.mode != NetworkProxyMode::Manual {
        return Ok(preferences);
    }
    if preferences.host.is_empty() {
        return Err(DogiError::InvalidArgument(
            "enter the proxy host".to_owned(),
        ));
    }
    if preferences.host.contains("://")
        || preferences.host.contains('@')
        || preferences.host.chars().any(char::is_whitespace)
    {
        return Err(DogiError::InvalidArgument(
            "enter only a hostname or IP address for the proxy host".to_owned(),
        ));
    }
    if preferences.port == 0 {
        return Err(DogiError::InvalidArgument(
            "enter a proxy port between 1 and 65535".to_owned(),
        ));
    }
    if preferences.authentication_enabled && preferences.username.is_empty() {
        return Err(DogiError::InvalidArgument(
            "enter the proxy username".to_owned(),
        ));
    }
    Ok(preferences)
}

fn policy_from_preferences(
    preferences: NetworkProxyPreferences,
    password: Option<String>,
) -> Result<NetworkPolicy> {
    let preferences = normalize(preferences)?;
    let proxy = match preferences.mode {
        NetworkProxyMode::System => ProxyPolicy::System,
        NetworkProxyMode::Direct => ProxyPolicy::Direct,
        NetworkProxyMode::Manual => {
            let protocol = match preferences.protocol {
                NetworkProxyProtocol::Http => ProxyProtocol::Http,
                NetworkProxyProtocol::Https => ProxyProtocol::Https,
                NetworkProxyProtocol::Socks5 => ProxyProtocol::Socks5h,
            };
            let mut builder = Proxy::builder(protocol)
                .host(&preferences.host)
                .port(preferences.port);
            if preferences.authentication_enabled {
                let password = match password {
                    Some(password) => password,
                    None => credential_entry()?
                        .get_password()
                        .map_err(credential_read_error)?,
                };
                builder = builder.username(&preferences.username).password(&password);
            }
            ProxyPolicy::Manual(builder.build().map_err(|error| {
                DogiError::InvalidArgument(format!("invalid proxy settings: {error}"))
            })?)
        }
    };
    Ok(NetworkPolicy { proxy })
}

fn resolve_system_proxy(url: &str) -> std::result::Result<(Option<Proxy>, String), String> {
    let resolver = gio::ProxyResolver::default();
    if resolver.is_supported() {
        let candidates = resolver
            .lookup(url, gio::Cancellable::NONE)
            .map_err(|error| format!("could not read the system proxy settings: {error}"))?;
        for candidate in candidates {
            if candidate.eq_ignore_ascii_case("direct://") {
                return Ok((None, String::new()));
            }
            if let Ok(proxy) = Proxy::new(candidate.as_str()) {
                let route = proxy_route(&proxy);
                return Ok((Some(proxy), route));
            }
        }
        return Err("the system proxy uses a protocol Dogi does not support".to_owned());
    }
    let proxy = Proxy::try_from_env();
    let route = proxy.as_ref().map(proxy_route).unwrap_or_default();
    Ok((proxy, route))
}

fn proxy_route(proxy: &Proxy) -> String {
    let protocol = match proxy.protocol() {
        ProxyProtocol::Http => "HTTP",
        ProxyProtocol::Https => "HTTPS",
        ProxyProtocol::Socks4 => "SOCKS4",
        ProxyProtocol::Socks4A => "SOCKS4A",
        ProxyProtocol::Socks5 => "SOCKS5",
        ProxyProtocol::Socks5h => "SOCKS5",
        _ => "PROXY",
    };
    format!("{protocol} · {}:{}", proxy.host(), proxy.port())
}

fn credential_entry() -> Result<Entry> {
    Entry::new(PROXY_CREDENTIAL_SERVICE, PROXY_CREDENTIAL_ACCOUNT).map_err(|error| {
        DogiError::BackendUnavailable(format!(
            "the desktop credential store is unavailable: {error}"
        ))
    })
}

fn credential_read_error(error: keyring::Error) -> DogiError {
    match error {
        keyring::Error::NoEntry => {
            DogiError::Config("the saved proxy password is missing; enter it again".to_owned())
        }
        error => DogiError::BackendUnavailable(format!(
            "could not read the proxy password from the desktop credential store: {error}"
        )),
    }
}

fn credential_write_error(error: keyring::Error) -> DogiError {
    DogiError::BackendUnavailable(format!(
        "could not save the proxy password in the desktop credential store: {error}"
    ))
}

fn remove_saved_password() -> Result<()> {
    let entry = credential_entry()?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(DogiError::BackendUnavailable(format!(
            "could not remove the proxy password from the desktop credential store: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn manual_proxy_requires_a_bare_host() {
        let preferences = NetworkProxyPreferences {
            mode: NetworkProxyMode::Manual,
            host: "https://proxy.example.com".to_owned(),
            ..NetworkProxyPreferences::default()
        };
        assert!(normalize(preferences).is_err());
    }

    #[test]
    fn manual_proxy_normalization_preserves_supported_protocols() {
        let preferences = NetworkProxyPreferences {
            mode: NetworkProxyMode::Manual,
            protocol: NetworkProxyProtocol::Socks5,
            host: " 127.0.0.1 ".to_owned(),
            port: 7890,
            ..NetworkProxyPreferences::default()
        };
        let normalized = normalize(preferences).unwrap();
        assert_eq!(normalized.host, "127.0.0.1");
        assert_eq!(normalized.protocol, NetworkProxyProtocol::Socks5);
    }

    #[test]
    fn direct_policy_explicitly_disables_proxying() {
        let preferences = NetworkProxyPreferences {
            mode: NetworkProxyMode::Direct,
            ..NetworkProxyPreferences::default()
        };
        let policy = policy_from_preferences(preferences, None).unwrap();
        let routed = policy.agent_for("https://api.github.com").unwrap();

        assert!(routed.agent.config().proxy().is_none());
        assert!(routed.route.is_empty());
    }

    #[test]
    fn manual_policy_builds_a_redacted_route_description() {
        let preferences = NetworkProxyPreferences {
            mode: NetworkProxyMode::Manual,
            protocol: NetworkProxyProtocol::Http,
            host: "proxy.example.com".to_owned(),
            port: 8080,
            ..NetworkProxyPreferences::default()
        };
        let policy = policy_from_preferences(preferences, None).unwrap();
        let routed = policy.agent_for("https://api.github.com").unwrap();

        assert_eq!(routed.route, "HTTP · proxy.example.com:8080");
        assert_eq!(
            routed.agent.config().proxy().unwrap().host(),
            "proxy.example.com"
        );
    }

    #[test]
    fn unauthenticated_proxy_can_be_saved_without_a_credential_store() {
        let root = unique_test_root("save-without-credentials");
        let store = ApplicationConfigStore::at(root.join("config.json"));
        let service = NetworkService::new(store);
        let preferences = NetworkProxyPreferences {
            mode: NetworkProxyMode::Manual,
            host: "proxy.example.com".to_owned(),
            port: 8080,
            ..NetworkProxyPreferences::default()
        };

        let saved = service
            .save(NetworkProxyDraft::from_preferences(preferences.clone()))
            .unwrap();

        assert_eq!(saved, preferences);
        assert_eq!(service.load_preferences().unwrap(), preferences);
        let _ = std::fs::remove_dir_all(root);
    }

    fn unique_test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dogi-network-{label}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ))
    }
}
