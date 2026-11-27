//! Reading the ports a host listens on, from the output of `ss` or `netstat`.
//!
//! Pure parsing: running the command is the adapters' job (see [`HOST_PORTS_SCRIPT`]).

use std::collections::BTreeMap;

/// The command a host runs to list its listening TCP ports: `ss` first, `netstat` as fallback.
pub const HOST_PORTS_SCRIPT: &str = "ss -Hltn 2>/dev/null || netstat -ltn 2>/dev/null";

/// One listening socket: the address it is bound to (without brackets) and its port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Listener {
    pub address: String,
    pub port: u16,
}

/// Split a `address:port` local address; the port is the text after the last `:`.
fn split_local(field: &str) -> Option<Listener> {
    let (address, port) = field.rsplit_once(':')?;
    let port = port.parse().ok()?;
    Some(Listener {
        address: address.trim_matches(|c| c == '[' || c == ']').to_string(),
        port,
    })
}

/// Parse `ss -Hltn` output: `LISTEN` lines, local address in column 4.
pub fn parse_ss(output: &str) -> Vec<Listener> {
    output
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.first() != Some(&"LISTEN") {
                return None;
            }
            split_local(cols.get(3)?)
        })
        .collect()
}

/// Parse `netstat -ltn` output: lines ending in `LISTEN`, local address in column 4.
pub fn parse_netstat(output: &str) -> Vec<Listener> {
    output
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if !cols.contains(&"LISTEN") {
                return None;
            }
            split_local(cols.get(3)?)
        })
        .collect()
}

/// Parse either tool's output: the `ss` shape when any line has it, else `netstat`'s.
pub fn parse_listeners(output: &str) -> Vec<Listener> {
    let ss_shaped = output
        .lines()
        .any(|l| l.split_whitespace().next() == Some("LISTEN"));
    if ss_shaped {
        parse_ss(output)
    } else {
        parse_netstat(output)
    }
}

/// The ports worth suggesting, ascending and without 22: one entry per port, the flag being
/// true when every listener of that port is bound to loopback only.
pub fn suggested_ports(listeners: &[Listener]) -> Vec<(u16, bool)> {
    let mut ports: BTreeMap<u16, bool> = BTreeMap::new();
    for l in listeners.iter().filter(|l| l.port != 22) {
        let loopback = l.address.starts_with("127.") || l.address == "::1";
        let entry = ports.entry(l.port).or_insert(true);
        *entry &= loopback;
    }
    ports.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SS: &str = "LISTEN 0 4096 0.0.0.0:22 0.0.0.0:*\n\
LISTEN 0 511 [::]:80 [::]:*\n\
LISTEN 0 128 *:8080 *:*\n\
LISTEN 0 244 127.0.0.1:5432 0.0.0.0:*\n\
LISTEN 0 244 [::1]:5432 [::]:*\n";

    const NETSTAT: &str = "Active Internet connections (only servers)\n\
Proto Recv-Q Send-Q Local Address Foreign Address State\n\
tcp6 0 0 :::80 :::* LISTEN\n\
tcp 0 0 127.0.0.1:6379 0.0.0.0:* LISTEN\n\
tcp 0 0 0.0.0.0:22 0.0.0.0:* LISTEN\n";

    #[test]
    fn ss_output_gives_sorted_ports_without_ssh() {
        assert_eq!(
            suggested_ports(&parse_ss(SS)),
            vec![(80, false), (5432, true), (8080, false)]
        );
    }

    #[test]
    fn netstat_output_gives_sorted_ports() {
        assert_eq!(
            suggested_ports(&parse_netstat(NETSTAT)),
            vec![(80, false), (6379, true)]
        );
    }

    #[test]
    fn parse_listeners_picks_the_shape() {
        assert_eq!(parse_listeners(SS), parse_ss(SS));
        assert_eq!(parse_listeners(NETSTAT), parse_netstat(NETSTAT));
    }

    #[test]
    fn addresses_lose_their_brackets() {
        let l = parse_ss("LISTEN 0 511 [::1]:80 [::]:*");
        assert_eq!(
            l,
            vec![Listener {
                address: "::1".into(),
                port: 80
            }]
        );
    }

    #[test]
    fn empty_and_garbage_give_nothing() {
        assert!(suggested_ports(&parse_listeners("")).is_empty());
        assert!(suggested_ports(&parse_listeners("not a table\nLISTEN x y z:q")).is_empty());
    }
}
