//! HTTP clients for untrusted video hosts. Validate both literal addresses and
//! DNS answers, including redirects, before opening an upstream connection.
use anyhow::{Result, bail};
use reqwest::{
    Method, RequestBuilder, Url,
    dns::{Addrs, Name, Resolve, Resolving},
};
use std::{
    io,
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::Duration,
};

#[derive(Clone)]
pub struct Client {
    inner: reqwest::Client,
    allow_loopback: bool,
}

impl Client {
    pub fn new() -> Result<Self> {
        Self::with_cookies(Arc::new(reqwest::cookie::Jar::default()))
    }

    pub fn with_cookies(jar: Arc<reqwest::cookie::Jar>) -> Result<Self> {
        Self::build(false, jar)
    }

    fn build(allow_loopback: bool, jar: Arc<reqwest::cookie::Jar>) -> Result<Self> {
        let inner = reqwest::Client::builder()
            .user_agent(crate::api::USER_AGENT)
            .cookie_provider(jar)
            // A proxy could resolve names itself and bypass our address checks.
            .no_proxy()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .dns_resolver(Arc::new(PublicDns { allow_loopback }))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("Too many redirects");
                }
                match validate_url(attempt.url(), allow_loopback) {
                    Ok(()) => attempt.follow(),
                    Err(_) => attempt.error("Blocked unsafe redirect destination"),
                }
            }))
            .build()?;
        Ok(Self {
            inner,
            allow_loopback,
        })
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::build(true, Arc::new(reqwest::cookie::Jar::default())).unwrap()
    }

    pub fn get(&self, url: impl AsRef<str>) -> Result<RequestBuilder> {
        self.request(Method::GET, url)
    }

    pub fn request(&self, method: Method, url: impl AsRef<str>) -> Result<RequestBuilder> {
        let url = Url::parse(url.as_ref())?;
        self.validate(&url)?;
        Ok(self.inner.request(method, url))
    }

    pub fn validate(&self, url: &Url) -> Result<()> {
        validate_url(url, self.allow_loopback)
    }
}

/// Peek without assuming a host supplies a correct Content-Type. The caller
/// must replay these bytes when forwarding/saving a non-playlist response.
pub async fn peek(response: &mut reqwest::Response) -> Result<Vec<u8>> {
    let mut prefix = Vec::new();
    while prefix.len() < 10 {
        match response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            Some(chunk) => prefix.extend_from_slice(&chunk),
            None => break,
        }
    }
    Ok(prefix)
}

pub fn hls_prefix(prefix: &[u8]) -> bool {
    prefix
        .strip_prefix(&[0xef, 0xbb, 0xbf])
        .unwrap_or(prefix)
        .starts_with(b"#EXTM3U")
}

fn validate_url(url: &Url, allow_loopback: bool) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("Only HTTP(S) URLs without credentials are allowed");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Missing upstream host"))?;
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>()
        && !allowed_ip(ip, allow_loopback)
    {
        bail!("Blocked non-public upstream address");
    }
    Ok(())
}

fn allowed_ip(ip: IpAddr, allow_loopback: bool) -> bool {
    if allow_loopback && ip.is_loopback() {
        return true;
    }
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let s = ip.segments();
            // Routable unicast only; exclude protocol assignments, documentation
            // and 6to4 (which can embed a private IPv4 destination).
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
                && s[0] != 0x3fff
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && a != 0
        && a < 224
        && !(a == 100 && (64..=127).contains(&b))
        && !(a == 192 && b == 0 && (c == 0 || c == 2))
        && !(a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        && !(a == 203 && b == 0 && c == 113)
}

struct PublicDns {
    allow_loopback: bool,
}

impl Resolve for PublicDns {
    fn resolve(&self, name: Name) -> Resolving {
        let name = name.as_str().to_owned();
        let allow_loopback = self.allow_loopback;
        Box::pin(async move {
            let addresses: Vec<_> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if addresses.is_empty()
                || addresses
                    .iter()
                    .any(|a| !allowed_ip(a.ip(), allow_loopback))
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Blocked non-public DNS destination",
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_special_addresses_and_credentials() {
        for host in [
            "127.0.0.1",
            "2130706433",
            "0x7f000001",
            "10.1.2.3",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "[::1]",
            "[fc00::1]",
            "[::ffff:127.0.0.1]",
            "[2002:7f00:1::1]",
        ] {
            assert!(
                Client::new()
                    .unwrap()
                    .get(format!("http://{host}/"))
                    .is_err(),
                "accepted {host}"
            );
        }
        for url in ["file:///etc/passwd", "https://user:password@example.com/"] {
            assert!(Client::new().unwrap().get(url).is_err());
        }
        assert!(Client::new().unwrap().get("https://example.com/").is_ok());
        assert!(allowed_ip("8.8.8.8".parse().unwrap(), false));
        assert!(allowed_ip("2606:4700:4700::1111".parse().unwrap(), false));
    }

    #[tokio::test]
    async fn redirects_cannot_reach_private_addresses() {
        use axum::{Router, response::Redirect, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/redirect", listener.local_addr().unwrap());
        let upstream = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/redirect",
                    get(|| async { Redirect::temporary("http://10.0.0.1/private") }),
                ),
            )
            .into_future(),
        );
        // Allow loopback only for this fixture; private LAN destinations remain blocked.
        let error = Client::for_test()
            .get(url)
            .unwrap()
            .send()
            .await
            .unwrap_err();
        assert!(error.is_redirect());
        upstream.abort();
    }

    #[tokio::test]
    async fn rejects_private_dns_answers() {
        assert!(
            PublicDns {
                allow_loopback: false
            }
            .resolve("localhost".parse().unwrap())
            .await
            .is_err()
        );
    }
}
