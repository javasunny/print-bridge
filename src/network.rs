use std::net::Ipv4Addr;

const LISTEN_PORT: u16 = 8631;

#[derive(Debug, serde::Deserialize)]
#[cfg(any(windows, test))]
struct AddressCandidate {
    #[serde(rename = "IPAddress", alias = "IpAddress", default)]
    ip_address: String,
    #[serde(rename = "InterfaceAlias", default)]
    interface_alias: String,
    #[serde(rename = "InterfaceDescription", default)]
    interface_description: String,
    #[serde(rename = "InterfaceMetric", default)]
    interface_metric: u32,
    #[serde(rename = "RouteMetric", default)]
    route_metric: u32,
}

#[cfg(windows)]
pub fn default_listen_address() -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    let script = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$items = @(
  Get-NetAdapter -Physical -ErrorAction Stop |
    Where-Object { $_.Status -eq 'Up' } |
    ForEach-Object {
      $adapter = $_
      $configuration = Get-NetIPConfiguration -InterfaceIndex $adapter.ifIndex -ErrorAction SilentlyContinue
      if ($null -ne $configuration -and $null -ne $configuration.IPv4DefaultGateway) {
        $route = Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -InterfaceIndex $adapter.ifIndex -ErrorAction SilentlyContinue |
          Sort-Object RouteMetric |
          Select-Object -First 1
        if ($null -ne $route) {
          $networkInterface = Get-NetIPInterface -InterfaceIndex $adapter.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue |
            Select-Object -First 1
          $interfaceMetric = if ($null -eq $networkInterface) { 0 } else { [uint32]$networkInterface.InterfaceMetric }
          foreach ($address in $configuration.IPv4Address) {
            if ($null -ne $address -and -not [string]::IsNullOrWhiteSpace($address.IPAddress)) {
              [PSCustomObject]@{
                IPAddress = [string]$address.IPAddress
                InterfaceAlias = [string]$adapter.Name
                InterfaceDescription = [string]$adapter.InterfaceDescription
                InterfaceMetric = $interfaceMetric
                RouteMetric = [uint32]$route.RouteMetric
              }
            }
          }
        }
      }
    }
)
ConvertTo-Json -InputObject $items -Depth 3 -Compress
"#;

    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .creation_flags(0x08000000)
        .output()
        .map_err(|_| "无法运行 Windows 网络地址检测程序".to_owned())?;
    if !output.status.success() {
        return Err("无法读取 Windows 网络配置，请确认网络服务可用".into());
    }
    select_windows_address(&output.stdout).map(|ip| format!("{ip}:{LISTEN_PORT}"))
}

#[cfg(target_os = "macos")]
pub fn default_listen_address() -> Result<String, String> {
    use std::net::UdpSocket;

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|_| "无法初始化本机网络地址检测".to_owned())?;
    // UDP connect selects the default route's local address without sending a packet.
    socket
        .connect((Ipv4Addr::new(8, 8, 8, 8), 53))
        .map_err(|_| "无法选定默认路由的本机 IPv4 地址".to_owned())?;
    let address = match socket
        .local_addr()
        .map_err(|_| "无法读取本机网络地址".to_owned())?
        .ip()
    {
        std::net::IpAddr::V4(address) if is_usable_ipv4(address) => address,
        _ => return Err(no_lan_address_error()),
    };
    Ok(format!("{address}:{LISTEN_PORT}"))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn default_listen_address() -> Result<String, String> {
    Err("默认监听地址建议仅支持 Windows 和 macOS".into())
}

#[cfg(any(windows, test))]
fn select_windows_address(bytes: &[u8]) -> Result<Ipv4Addr, String> {
    let candidates = parse_candidates(bytes)?;
    candidates
        .iter()
        .filter(|candidate| !is_virtual_adapter(candidate))
        .filter_map(|candidate| {
            candidate
                .ip_address
                .parse::<Ipv4Addr>()
                .ok()
                .filter(|address| is_usable_ipv4(*address))
                .map(|address| {
                    (
                        !address.is_private(),
                        candidate
                            .interface_metric
                            .saturating_add(candidate.route_metric),
                        address.octets(),
                        address,
                    )
                })
        })
        .min_by_key(|(non_private, metric, octets, _)| (*non_private, *metric, *octets))
        .map(|(_, _, _, address)| address)
        .ok_or_else(no_lan_address_error)
}

#[cfg(any(windows, test))]
fn is_virtual_adapter(candidate: &AddressCandidate) -> bool {
    const VIRTUAL_HINTS: [&str; 14] = [
        "virtual",
        "hyper-v",
        "hyperv",
        "vethernet",
        "wsl",
        "vmware",
        "virtualbox",
        "vbox",
        "vpn",
        "wireguard",
        "wintun",
        "tailscale",
        "zerotier",
        "docker",
    ];
    let names = format!(
        "{} {}",
        candidate.interface_alias.to_ascii_lowercase(),
        candidate.interface_description.to_ascii_lowercase()
    );
    VIRTUAL_HINTS.iter().any(|hint| names.contains(hint))
        || names.contains("tap")
        || names.contains("loopback")
}

#[cfg(any(windows, target_os = "macos", test))]
fn is_usable_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_multicast()
        && octets[0] != 0
        && octets[0] < 224
}

#[cfg(any(windows, target_os = "macos", test))]
fn no_lan_address_error() -> String {
    "没有可用的局域网 IPv4 地址，请确认已连接的物理网卡和默认网关".into()
}

#[cfg(any(windows, test))]
fn parse_candidates(bytes: &[u8]) -> Result<Vec<AddressCandidate>, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "Windows 网络配置结果不是 UTF-8".to_owned())?
        .trim_start_matches('\u{feff}')
        .trim();
    if text.is_empty() || text == "null" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "Windows 网络配置结果无效".to_owned())?;
    match value {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| {
                serde_json::from_value(value).map_err(|_| "Windows 网络配置结果无效".to_owned())
            })
            .collect(),
        value @ serde_json::Value::Object(_) => serde_json::from_value(value)
            .map(|candidate| vec![candidate])
            .map_err(|_| "Windows 网络配置结果无效".to_owned()),
        _ => Err("Windows 网络配置结果无效".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_ipv4_excludes_invalid_and_non_unicast_addresses() {
        for address in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "169.254.10.20",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!is_usable_ipv4(address.parse().unwrap()), "{address}");
        }
        for address in ["10.1.2.3", "172.20.2.3", "192.168.9.128", "8.8.8.8"] {
            assert!(is_usable_ipv4(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn parses_empty_single_and_multiple_network_configurations() {
        assert!(parse_candidates(b"").unwrap().is_empty());
        assert!(parse_candidates(b"null").unwrap().is_empty());
        assert!(
            parse_candidates(br#"{"IPAddress":"192.168.9.128","InterfaceAlias":"Ethernet"}"#)
                .unwrap()
                .len()
                == 1
        );
        assert_eq!(
            parse_candidates(br#"[{"IPAddress":"192.168.1.5"},{"IPAddress":"10.0.0.2"}]"#)
                .unwrap()
                .len(),
            2
        );
        assert!(parse_candidates(b"not json").is_err());
    }

    #[test]
    fn chooses_private_primary_physical_ipv4_and_skips_invalid_or_virtual_rows() {
        let bytes = br#"[
          {"IPAddress":"169.254.10.2","InterfaceAlias":"Ethernet","InterfaceMetric":5,"RouteMetric":1},
          {"IPAddress":"10.0.0.5","InterfaceAlias":"vEthernet (WSL)","InterfaceMetric":1,"RouteMetric":1},
          {"IPAddress":"8.8.8.8","InterfaceAlias":"Ethernet","InterfaceMetric":2,"RouteMetric":1},
          {"IPAddress":"192.168.9.128","InterfaceAlias":"Wi-Fi","InterfaceDescription":"Intel Wireless Adapter","InterfaceMetric":10,"RouteMetric":5},
          {"IPAddress":"192.168.1.30","InterfaceAlias":"Ethernet","InterfaceDescription":"Intel Ethernet","InterfaceMetric":20,"RouteMetric":5}
        ]"#;
        assert_eq!(
            select_windows_address(bytes).unwrap(),
            "192.168.9.128".parse::<Ipv4Addr>().unwrap()
        );
    }

    #[test]
    fn reports_no_lan_address_instead_of_falling_back_to_loopback() {
        let bytes = br#"[{"IPAddress":"127.0.0.1","InterfaceAlias":"Ethernet"},{"IPAddress":"169.254.2.3","InterfaceAlias":"Wi-Fi"}]"#;
        assert!(select_windows_address(bytes)
            .unwrap_err()
            .contains("局域网 IPv4 地址"));
    }
}
