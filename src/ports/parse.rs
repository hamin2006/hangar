//! Pure parsers for socket listings. Kept platform-independent so both can be tested anywhere.

use std::net::{Ipv4Addr, Ipv6Addr};

/// One listening socket as reported by the OS, before grouping and enrichment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSocket {
    pub pid: u32,
    pub process: String,
    /// `*` for all interfaces, otherwise the bound address.
    pub addr: String,
    pub port: u16,
}

/// Parses `lsof -nP -iTCP -sTCP:LISTEN -F pcn` field output.
pub fn lsof(text: &str) -> Vec<RawSocket> {
    let mut out = Vec::new();
    let mut pid: Option<u32> = None;
    let mut process = String::new();
    for line in text.lines() {
        let mut chars = line.chars();
        let Some(tag) = chars.next() else { continue };
        let value = chars.as_str();
        match tag {
            'p' => {
                pid = value.trim().parse().ok();
                process.clear();
            }
            'c' => process = value.to_string(),
            'n' => {
                if let (Some(pid), Some((addr, port))) = (pid, split_host_port(value)) {
                    out.push(RawSocket {
                        pid,
                        process: process.clone(),
                        addr,
                        port,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// `*:3000`, `127.0.0.1:5173`, `[::1]:8080`, `[fe80::1%lo0]:22` → (host, port).
fn split_host_port(name: &str) -> Option<(String, u16)> {
    let name = name.split("->").next()?.trim();
    let name = name.split_whitespace().next()?;
    let (host, port) = name.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some((normalize_addr(host), port))
}

fn normalize_addr(host: &str) -> String {
    match host {
        "" | "*" | "0.0.0.0" | "::" => "*".into(),
        h => h.to_string(),
    }
}

/// A socket row from `/proc/net/tcp` or `/proc/net/tcp6` in the LISTEN state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSocket {
    pub addr: String,
    pub port: u16,
    pub inode: u64,
}

/// Parses `/proc/net/tcp{,6}`, keeping only listening sockets (state `0A`).
pub fn proc_net_tcp(text: &str) -> Vec<ProcSocket> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 || cols[3] != "0A" {
                return None;
            }
            let (addr_hex, port_hex) = cols[1].split_once(':')?;
            let port = u16::from_str_radix(port_hex, 16).ok()?;
            let addr = decode_proc_addr(addr_hex)?;
            let inode = cols[9].parse().ok()?;
            Some(ProcSocket {
                addr: normalize_addr(&addr),
                port,
                inode,
            })
        })
        .collect()
}

/// Addresses are printed as host-order 32-bit words; on the little-endian machines Linux runs on
/// that means each word's bytes are reversed.
fn decode_proc_addr(hex: &str) -> Option<String> {
    let words: Vec<u32> = (0..hex.len() / 8)
        .map(|i| u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    match bytes.len() {
        4 => Some(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string()),
        16 => {
            let arr: [u8; 16] = bytes.try_into().ok()?;
            let v6 = Ipv6Addr::from(arr);
            Some(match v6.to_ipv4_mapped() {
                Some(v4) => v4.to_string(),
                None => v6.to_string(),
            })
        }
        _ => None,
    }
}

/// Extracts the inode from a `/proc/<pid>/fd/*` link target like `socket:[12345]`.
pub fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_fields() {
        let text = "p588\ncControlCenter\nf8\nn*:7000\nf9\nn*:7000\np4242\ncnode\nf23\nn127.0.0.1:5173\nf24\nn[::1]:5173\np77\ncweird name\nn[fe80::1%lo0]:22\nnbogus\np\nn*:1\n";
        let s = lsof(text);
        assert_eq!(s.len(), 5);
        assert_eq!(
            s[0],
            RawSocket {
                pid: 588,
                process: "ControlCenter".into(),
                addr: "*".into(),
                port: 7000
            }
        );
        assert_eq!(s[2].addr, "127.0.0.1");
        assert_eq!(s[3].addr, "::1");
        assert_eq!(s[3].port, 5173);
        assert_eq!(s[4].process, "weird name");
        assert_eq!(s[4].addr, "fe80::1%lo0");
        assert!(lsof("").is_empty());
    }

    #[test]
    fn proc_tcp_v4_and_v6() {
        let v4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 31337 1 0000000000000000 100 0 0 10 0
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 222 1 0000000000000000 100 0 0 10 0
   2: 0100007F:A5B2 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 999 1 0000000000000000 20 4 30 10 -1
";
        let s = proc_net_tcp(v4);
        assert_eq!(s.len(), 2, "established sockets are skipped");
        assert_eq!(
            s[0],
            ProcSocket {
                addr: "127.0.0.1".into(),
                port: 8080,
                inode: 31337
            }
        );
        assert_eq!(s[1].addr, "*");
        assert_eq!(s[1].port, 22);

        let v6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:1435 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4444 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 5555 1 0000000000000000 100 0 0 10 0
   2: 0000000000000000FFFF00000100007F:0050 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 6666 1 0000000000000000 100 0 0 10 0
";
        let s = proc_net_tcp(v6);
        assert_eq!(s[0].addr, "::1");
        assert_eq!(s[0].port, 5173);
        assert_eq!(s[1].addr, "*");
        assert_eq!(s[1].port, 3000);
        assert_eq!(s[2].addr, "127.0.0.1", "v4-mapped addresses read as IPv4");
        assert!(proc_net_tcp("header only\n").is_empty());
        assert!(proc_net_tcp("h\n 0: ZZZZ:XYZ 0 0A 0 0 0 0 0 1\n").is_empty());
    }

    #[test]
    fn inode_links() {
        assert_eq!(socket_inode("socket:[31337]"), Some(31337));
        assert_eq!(socket_inode("pipe:[1]"), None);
        assert_eq!(socket_inode("/dev/null"), None);
    }
}
