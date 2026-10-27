#[cfg(windows)]
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use std::{env, net::IpAddr};

use reqwest::{Proxy, Url};

/// Resolves the proxy used by every request in one network session.
///
/// An explicit command-line or GUI proxy wins. Without one, standard proxy
/// environment variables win. Windows falls back to the current user's
/// WinHTTP configuration, including static settings and PAC/WPAD evaluation.
pub(crate) struct ProxyResolver {
    selection: ProxySelection,
}

#[derive(Clone)]
enum ProxySelection {
    Direct,
    Fixed(ProxyRules),
    #[cfg(windows)]
    Windows(Arc<WindowsProxyResolver>),
}

#[derive(Clone, Default)]
struct ProxyRules {
    http: Option<String>,
    https: Option<String>,
    all: Option<String>,
    bypass: Vec<BypassRule>,
}

#[derive(Clone)]
enum BypassRule {
    All,
    Local,
    Host(String),
    HostPort { host: String, port: u16 },
    Cidr { network: IpAddr, prefix: u8 },
}

impl ProxyResolver {
    pub(crate) fn new(explicit: Option<&str>) -> Result<Self, ProxyResolutionError> {
        let selection = if let Some(proxy) =
            explicit.map(str::trim).filter(|value| !value.is_empty())
        {
            ProxySelection::Fixed(ProxyRules::single(proxy)?)
        } else if let Some(rules) = ProxyRules::from_environment()? {
            ProxySelection::Fixed(rules)
        } else {
            #[cfg(windows)]
            {
                match WindowsProxyResolver::new() {
                    Ok(Some(resolver)) => ProxySelection::Windows(Arc::new(resolver)),
                    Ok(None) => ProxySelection::Direct,
                    Err(error) => {
                        eprintln!(
                            "[network] system proxy discovery failed ({error}); using direct connections"
                        );
                        ProxySelection::Direct
                    }
                }
            }
            #[cfg(not(windows))]
            {
                ProxySelection::Direct
            }
        };
        Ok(Self { selection })
    }

    pub(crate) fn into_reqwest_proxy(self) -> Option<Proxy> {
        match self.selection {
            ProxySelection::Direct => None,
            ProxySelection::Fixed(rules) => Some(Proxy::custom(move |url| rules.for_url(url))),
            #[cfg(windows)]
            ProxySelection::Windows(resolver) => {
                Some(Proxy::custom(move |url| resolver.for_url(url)))
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProxyResolutionError {
    #[error("invalid proxy URL")]
    InvalidProxyUrl,
    #[error("proxy URL must use http://, https://, socks4://, or socks5://")]
    UnsupportedScheme,
}

impl ProxyRules {
    fn single(value: &str) -> Result<Self, ProxyResolutionError> {
        Ok(Self {
            all: Some(normalize_proxy_url(value)?),
            ..Self::default()
        })
    }

    fn from_environment() -> Result<Option<Self>, ProxyResolutionError> {
        let entries = env::vars().collect::<Vec<_>>();
        Self::from_environment_entries(
            entries
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
    }

    fn from_environment_entries<'a>(
        entries: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Option<Self>, ProxyResolutionError> {
        let entries = entries.into_iter().collect::<Vec<_>>();
        let http = first_entry(&entries, "HTTP_PROXY");
        let https = first_entry(&entries, "HTTPS_PROXY");
        let all = first_entry(&entries, "ALL_PROXY");
        if http.is_none() && https.is_none() && all.is_none() {
            return Ok(None);
        }
        Ok(Some(Self {
            http: http.as_deref().map(normalize_proxy_url).transpose()?,
            https: https.as_deref().map(normalize_proxy_url).transpose()?,
            all: all.as_deref().map(normalize_proxy_url).transpose()?,
            bypass: first_entry(&entries, "NO_PROXY")
                .as_deref()
                .map(parse_bypass_rules)
                .unwrap_or_default(),
        }))
    }

    fn for_url(&self, url: &Url) -> Option<String> {
        if self.bypass.iter().any(|rule| rule.matches(url)) {
            return None;
        }
        match url.scheme() {
            "http" => self.http.as_ref().or(self.all.as_ref()).cloned(),
            "https" => self.https.as_ref().or(self.all.as_ref()).cloned(),
            _ => None,
        }
    }
}

impl BypassRule {
    fn matches(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let normalized = normalize_host(host);
        match self {
            Self::All => true,
            Self::Local => {
                (normalized == "localhost" || !normalized.contains('.'))
                    && normalized.parse::<IpAddr>().is_err()
            }
            Self::Host(value) => host_rule_matches(value, &normalized),
            Self::HostPort { host, port } => {
                host_rule_matches(host, &normalized) && url.port_or_known_default() == Some(*port)
            }
            Self::Cidr { network, prefix } => normalized
                .parse::<IpAddr>()
                .is_ok_and(|address| ip_in_cidr(address, *network, *prefix)),
        }
    }
}

fn host_rule_matches(rule: &str, host: &str) -> bool {
    if rule.contains('*') {
        return wildcard_matches(rule, host);
    }
    host == rule || host.ends_with(&format!(".{rule}"))
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_index, mut value_index) = (0_usize, 0_usize);
    let (mut star_index, mut resume_value_index) = (None, 0_usize);

    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            resume_value_index = value_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            resume_value_index += 1;
            value_index = resume_value_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn first_entry(entries: &[(&str, &str)], canonical_name: &str) -> Option<String> {
    let lowercase_name = canonical_name.to_ascii_lowercase();
    if let Some((_, value)) = entries
        .iter()
        .find(|(key, value)| *key == canonical_name && !value.trim().is_empty())
    {
        return Some((*value).to_string());
    }
    if let Some((_, value)) = entries
        .iter()
        .find(|(key, value)| *key == lowercase_name && !value.trim().is_empty())
    {
        return Some((*value).to_string());
    }
    entries
        .iter()
        .find(|(key, value)| key.eq_ignore_ascii_case(canonical_name) && !value.trim().is_empty())
        .map(|(_, value)| (*value).to_string())
}

#[cfg(any(windows, test))]
fn parse_system_proxy_rules(
    proxy: Option<&str>,
    bypass: Option<&str>,
) -> Result<ProxyRules, String> {
    let mut rules = ProxyRules {
        bypass: bypass.map(parse_bypass_rules).unwrap_or_default(),
        ..ProxyRules::default()
    };
    let Some(proxy) = proxy.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(rules);
    };

    for part in proxy
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if part.eq_ignore_ascii_case("DIRECT") {
            continue;
        }
        if let Some((kind, address)) = part.split_once('=') {
            let kind = kind.trim().to_ascii_lowercase();
            let address = address.trim();
            let proxy_scheme = match kind.as_str() {
                "http" | "https" | "proxy" => "http",
                "socks" | "socks5" => "socks5",
                "socks4" => "socks4",
                _ => continue,
            };
            let address = if address.contains("://") {
                address.to_string()
            } else {
                format!("{proxy_scheme}://{address}")
            };
            let normalized = normalize_proxy_url(&address)
                .map_err(|_| "invalid system proxy URL".to_string())?;
            match kind.as_str() {
                "http" => rules.http = Some(normalized),
                "https" => rules.https = Some(normalized),
                "proxy" | "socks" | "socks4" | "socks5" => rules.all = Some(normalized),
                _ => continue,
            }
            continue;
        }
        rules.all =
            Some(normalize_proxy_url(part).map_err(|_| "invalid system proxy URL".to_string())?);
    }
    Ok(rules)
}

/// Parses the proxy directive list returned by WinHTTP PAC/WPAD evaluation.
///
/// PAC uses an ordered fallback list. The first supported, syntactically valid
/// directive wins; `DIRECT` is an explicit direct-connection result. Invalid or
/// unsupported entries are ignored so a later valid fallback can be used.
#[cfg(any(windows, test))]
fn parse_pac_proxy_result(value: &str) -> Result<Option<String>, &'static str> {
    for entry in value
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        if entry.eq_ignore_ascii_case("DIRECT") {
            return Ok(None);
        }

        let (kind, address) = entry
            .split_once(char::is_whitespace)
            .map(|(kind, address)| (kind, address.trim()))
            .unwrap_or(("PROXY", entry));
        let scheme = match kind.to_ascii_lowercase().as_str() {
            "proxy" | "http" => "http",
            "https" => "https",
            "socks" | "socks5" => "socks5",
            "socks4" => "socks4",
            _ => continue,
        };
        if address.is_empty() {
            continue;
        }
        let address = if address.contains("://") {
            address.to_string()
        } else {
            format!("{scheme}://{address}")
        };
        if let Ok(proxy) = normalize_proxy_url(&address) {
            return Ok(Some(proxy));
        }
    }
    Err("invalid automatic proxy result")
}

fn normalize_proxy_url(value: &str) -> Result<String, ProxyResolutionError> {
    let value = value.trim();
    let value = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let parsed = Url::parse(&value).map_err(|_| ProxyResolutionError::InvalidProxyUrl)?;
    if !matches!(parsed.scheme(), "http" | "https" | "socks4" | "socks5")
        || parsed.host_str().is_none()
        || (!parsed.path().is_empty() && parsed.path() != "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ProxyResolutionError::UnsupportedScheme);
    }
    Ok(value)
}

fn parse_bypass_rules(value: &str) -> Vec<BypassRule> {
    value
        .split([',', ';'])
        .filter_map(|item| {
            let item = item.trim();
            if item.is_empty() {
                return None;
            }
            if item == "*" {
                return Some(BypassRule::All);
            }
            if item.eq_ignore_ascii_case("<local>") {
                return Some(BypassRule::Local);
            }
            if let Some((address, prefix)) = item.split_once('/') {
                let prefix = prefix.parse::<u8>().ok()?;
                let network = address.trim().parse::<IpAddr>().ok()?;
                let maximum_prefix = match network {
                    IpAddr::V4(_) => 32,
                    IpAddr::V6(_) => 128,
                };
                return (prefix <= maximum_prefix).then_some(BypassRule::Cidr { network, prefix });
            }
            if let Some((host, port)) = parse_bypass_host_port(item) {
                return Some(BypassRule::HostPort { host, port });
            }
            let host = normalize_host(item.trim_start_matches('.'));
            (!host.is_empty()).then_some(BypassRule::Host(host))
        })
        .collect()
}

fn parse_bypass_host_port(value: &str) -> Option<(String, u16)> {
    if let Some(value) = value.strip_prefix('[') {
        let (host, port) = value.split_once("]:")?;
        return Some((normalize_host(host), port.parse().ok()?));
    }
    let (host, port) = value.rsplit_once(':')?;
    if host.contains(':') {
        return None;
    }
    Some((normalize_host(host), port.parse().ok()?))
}

fn normalize_host(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn ip_in_cidr(address: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (address, network) {
        (IpAddr::V4(address), IpAddr::V4(network)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            u32::from(address) & mask == u32::from(network) & mask
        }
        (IpAddr::V6(address), IpAddr::V6(network)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            u128::from(address) & mask == u128::from(network) & mask
        }
        _ => false,
    }
}

#[cfg(windows)]
struct WindowsProxyResolver {
    static_rules: ProxyRules,
    auto: WindowsAutoProxy,
    cache: WindowsProxyCache,
}

#[cfg(windows)]
const SYSTEM_PROXY_CACHE_LIMIT: usize = 256;

#[cfg(windows)]
const SYSTEM_PROXY_FAILURE_CACHE_TTL: Duration = Duration::from_secs(30);

#[cfg(windows)]
const MAX_WINDOWS_PROXY_TEXT_CHARS: usize = 32 * 1024;

#[cfg(windows)]
const MAX_WINDOWS_PROXY_CACHE_KEY_BYTES: usize = 8 * 1024;

#[cfg(windows)]
struct WindowsProxyCache {
    state: Mutex<WindowsProxyCacheState>,
    changed: Condvar,
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsProxyCacheState {
    entries: HashMap<String, CachedProxy>,
    order: VecDeque<String>,
    in_flight: HashSet<String>,
}

#[cfg(windows)]
#[derive(Clone)]
struct CachedProxy {
    value: Option<String>,
    expires_at: Option<Instant>,
}

#[cfg(windows)]
impl Default for WindowsProxyCache {
    fn default() -> Self {
        Self {
            state: Mutex::new(WindowsProxyCacheState::default()),
            changed: Condvar::new(),
        }
    }
}

#[cfg(windows)]
struct WindowsAutoProxy {
    auto_detect: bool,
    pac_url: Option<Vec<u16>>,
}

#[cfg(windows)]
impl WindowsProxyResolver {
    fn new() -> Result<Option<Self>, String> {
        use windows_sys::Win32::Foundation::GlobalFree;
        use windows_sys::Win32::Networking::WinHttp::{
            WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WinHttpGetIEProxyConfigForCurrentUser,
        };
        let mut config = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        if unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut config) } == 0 {
            return Err("WinHttpGetIEProxyConfigForCurrentUser failed".to_string());
        }
        let auto_detect = config.fAutoDetect != 0;
        let pac_url = unsafe { take_wide(config.lpszAutoConfigUrl) };
        let static_proxy = unsafe { take_wide(config.lpszProxy) };
        let bypass = unsafe { take_wide(config.lpszProxyBypass) };
        unsafe {
            if !config.lpszAutoConfigUrl.is_null() {
                let _ = GlobalFree(config.lpszAutoConfigUrl.cast());
            }
            if !config.lpszProxy.is_null() {
                let _ = GlobalFree(config.lpszProxy.cast());
            }
            if !config.lpszProxyBypass.is_null() {
                let _ = GlobalFree(config.lpszProxyBypass.cast());
            }
        }
        let pac_url = pac_url?.as_deref().map(wide);
        let static_proxy = static_proxy?;
        let bypass = bypass?;
        let static_rules = parse_system_proxy_rules(static_proxy.as_deref(), bypass.as_deref())?;
        if !auto_detect
            && pac_url.is_none()
            && static_rules.http.is_none()
            && static_rules.https.is_none()
            && static_rules.all.is_none()
        {
            return Ok(None);
        }
        Ok(Some(Self {
            static_rules,
            auto: WindowsAutoProxy {
                auto_detect,
                pac_url,
            },
            cache: WindowsProxyCache::default(),
        }))
    }

    fn for_url(&self, url: &Url) -> Option<String> {
        if !self.auto.auto_detect && self.auto.pac_url.is_none() {
            return self.static_rules.for_url(url);
        }
        let Some(key) = automatic_proxy_cache_key(url) else {
            return self.resolve_automatic_proxy(url).value;
        };

        loop {
            let mut cache = self.lock_cache();
            if let Some(entry) = cache.entries.get(&key).cloned() {
                if entry
                    .expires_at
                    .is_none_or(|deadline| deadline > Instant::now())
                {
                    return entry.value;
                }
                remove_cached_proxy(&mut cache, &key);
            }
            if cache.in_flight.insert(key.clone()) {
                break;
            }
            cache = match self.cache.changed.wait(cache) {
                Ok(cache) => cache,
                Err(poisoned) => {
                    eprintln!("[network] system proxy cache lock was poisoned; recovering it");
                    poisoned.into_inner()
                }
            };
            drop(cache);
        }

        let resolved = self.resolve_automatic_proxy(url);
        let value = resolved.value.clone();
        let mut cache = self.lock_cache();
        insert_cached_proxy(&mut cache, key.clone(), resolved);
        cache.in_flight.remove(&key);
        self.cache.changed.notify_all();
        value
    }

    fn resolve_automatic_proxy(&self, url: &Url) -> CachedProxy {
        match resolve_winhttp_auto_proxy(url, &self.auto) {
            Ok(value) => CachedProxy {
                value,
                expires_at: None,
            },
            Err(error) => {
                eprintln!(
                    "[network] system automatic proxy lookup failed for {} ({error}); using static/direct settings",
                    redact_url(url),
                );
                CachedProxy {
                    value: self.static_rules.for_url(url),
                    expires_at: Some(Instant::now() + SYSTEM_PROXY_FAILURE_CACHE_TTL),
                }
            }
        }
    }

    fn lock_cache(&self) -> MutexGuard<'_, WindowsProxyCacheState> {
        match self.cache.state.lock() {
            Ok(cache) => cache,
            Err(poisoned) => {
                eprintln!("[network] system proxy cache lock was poisoned; recovering it");
                poisoned.into_inner()
            }
        }
    }
}

#[cfg(windows)]
fn automatic_proxy_cache_key(url: &Url) -> Option<String> {
    // PAC scripts may use the complete URL, not merely the origin. Preserve that
    // behavior while bounding both cache-key size and total cache entries.
    (url.as_str().len() <= MAX_WINDOWS_PROXY_CACHE_KEY_BYTES).then(|| url.as_str().to_string())
}

#[cfg(windows)]
fn remove_cached_proxy(cache: &mut WindowsProxyCacheState, key: &str) {
    cache.entries.remove(key);
    cache.order.retain(|entry| entry != key);
}

#[cfg(windows)]
fn insert_cached_proxy(cache: &mut WindowsProxyCacheState, key: String, value: CachedProxy) {
    remove_cached_proxy(cache, &key);
    while cache.entries.len() >= SYSTEM_PROXY_CACHE_LIMIT {
        let Some(oldest) = cache.order.pop_front() else {
            cache.entries.clear();
            break;
        };
        cache.entries.remove(&oldest);
    }
    cache.order.push_back(key.clone());
    cache.entries.insert(key, value);
}

#[cfg(windows)]
fn resolve_winhttp_auto_proxy(
    url: &Url,
    auto: &WindowsAutoProxy,
) -> Result<Option<String>, String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTO_DETECT_TYPE_DHCP,
        WINHTTP_AUTO_DETECT_TYPE_DNS_A, WINHTTP_AUTOPROXY_AUTO_DETECT,
        WINHTTP_AUTOPROXY_CONFIG_URL, WINHTTP_AUTOPROXY_OPTIONS, WINHTTP_PROXY_INFO,
        WinHttpCloseHandle, WinHttpGetProxyForUrl, WinHttpOpen, WinHttpSetTimeouts,
    };
    if !auto.auto_detect && auto.pac_url.is_none() {
        return Err("automatic proxy is not configured".to_string());
    }
    let agent = wide("mcmpu");
    let session = unsafe {
        WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if session.is_null() {
        return Err("WinHttpOpen failed".to_string());
    }
    if unsafe { WinHttpSetTimeouts(session, 5_000, 5_000, 5_000, 5_000) } == 0 {
        unsafe {
            let _ = WinHttpCloseHandle(session);
        }
        return Err("WinHttpSetTimeouts failed".to_string());
    }
    let url_wide = wide(url.as_str());
    let mut options = WINHTTP_AUTOPROXY_OPTIONS::default();
    if auto.auto_detect {
        options.dwFlags |= WINHTTP_AUTOPROXY_AUTO_DETECT;
        options.dwAutoDetectFlags = WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A;
    }
    if let Some(pac_url) = &auto.pac_url {
        options.dwFlags |= WINHTTP_AUTOPROXY_CONFIG_URL;
        options.lpszAutoConfigUrl = pac_url.as_ptr();
    }
    let mut proxy = WINHTTP_PROXY_INFO::default();
    let success =
        unsafe { WinHttpGetProxyForUrl(session, url_wide.as_ptr(), &mut options, &mut proxy) } != 0;
    unsafe {
        let _ = WinHttpCloseHandle(session);
    }
    if !success {
        return Err("WinHttpGetProxyForUrl failed".to_string());
    }
    let proxy_value = unsafe { take_wide(proxy.lpszProxy) };
    unsafe {
        if !proxy.lpszProxy.is_null() {
            let _ = GlobalFree(proxy.lpszProxy.cast());
        }
        if !proxy.lpszProxyBypass.is_null() {
            let _ = GlobalFree(proxy.lpszProxyBypass.cast());
        }
    }
    let Some(proxy_value) = proxy_value? else {
        return Ok(None);
    };
    parse_pac_proxy_result(&proxy_value).map_err(str::to_string)
}

#[cfg(windows)]
unsafe fn take_wide(pointer: *mut u16) -> Result<Option<String>, String> {
    if pointer.is_null() {
        return Ok(None);
    }
    let mut length = 0usize;
    while length < MAX_WINDOWS_PROXY_TEXT_CHARS && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    if length == MAX_WINDOWS_PROXY_TEXT_CHARS {
        return Err("Windows proxy configuration text exceeded its size limit".to_string());
    }
    Ok(Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(pointer, length)
    })))
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn redact_url(url: &Url) -> String {
    format!("{}://{}", url.scheme(), url.host_str().unwrap_or("unknown"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_proxy_wins_over_environment() {
        let rules = ProxyRules::single("http://explicit.example:8080").unwrap();
        assert_eq!(
            rules
                .for_url(&Url::parse("https://example.invalid/a").unwrap())
                .as_deref(),
            Some("http://explicit.example:8080")
        );
    }

    #[test]
    fn environment_rules_choose_protocol_and_bypass_hosts() {
        let rules = ProxyRules {
            http: Some("http://http-proxy.example:8080".to_string()),
            https: Some("http://https-proxy.example:8443".to_string()),
            all: None,
            bypass: parse_bypass_rules("localhost,.internal.example,10.0.0.0/8"),
        };
        assert_eq!(
            rules
                .for_url(&Url::parse("http://public.example/a").unwrap())
                .as_deref(),
            Some("http://http-proxy.example:8080")
        );
        assert_eq!(
            rules
                .for_url(&Url::parse("https://public.example/a").unwrap())
                .as_deref(),
            Some("http://https-proxy.example:8443")
        );
        assert_eq!(
            rules.for_url(&Url::parse("https://api.internal.example/a").unwrap()),
            None
        );
        assert_eq!(
            rules.for_url(&Url::parse("https://10.4.5.6/a").unwrap()),
            None
        );
    }

    #[test]
    fn environment_variables_are_case_aware_and_all_proxy_is_fallback() {
        let rules = ProxyRules::from_environment_entries([
            ("HTTP_PROXY", "http://upper.example:8080"),
            ("http_proxy", "http://lower.example:8080"),
            ("ALL_PROXY", "http://all.example:8080"),
            ("NO_PROXY", "localhost"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            rules
                .for_url(&Url::parse("http://example.com").unwrap())
                .as_deref(),
            Some("http://upper.example:8080")
        );
        assert_eq!(
            rules
                .for_url(&Url::parse("https://example.com").unwrap())
                .as_deref(),
            Some("http://all.example:8080")
        );
        assert_eq!(
            rules.for_url(&Url::parse("http://localhost").unwrap()),
            None
        );
    }

    #[test]
    fn no_proxy_star_bypasses_every_host() {
        let rules = ProxyRules {
            all: Some("http://proxy.example:8080".to_string()),
            bypass: parse_bypass_rules("*"),
            ..ProxyRules::default()
        };
        assert_eq!(
            rules.for_url(&Url::parse("https://example.com/a").unwrap()),
            None
        );
    }

    #[test]
    fn no_system_proxy_resolves_to_direct() {
        let rules = parse_system_proxy_rules(None, None).unwrap();
        assert_eq!(
            rules.for_url(&Url::parse("https://example.com/a").unwrap()),
            None
        );
    }

    #[test]
    fn static_windows_rules_select_protocol_and_bypass() {
        let rules = parse_system_proxy_rules(
            Some("http=127.0.0.1:7890;https=127.0.0.1:7897"),
            Some("<local>;*.corp.example"),
        )
        .unwrap();
        assert_eq!(
            rules
                .for_url(&Url::parse("http://example.com/a").unwrap())
                .as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            rules
                .for_url(&Url::parse("https://example.com/a").unwrap())
                .as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            rules.for_url(&Url::parse("https://host.corp.example/a").unwrap()),
            None
        );
        assert_eq!(
            rules.for_url(&Url::parse("https://intranet/a").unwrap()),
            None
        );
    }

    #[test]
    fn bypass_rules_support_windows_ipv4_and_domain_wildcards() {
        let rules = ProxyRules {
            all: Some("http://proxy.example:8080".to_string()),
            bypass: parse_bypass_rules("127.*;*.internal.example"),
            ..ProxyRules::default()
        };
        assert_eq!(
            rules.for_url(&Url::parse("https://127.0.0.1/a").unwrap()),
            None
        );
        assert_eq!(
            rules.for_url(&Url::parse("https://api.internal.example/a").unwrap()),
            None
        );
        assert_eq!(
            rules
                .for_url(&Url::parse("https://internal.example/a").unwrap())
                .as_deref(),
            Some("http://proxy.example:8080")
        );
    }

    #[test]
    fn pac_results_use_supported_proxy_before_direct_fallback() {
        assert_eq!(
            parse_pac_proxy_result("SOCKS4A unsupported:1080; PROXY 127.0.0.1:7897; DIRECT")
                .unwrap()
                .as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            parse_pac_proxy_result("DIRECT; PROXY ignored:8080").unwrap(),
            None
        );
        assert!(parse_pac_proxy_result("SOCKS4A unsupported:1080").is_err());
    }

    #[test]
    fn unsupported_system_proxy_protocol_does_not_break_http_rules() {
        let rules = parse_system_proxy_rules(
            Some("ftp=invalid value;http=127.0.0.1:7890;https=127.0.0.1:7897"),
            None,
        )
        .unwrap();
        assert_eq!(
            rules
                .for_url(&Url::parse("http://example.com/a").unwrap())
                .as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            rules
                .for_url(&Url::parse("https://example.com/a").unwrap())
                .as_deref(),
            Some("http://127.0.0.1:7897")
        );
    }
}
