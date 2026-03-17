// src/tools/fetch.rs
// Fetch 工具：获取网页内容并转换为 Markdown
// 安全防护：SSRF 防护（拦截内网 IP）+ 域名白名单/黑名单过滤
//
// 设计原则：
// - 默认禁用，需要在 config.toml 的 [fetch] 段显式启用
// - SSRF 防护：DNS 解析后检查所有返回 IP，拦截私有/保留地址
// - 域名过滤：支持白名单/黑名单两种模式，支持子域名匹配
// - 大小限制：响应体上限 + 输出字符数上限，防止撑爆上下文
// - HTML → Markdown 自动转换：使用 htmd crate 将网页转为 LLM 友好格式

use anyhow::{Context as _, bail};
use crate::config::FetchConfig;
use crate::tools::ToolResult;

// ============================================================================
// 安全常量
// ============================================================================

/// 默认 User-Agent 字符串
/// 使用 Chrome 浏览器的 UA 以获得更好的网页兼容性
/// 许多网站会根据 UA 返回不同的内容或直接拒绝非浏览器请求
const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/131.0.0.0 Safari/537.36";

// ============================================================================
// SSRF 防护
// ============================================================================

/// 检查 IP 地址是否为私有/保留地址
/// 用于防止 SSRF 攻击：阻止 Agent 访问内网资源
///
/// 拦截范围（IPv4）：
/// - 127.0.0.0/8    — 环回地址（loopback）
/// - 10.0.0.0/8     — A 类私有网络
/// - 172.16.0.0/12  — B 类私有网络
/// - 192.168.0.0/16 — C 类私有网络
/// - 169.254.0.0/16 — 链路本地地址
/// - 0.0.0.0        — 未指定地址
/// - 255.255.255.255 — 广播地址
/// - 100.64.0.0/10  — CGNAT（运营商级 NAT）保留地址
///
/// 拦截范围（IPv6）：
/// - ::1            — 环回地址
/// - ::             — 未指定地址
/// - fc00::/7       — 唯一本地地址 (ULA)
/// - fe80::/10      — 链路本地地址
/// - ::ffff:0:0/96  — IPv4 映射地址（递归检查映射的 IPv4）
pub fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        // IPv4 地址检查
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()            // 127.0.0.0/8 — 环回地址
            || v4.is_private()          // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 — 私有网络
            || v4.is_link_local()       // 169.254.0.0/16 — 链路本地地址
            || v4.is_unspecified()      // 0.0.0.0 — 未指定地址
            || v4.is_broadcast()        // 255.255.255.255 — 广播地址
            || (v4.octets()[0] == 100   // 100.64.0.0/10 — CGNAT（运营商级 NAT）保留地址
                && (v4.octets()[1] >= 64 && v4.octets()[1] <= 127))
        }
        // IPv6 地址检查
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()            // ::1 — 环回地址
            || v6.is_unspecified()      // :: — 未指定地址
            // fc00::/7 — 唯一本地地址 (ULA)
            || (v6.segments()[0] & 0xfe00) == 0xfc00
            // fe80::/10 — 链路本地地址
            || (v6.segments()[0] & 0xffc0) == 0xfe80
            // ::ffff:0:0/96 — IPv4 映射地址
            // 需要递归检查映射的 IPv4 是否为私有地址
            // 防止通过 ::ffff:127.0.0.1 等方式绕过 SSRF 防护
            || match v6.to_ipv4_mapped() {
                Some(v4) => is_private_ip(&std::net::IpAddr::V4(v4)),
                None => false,
            }
        }
    }
}

// ============================================================================
// 域名过滤
// ============================================================================

/// 检查域名是否被允许访问
/// 根据配置的白名单/黑名单模式进行判断
///
/// # 匹配规则
/// - 精确匹配：域名完全相同（不区分大小写）
/// - 子域名匹配：请求域名以 `.{配置域名}` 结尾时匹配
///   例如：配置 `example.com` 匹配 `sub.example.com`
///
/// # 白名单模式 ("whitelist")
/// - 域名必须在列表中才允许访问
/// - 空列表 = 禁止所有访问（安全默认值）
///
/// # 黑名单模式 ("blacklist")
/// - 域名在列表中则禁止访问
/// - 空列表 = 允许所有访问
pub fn check_domain_allowed(hostname: &str, config: &FetchConfig) -> anyhow::Result<()> {
    // 统一转为小写进行比较（域名不区分大小写）
    let hostname_lower = hostname.to_lowercase();
    // 预计算所有配置域名的小写形式
    let domains_lower: Vec<String> = config.domains.iter().map(|d| d.to_lowercase()).collect();

    match config.mode.as_str() {
        // 白名单模式：域名必须在列表中
        "whitelist" => {
            // 空白名单 = 没有可访问的域名，返回错误提示
            if domains_lower.is_empty() {
                bail!(
                    "Fetch 白名单为空，没有可访问的域名。\
                     请在 config.toml [fetch] 的 domains 中添加允许的域名。"
                );
            }
            // 检查请求域名是否匹配白名单中的任一域名（精确 或 子域名）
            if !domains_lower.iter().any(|d| {
                hostname_lower == *d || hostname_lower.ends_with(&format!(".{d}"))
            }) {
                bail!("域名 '{}' 不在 Fetch 白名单中", hostname);
            }
            Ok(())
        }
        // 黑名单模式（默认）：域名不能在列表中
        "blacklist" | _ => {
            // 检查请求域名是否匹配黑名单中的任一域名（精确 或 子域名）
            if domains_lower.iter().any(|d| {
                hostname_lower == *d || hostname_lower.ends_with(&format!(".{d}"))
            }) {
                bail!("域名 '{}' 在 Fetch 黑名单中，禁止访问", hostname);
            }
            Ok(())
        }
    }
}

// ============================================================================
// DNS 解析 + SSRF 检查
// ============================================================================

/// 解析域名并检查所有返回的 IP 是否安全
/// 防止 DNS 重绑定攻击：必须检查所有解析结果，而非仅检查第一个
///
/// # 参数
/// - `hostname`: 要解析的域名
/// - `port`: 端口号（用于构造 socket 地址格式）
///
/// # 错误
/// - DNS 解析失败
/// - DNS 返回空结果
/// - 任一 IP 为私有/保留地址
async fn resolve_and_check_ssrf(hostname: &str, port: u16) -> anyhow::Result<()> {
    // 构造 host:port 格式用于 DNS 查询
    let addr = format!("{}:{}", hostname, port);
    // 异步 DNS 解析，返回所有 IP 地址
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&addr)
        .await
        .with_context(|| format!("DNS 解析失败: {}", hostname))?
        .collect();

    // 空结果检查：DNS 解析成功但无记录
    if addrs.is_empty() {
        bail!("DNS 解析返回空结果: {}", hostname);
    }

    // 逐一检查每个解析到的 IP 地址
    // 必须全部检查，防止攻击者通过多 A 记录绕过
    for addr in &addrs {
        if is_private_ip(&addr.ip()) {
            bail!(
                "SSRF 防护：域名 '{}' 解析到私有 IP 地址 {}，已拦截",
                hostname,
                addr.ip()
            );
        }
    }

    Ok(())
}

// ============================================================================
// Fetch 核心逻辑
// ============================================================================

/// Fetch 工具入口：获取 URL 内容并转为 Markdown
/// 这是暴露给 Agent 的顶层函数
///
/// # 参数
/// - `url`: 要获取的网页 URL
/// - `config`: Fetch 工具配置（超时、大小限制、域名过滤等）
///
/// # 返回
/// - 成功：包含 URL、Content-Type 和 Markdown 内容的格式化字符串
/// - 失败：包含错误描述的 ToolResult
pub async fn fetch_url(url: &str, config: &FetchConfig) -> ToolResult {
    // 委托给内部实现函数，统一错误处理
    match fetch_url_inner(url, config).await {
        Ok(content) => ToolResult::ok(content),
        Err(e) => ToolResult::err(format!("Fetch 失败: {e}")),
    }
}

/// Fetch 内部实现
/// 执行完整的 URL 获取流程：解析 → 验证 → DNS 检查 → HTTP 请求 → 内容转换
async fn fetch_url_inner(url: &str, config: &FetchConfig) -> anyhow::Result<String> {
    // 1. 解析 URL 为结构化格式
    let parsed = reqwest::Url::parse(url)
        .with_context(|| format!("无效的 URL: {url}"))?;

    // 2. 验证 scheme：仅允许 http 和 https
    match parsed.scheme() {
        "http" | "https" => {}  // 允许的协议
        other => bail!("不支持的 URL scheme: '{other}'，仅支持 http 和 https"),
    }

    // 3. 提取主机名（域名或 IP）
    let hostname = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL 中缺少主机名: {url}"))?;

    // 4. 域名过滤检查（白名单/黑名单）
    check_domain_allowed(hostname, config)?;

    // 5. DNS 解析 + SSRF 防护检查
    // 获取端口号：显式指定的端口 或 协议默认端口（http=80, https=443）
    let port = parsed.port_or_known_default().unwrap_or(443);
    resolve_and_check_ssrf(hostname, port).await?;

    // 6. 构建 HTTP 客户端
    // 使用配置的 User-Agent 或默认的 Chrome UA
    let ua = config.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.timeout_secs)) // 请求超时
        .user_agent(ua)                                                // User-Agent 头
        .redirect(reqwest::redirect::Policy::limited(5))               // 最多跟随 5 次重定向
        .build()
        .context("构建 HTTP 客户端失败")?;

    // 7. 发送 GET 请求
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("HTTP 请求失败: {url}"))?;

    // 提取响应状态码和 Content-Type（在消费 body 之前）
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)   // 获取 Content-Type 头
        .and_then(|v| v.to_str().ok())         // 转为字符串
        .unwrap_or("unknown")                  // 缺失时使用 "unknown"
        .to_string();

    // 8. 检查 HTTP 状态码
    if !status.is_success() {
        bail!("HTTP {} — {}", status.as_u16(), url);
    }

    // 9. 读取响应体（带大小限制）
    // 直接读取全部 bytes，然后检查是否超出限制
    let body_bytes = response
        .bytes()
        .await
        .context("读取响应体失败")?;
    // 如果超出限制，截断到指定大小
    let body_bytes = if body_bytes.len() > config.max_response_bytes {
        body_bytes.slice(..config.max_response_bytes)
    } else {
        body_bytes
    };

    // 10. 转为 UTF-8 文本（非 UTF-8 字节替换为 U+FFFD）
    let body_text = String::from_utf8_lossy(&body_bytes);

    // 11. 根据 Content-Type 处理内容
    let content = if content_type.contains("text/html")
        || content_type.contains("application/xhtml")
    {
        // HTML → Markdown 转换
        // 使用 htmd crate 将 HTML 转为 Markdown 格式
        htmd::convert(&body_text).unwrap_or_else(|_| body_text.to_string())
    } else if content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
    {
        // 文本类型（纯文本、JSON、XML 等）直接返回原文
        body_text.to_string()
    } else {
        // 二进制或其他非文本类型：仅返回元数据摘要
        format!(
            "（非文本内容，Content-Type: {}，大小: {} 字节）",
            content_type,
            body_bytes.len()
        )
    };

    // 12. 截断输出到配置的最大字符数
    let char_count = content.chars().count();
    let truncated = if char_count > config.max_output_chars {
        // 超出限制时截断并附加提示信息
        let truncated: String = content.chars().take(config.max_output_chars).collect();
        format!(
            "{}\n\n[内容已截断，原始长度: {} 字符]",
            truncated, char_count
        )
    } else {
        content
    };

    // 13. 格式化最终输出（包含 URL 和 Content-Type 元数据）
    Ok(format!(
        "URL: {}\nContent-Type: {}\n\n{}",
        url, content_type, truncated
    ))
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // SSRF 防护测试
    // ========================================================================

    /// 测试：IPv4 私有/保留地址应被拦截
    #[test]
    fn test_private_ipv4() {
        // 环回地址 127.0.0.0/8
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        // A 类私有网络 10.0.0.0/8
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        // B 类私有网络 172.16.0.0/12
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        // C 类私有网络 192.168.0.0/16
        assert!(is_private_ip(&"192.168.1.1".parse().unwrap()));
        // 链路本地地址 169.254.0.0/16
        assert!(is_private_ip(&"169.254.1.1".parse().unwrap()));
        // 未指定地址
        assert!(is_private_ip(&"0.0.0.0".parse().unwrap()));
        // CGNAT 保留地址 100.64.0.0/10
        assert!(is_private_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_private_ip(&"100.127.255.254".parse().unwrap()));
        // 公网 IP 不应被拦截
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip(&"203.0.113.1".parse().unwrap()));
        // CGNAT 范围外的 100.x.x.x 不应被拦截
        assert!(!is_private_ip(&"100.128.0.1".parse().unwrap()));
        assert!(!is_private_ip(&"100.63.255.255".parse().unwrap()));
    }

    /// 测试：IPv6 私有/保留地址应被拦截
    #[test]
    fn test_private_ipv6() {
        // 环回地址 ::1
        assert!(is_private_ip(&"::1".parse().unwrap()));
        // 未指定地址 ::
        assert!(is_private_ip(&"::".parse().unwrap()));
        // 唯一本地地址 fc00::/7
        assert!(is_private_ip(&"fc00::1".parse().unwrap()));
        assert!(is_private_ip(&"fd00::1".parse().unwrap()));
        // 链路本地地址 fe80::/10
        assert!(is_private_ip(&"fe80::1".parse().unwrap()));
        // 公网 IPv6 不应被拦截
        assert!(!is_private_ip(&"2001:4860:4860::8888".parse().unwrap()));
        assert!(!is_private_ip(&"2606:4700:4700::1111".parse().unwrap()));
    }

    /// 测试：IPv4 映射的 IPv6 地址应递归检查映射的 IPv4
    #[test]
    fn test_ipv4_mapped_ipv6() {
        // ::ffff:127.0.0.1 → 映射到 127.0.0.1（环回），应拦截
        assert!(is_private_ip(&"::ffff:127.0.0.1".parse().unwrap()));
        // ::ffff:10.0.0.1 → 映射到 10.0.0.1（私有），应拦截
        assert!(is_private_ip(&"::ffff:10.0.0.1".parse().unwrap()));
        // ::ffff:192.168.1.1 → 映射到 192.168.1.1（私有），应拦截
        assert!(is_private_ip(&"::ffff:192.168.1.1".parse().unwrap()));
        // ::ffff:8.8.8.8 → 映射到 8.8.8.8（公网），不应拦截
        assert!(!is_private_ip(&"::ffff:8.8.8.8".parse().unwrap()));
    }

    // ========================================================================
    // 域名过滤测试
    // ========================================================================

    /// 测试：白名单模式——在列表中的域名及子域名应允许
    #[test]
    fn test_domain_whitelist() {
        // 构造白名单配置
        let config = FetchConfig {
            mode: "whitelist".to_string(),
            domains: vec!["example.com".to_string(), "api.github.com".to_string()],
            ..Default::default()
        };
        // 精确匹配应通过
        assert!(check_domain_allowed("example.com", &config).is_ok());
        // 子域名匹配应通过
        assert!(check_domain_allowed("sub.example.com", &config).is_ok());
        // 精确匹配应通过
        assert!(check_domain_allowed("api.github.com", &config).is_ok());
        // 不在白名单中的域名应拒绝
        assert!(check_domain_allowed("evil.com", &config).is_err());
        // github.com 本身不在白名单中（只有 api.github.com）
        assert!(check_domain_allowed("github.com", &config).is_err());
    }

    /// 测试：黑名单模式——在列表中的域名及子域名应禁止
    #[test]
    fn test_domain_blacklist() {
        // 构造黑名单配置
        let config = FetchConfig {
            mode: "blacklist".to_string(),
            domains: vec!["evil.com".to_string()],
            ..Default::default()
        };
        // 不在黑名单中的域名应允许
        assert!(check_domain_allowed("example.com", &config).is_ok());
        // 精确匹配应拒绝
        assert!(check_domain_allowed("evil.com", &config).is_err());
        // 子域名匹配应拒绝
        assert!(check_domain_allowed("sub.evil.com", &config).is_err());
    }

    /// 测试：空白名单应拒绝所有域名
    #[test]
    fn test_empty_whitelist_rejects_all() {
        let config = FetchConfig {
            mode: "whitelist".to_string(),
            domains: vec![],               // 空白名单
            ..Default::default()
        };
        // 任何域名都应被拒绝
        assert!(check_domain_allowed("anything.com", &config).is_err());
    }

    /// 测试：空黑名单应允许所有域名
    #[test]
    fn test_empty_blacklist_allows_all() {
        let config = FetchConfig {
            mode: "blacklist".to_string(),
            domains: vec![],               // 空黑名单
            ..Default::default()
        };
        // 任何域名都应被允许
        assert!(check_domain_allowed("anything.com", &config).is_ok());
    }

    /// 测试：域名比较应不区分大小写
    #[test]
    fn test_domain_case_insensitive() {
        let config = FetchConfig {
            mode: "blacklist".to_string(),
            domains: vec!["Evil.COM".to_string()],
            ..Default::default()
        };
        // 大小写不同但应匹配
        assert!(check_domain_allowed("evil.com", &config).is_err());
        assert!(check_domain_allowed("EVIL.COM", &config).is_err());
        assert!(check_domain_allowed("Evil.Com", &config).is_err());
    }
}
