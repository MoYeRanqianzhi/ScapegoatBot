//! 网络工具的共享策略：SSRF 防护、域名允许/阻止、大小限制。
//!
//! 设计依据：设计 §5.2 `Download` / `WebFetch`。
//!
//! 防护顺序：
//! 1. 协议只允许 http/https；
//! 2. 域名允许/阻止列表（阻止优先）；
//! 3. 解析出的每个地址都必须是公网地址（拒绝 loopback/私网/链路本地/未指定）；
//! 4. 重定向最多 3 次，每次重新检查地址与域名；
//! 5. 读取上限由调用方给出，超过即中止。

use std::net::IpAddr;
use std::time::Duration;

use reqwest::Url;

/// 一次下载 / 抓取的策略。
#[derive(Debug, Clone)]
pub struct NetPolicy {
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
    pub max_bytes: u64,
    pub max_redirects: usize,
    pub timeout_secs: u64,
}

impl Default for NetPolicy {
    fn default() -> Self {
        Self {
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            max_bytes: 512 * 1024 * 1024,
            max_redirects: 3,
            timeout_secs: 60,
        }
    }
}

impl NetPolicy {
    /// 构造 reqwest 客户端（重定向策略在 `check_hop` 之外由调用方逐跳检查）。
    pub fn client(&self) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(self.timeout_secs.max(1)))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.to_string())
    }

    /// 校验 URL 的协议与域名；不解析 DNS。
    pub fn check_url(&self, raw: &str) -> Result<Url, String> {
        let url = Url::parse(raw).map_err(|error| format!("invalid URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("only http and https URLs are allowed".to_owned());
        }
        let host = url
            .host_str()
            .ok_or_else(|| "URL has no host".to_owned())?
            .to_ascii_lowercase();

        for blocked in &self.blocked_domains {
            if domain_matches(&host, blocked) {
                return Err(format!("domain {host} is blocked"));
            }
        }
        if !self.allowed_domains.is_empty()
            && !self
                .allowed_domains
                .iter()
                .any(|allowed| domain_matches(&host, allowed))
        {
            return Err(format!("domain {host} is not in the allowed list"));
        }
        Ok(url)
    }

    /// 解析并校验每个 IP；拒绝非公网地址。
    pub async fn resolve_public(&self, url: &Url) -> Result<(), String> {
        let host = url.host_str().ok_or_else(|| "URL has no host".to_owned())?;
        if let Ok(ip) = host.parse::<IpAddr>() {
            return if is_public_ip(ip) {
                Ok(())
            } else {
                Err(format!("address {ip} is not public"))
            };
        }
        let port = url.port_or_known_default().unwrap_or(443);
        let addresses = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| format!("DNS lookup for {host} timed out"))?
        .map_err(|error| format!("cannot resolve {host}: {error}"))?;
        let mut any = false;
        for address in addresses {
            any = true;
            if !is_public_ip(address.ip()) {
                return Err(format!(
                    "{host} resolves to a non-public address ({})",
                    address.ip()
                ));
            }
        }
        if !any {
            return Err(format!("{host} did not resolve to any address"));
        }
        Ok(())
    }

    /// 跟随有限次重定向，每一跳都重新校验。
    pub async fn send(
        &self,
        client: &reqwest::Client,
        url: Url,
        method: reqwest::Method,
        headers: reqwest::header::HeaderMap,
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Response, String> {
        let mut current = url;
        for _ in 0..=self.max_redirects {
            self.resolve_public(&current).await?;
            let mut request = client
                .request(method.clone(), current.clone())
                .headers(headers.clone());
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request.send().await.map_err(|error| error.to_string())?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| "redirect without a Location header".to_owned())?;
                let next = current
                    .join(location)
                    .map_err(|error| format!("invalid redirect target: {error}"))?;
                // 重定向同样要过协议与域名检查。
                current = self.check_url(next.as_str())?;
                continue;
            }
            return Ok(response);
        }
        Err("too many redirects".to_owned())
    }
}

/// 域名匹配：相等或以 `.domain` 结尾。
fn domain_matches(host: &str, rule: &str) -> bool {
    let rule = rule.trim().trim_start_matches('.').to_ascii_lowercase();
    host == rule || host.ends_with(&format!(".{rule}"))
}

/// 是否为公网地址。
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_private()
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_unspecified()
                && !v4.is_broadcast()
                && !v4.is_documentation()
                && !v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                && !is_unique_local_v6(v6)
                && !is_link_local_v6(v6)
        }
    }
}

fn is_unique_local_v6(ip: std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

fn is_link_local_v6(ip: std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// 读取响应体，超过 `max_bytes` 立即中止。
pub async fn read_limited(
    response: reqwest::Response,
    max_bytes: u64,
) -> Result<(Vec<u8>, String), String> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    if let Some(length) = response.content_length() {
        if length > max_bytes {
            return Err(format!(
                "response is {length} bytes; the limit is {max_bytes}"
            ));
        }
    }
    let mut response = response;
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        out.extend_from_slice(&chunk);
        if out.len() as u64 > max_bytes {
            return Err(format!("response exceeded the {max_bytes}-byte limit"));
        }
    }
    Ok((out, content_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_ip_filter() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "0.0.0.0",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(!is_public_ip("fc00::1".parse().unwrap()));
    }

    #[test]
    fn domain_rules() {
        let policy = NetPolicy {
            allowed_domains: vec!["example.com".to_owned()],
            blocked_domains: vec!["evil.example.com".to_owned()],
            ..NetPolicy::default()
        };
        assert!(policy.check_url("https://example.com/a").is_ok());
        assert!(policy.check_url("https://sub.example.com/a").is_ok());
        assert!(policy.check_url("https://evil.example.com/a").is_err());
        assert!(policy.check_url("https://other.com/a").is_err());
        assert!(policy.check_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn blocked_wins_over_allowed() {
        let policy = NetPolicy {
            allowed_domains: vec!["example.com".to_owned()],
            blocked_domains: vec!["bad.example.com".to_owned()],
            ..NetPolicy::default()
        };
        assert!(policy.check_url("https://bad.example.com").is_err());
    }
}
