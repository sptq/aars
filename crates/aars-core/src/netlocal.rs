//! Czy klient panelu jest w sieci lokalnej stacji (etap 10, R12). Kreator
//! pierwszego konta działa tylko stąd. Czysta logika: listę sieci
//! interfejsów liczy binarka przy starcie.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Adres interfejsu stacji z długością prefiksu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNet {
    pub addr: IpAddr,
    pub prefix: u8,
}

/// Długość prefiksu IPv6, przy której klient należy do sieci interfejsu.
/// Stała, a nie prefiks interfejsu: adres z DHCPv6 bywa `/128`, a operator
/// może przydzielić `/56` całemu klientowi, a nie jednej sieci LAN.
const LAN_PREFIX_V6: u32 = 64;

/// Pętla zwrotna, RFC 1918, link-local v4/v6, ULA `fc00::/7` oraz IPv6 z tym
/// samym `/64` co dowolny globalny adres interfejsu stacji. IPv4 w postaci
/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`) liczy się jak IPv4. Publiczne IPv4 i
/// CGNAT (`100.64.0.0/10`) nie są lokalne nawet w sieci interfejsu.
pub fn is_local_client(ip: IpAddr, nets: &[LocalNet]) -> bool {
    match ip {
        IpAddr::V4(v4) => is_local_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_local_v4(v4),
            None => is_local_v6(v6, nets),
        },
    }
}

fn is_local_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn is_local_v6(ip: Ipv6Addr, nets: &[LocalNet]) -> bool {
    let segments = ip.segments();
    let link_local = segments[0] & 0xffc0 == 0xfe80;
    let unique_local = segments[0] & 0xfe00 == 0xfc00;
    if ip.is_loopback() || link_local || unique_local {
        return true;
    }
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    let mask = u128::MAX << (128 - LAN_PREFIX_V6);
    nets.iter().any(|net| match net.addr {
        IpAddr::V6(own) if is_global_interface(own) => {
            u128::from(own) & mask == u128::from(ip) & mask
        }
        _ => false,
    })
}

/// Adres interfejsu, którego `/64` wyznacza sieć LAN: nie pętla, nie
/// link-local, nie ULA (te są lokalne i tak), nie `::` ani multicast.
fn is_global_interface(own: Ipv6Addr) -> bool {
    let first = own.segments()[0];
    !(own.is_loopback()
        || own.is_unspecified()
        || own.is_multicast()
        || own.to_ipv4_mapped().is_some()
        || first & 0xffc0 == 0xfe80
        || first & 0xfe00 == 0xfc00)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(addr: &str, prefix: u8) -> LocalNet {
        LocalNet {
            addr: addr.parse().unwrap(),
            prefix,
        }
    }

    #[test]
    fn is_local_client_table() {
        let nets = [
            net("192.168.1.2", 24),
            net("2a01:110::1", 64),
            net("::1", 128),
            net("fe80::1", 64),
        ];
        let cases = [
            ("127.0.0.1", true),
            ("10.1.2.3", true),
            ("172.16.0.1", true),
            ("172.31.255.255", true),
            ("172.32.0.1", false),
            ("192.168.1.5", true),
            ("169.254.1.1", true),
            ("100.64.0.1", false),
            ("8.8.8.8", false),
            ("::1", true),
            ("fe80::1", true),
            ("febf::1", true),
            ("fec0::1", false),
            ("fd00::1", true),
            ("fc00::1", true),
            ("2a01:110::5", true),
            ("2a01:110::ffff:1:2:3", true),
            ("2a01:111::5", false),
            ("2a01:110:0:1::5", false),
            ("::ffff:192.168.1.5", true),
            ("::ffff:8.8.8.8", false),
            ("::", false),
            ("ff02::1", false),
        ];
        let mut failures = Vec::new();
        for (ip, expected) in cases {
            let got = is_local_client(ip.parse().unwrap(), &nets);
            if got != expected {
                failures.push(format!("{ip}: {got}, oczekiwano {expected}"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn global_ipv6_needs_an_interface_in_the_same_slash_64() {
        let ip: IpAddr = "2a01:110::5".parse().unwrap();
        assert!(!is_local_client(ip, &[]));
        // Sama pętla i link-local nie otwierają całego `::/64` ani `fe80::/64`.
        assert!(!is_local_client(
            "::5".parse().unwrap(),
            &[net("::1", 128)]
        ));
        // Adres interfejsu `/128` z DHCPv6 też wyznacza swoje `/64`.
        assert!(is_local_client(ip, &[net("2a01:110::1", 128)]));
        // Sieć IPv4 interfejsu nie otwiera publicznych adresów IPv4.
        assert!(!is_local_client(
            "203.0.113.7".parse().unwrap(),
            &[net("203.0.113.1", 24)]
        ));
    }
}
