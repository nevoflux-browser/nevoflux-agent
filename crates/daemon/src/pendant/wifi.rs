//! The Wi-Fi this computer is on, and what else is in range, so the pendant's
//! form starts filled in — and so a 5 GHz network is caught before the pendant
//! (2.4 GHz only) fails to join it with reason 201.
//!
//! Windows only, from `netsh wlan`. Its labels are translated ("Channel" is
//! "信道" on a Chinese system) but "SSID" is not, so the parsers know the
//! labels in a few languages and, for the band, fall back on the channel
//! number. Elsewhere this returns nothing and the form starts empty.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Band {
    #[serde(rename = "2.4")]
    G24,
    #[serde(rename = "5")]
    G5,
    #[serde(rename = "6")]
    G6,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Network {
    pub ssid: String,
    pub band: Option<Band>,
    /// Percent, as netsh gives it.
    pub signal: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WifiView {
    pub current: Option<Network>,
    /// Read from the saved profile when Windows lets this user see it.
    pub password: Option<String>,
    /// Networks in range a pendant can join (2.4 GHz), strongest first.
    pub nearby: Vec<Network>,
}

const CHANNEL_KEYS: [&str; 6] = ["Channel", "信道", "頻道", "通道", "Kanal", "Canal"];
const BAND_KEYS: [&str; 5] = ["Band", "频带", "頻帶", "波段", "Bande"];
const KEY_CONTENT_KEYS: [&str; 6] = ["Key Content", "关键内容", "金鑰內容", "密钥内容", "Schlüsselinhalt", "Contenu de la clé"];

fn key_value(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(':')?;
    Some((k.trim(), v.trim()))
}

fn band_of(text: &str) -> Option<Band> {
    let t = text.replace(' ', "");
    if t.starts_with("2.4") {
        Some(Band::G24)
    } else if t.starts_with('5') {
        Some(Band::G5)
    } else if t.starts_with('6') {
        Some(Band::G6)
    } else {
        None
    }
}

/// 1–14 are 2.4 GHz, 32 and up 5 GHz (a 6 GHz channel says so in its band line).
fn band_of_channel(ch: &str) -> Option<Band> {
    let n: u32 = ch.split_whitespace().next()?.parse().ok()?;
    match n {
        1..=14 => Some(Band::G24),
        32..=177 => Some(Band::G5),
        _ => None,
    }
}

/// `netsh wlan show interfaces`: the network the first connected adapter is on.
pub fn parse_interfaces(text: &str) -> Option<Network> {
    let mut ssid = None;
    let mut band = None;
    let mut channel_band = None;
    let mut signal = None;
    for line in text.lines() {
        let Some((k, v)) = key_value(line) else { continue };
        if k == "SSID" && !v.is_empty() {
            if ssid.is_some() {
                break; // a second adapter
            }
            ssid = Some(v.to_string());
        } else if BAND_KEYS.contains(&k) {
            band = band_of(v);
        } else if CHANNEL_KEYS.contains(&k) {
            channel_band = band_of_channel(v);
        } else if let Some(p) = v.strip_suffix('%') {
            signal = p.trim().parse().ok();
        }
    }
    Some(Network { ssid: ssid?, band: band.or(channel_band), signal })
}

/// `netsh wlan show networks mode=bssid`: every network in range, one entry
/// per name, at its strongest access point and with each band it is on.
pub fn parse_networks(text: &str) -> Vec<Network> {
    let mut out: Vec<(Network, bool)> = Vec::new(); // (network, seen on 2.4 GHz)
    let mut cur: Option<usize> = None;
    let mut ap_band: Option<Band> = None;
    let mut ap_channel_band: Option<Band> = None;
    let mut ap_signal: Option<u8> = None;
    let close_ap = |out: &mut Vec<(Network, bool)>, cur: Option<usize>, b: Option<Band>, s: Option<u8>| {
        if let Some(i) = cur {
            let (n, on24) = &mut out[i];
            if b == Some(Band::G24) {
                *on24 = true;
            }
            if s > n.signal {
                n.signal = s;
            }
            if n.band.is_none() || b == Some(Band::G24) {
                n.band = b.or(n.band);
            }
        }
    };
    for line in text.lines() {
        let Some((k, v)) = key_value(line) else { continue };
        // "SSID 1 : name" starts a network, "BSSID 1 : aa:bb:…" an access point in it.
        let mut words = k.split_whitespace();
        let first = words.next().unwrap_or("");
        let numbered = words.next().is_some_and(|w| w.chars().all(|c| c.is_ascii_digit()));
        if first == "SSID" && numbered {
            close_ap(&mut out, cur, ap_band.or(ap_channel_band), ap_signal);
            (ap_band, ap_channel_band, ap_signal) = (None, None, None);
            if v.is_empty() {
                cur = None; // a hidden network: nothing to join by name
                continue;
            }
            cur = Some(match out.iter().position(|(n, _)| n.ssid == v) {
                Some(i) => i,
                None => {
                    out.push((Network { ssid: v.to_string(), band: None, signal: None }, false));
                    out.len() - 1
                }
            });
        } else if first == "BSSID" && numbered {
            close_ap(&mut out, cur, ap_band.or(ap_channel_band), ap_signal);
            (ap_band, ap_channel_band, ap_signal) = (None, None, None);
        } else if BAND_KEYS.contains(&k) {
            ap_band = band_of(v);
        } else if CHANNEL_KEYS.contains(&k) {
            ap_channel_band = band_of_channel(v);
        } else if let Some(p) = v.strip_suffix('%') {
            ap_signal = p.trim().parse().ok();
        }
    }
    close_ap(&mut out, cur, ap_band.or(ap_channel_band), ap_signal);
    let mut nets: Vec<Network> = out
        .into_iter()
        .map(|(mut n, on24)| {
            if on24 {
                n.band = Some(Band::G24);
            }
            n
        })
        .collect();
    nets.sort_by(|a, b| b.signal.cmp(&a.signal));
    nets
}

/// `netsh wlan show profile name=… key=clear`: the saved password, if shown.
pub fn parse_key_content(text: &str) -> Option<String> {
    text.lines()
        .filter_map(key_value)
        .find(|(k, v)| KEY_CONTENT_KEYS.contains(k) && !v.is_empty())
        .map(|(_, v)| v.to_string())
}

/// What the form starts with.
pub fn view() -> WifiView {
    imp::view()
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::process::CommandExt;

    /// No console window flashing up for each call.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// netsh in UTF-8, whatever the console's code page is.
    fn netsh(args: &str) -> Option<String> {
        let out = std::process::Command::new("cmd")
            .raw_arg(format!("/d /c chcp 65001 >nul & netsh {args}"))
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// A name netsh can take in quotes; anything else is not read.
    fn quotable(s: &str) -> bool {
        !s.is_empty() && !s.contains(['"', '&', '|', '<', '>', '^', '%', '\r', '\n'])
    }

    pub fn view() -> WifiView {
        let current = netsh("wlan show interfaces").and_then(|t| parse_interfaces(&t));
        let password = current
            .as_ref()
            .filter(|c| quotable(&c.ssid))
            .and_then(|c| netsh(&format!("wlan show profile name=\"{}\" key=clear", c.ssid)))
            .and_then(|t| parse_key_content(&t));
        let nearby = netsh("wlan show networks mode=bssid")
            .map(|t| parse_networks(&t))
            .unwrap_or_default()
            .into_iter()
            .filter(|n| n.band == Some(Band::G24))
            .collect();
        WifiView { current, password, nearby }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn view() -> WifiView {
        WifiView::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IFACE_EN: &str = "
There is 1 interface on the system:

    Name                   : Wi-Fi
    Description            : Intel(R) Wi-Fi 6 AX201 160MHz
    State                  : connected
    SSID                   : HomeNet-5G
    BSSID                  : a4:2b:b0:11:22:33
    Network type           : Infrastructure
    Radio type             : 802.11ac
    Authentication         : WPA2-Personal
    Band                   : 5 GHz
    Channel                : 149
    Signal                 : 92%
    Profile                : HomeNet-5G
";

    const IFACE_ZH_NO_BAND: &str = "
系统上有 1 个接口:

    名称                   : WLAN
    状态                   : 已连接
    SSID                   : 家里的WiFi
    BSSID                  : a4:2b:b0:11:22:34
    无线电类型             : 802.11n
    信道                   : 6
    信号                   : 80%
";

    #[test]
    fn current_network_and_its_band() {
        let n = parse_interfaces(IFACE_EN).unwrap();
        assert_eq!((n.ssid.as_str(), n.band, n.signal), ("HomeNet-5G", Some(Band::G5), Some(92)));
        let n = parse_interfaces(IFACE_ZH_NO_BAND).unwrap();
        assert_eq!((n.ssid.as_str(), n.band), ("家里的WiFi", Some(Band::G24)));
        assert!(parse_interfaces("    State : disconnected\n").is_none());
    }

    #[test]
    fn nearby_networks_one_per_name_preferring_2_4() {
        let text = "
SSID 1 : HomeNet
    Network type            : Infrastructure
    BSSID 1                 : a4:2b:b0:11:22:33
         Signal             : 60%
         Band               : 5 GHz
         Channel            : 44
    BSSID 2                 : a4:2b:b0:11:22:35
         Signal             : 75%
         Band               : 2.4 GHz
         Channel            : 6
SSID 2 : Office-5G
    BSSID 1                 : 00:11:22:33:44:55
         信号               : 90%
         信道               : 157
SSID 3 :
    BSSID 1                 : 00:11:22:33:44:66
         Signal             : 40%
";
        let n = parse_networks(text);
        assert_eq!(n.len(), 2);
        assert_eq!((n[0].ssid.as_str(), n[0].band, n[0].signal), ("Office-5G", Some(Band::G5), Some(90)));
        assert_eq!((n[1].ssid.as_str(), n[1].band, n[1].signal), ("HomeNet", Some(Band::G24), Some(75)));
    }

    #[test]
    fn saved_password_in_either_language() {
        assert_eq!(parse_key_content("    Key Content            : s3cret pass\n").as_deref(), Some("s3cret pass"));
        assert_eq!(parse_key_content("    关键内容            : 密码123\n").as_deref(), Some("密码123"));
        assert_eq!(parse_key_content("    Security key           : Present\n"), None);
    }
}
