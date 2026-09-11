//! Windows LAN discovery must not depend on the chosen internet interface.
//! This selects the address advertised in pairing links. The desktop listener
//! still binds 0.0.0.0 and accepts connections through all IPv4 interfaces.
use super::PeerSyncError;
use std::{collections::HashMap, net::Ipv4Addr};
use windows_sys::Win32::{
    Foundation::NO_ERROR,
    NetworkManagement::IpHelper::{
        FreeMibTable, GetIpForwardTable2, GetIpInterfaceEntry, MIB_IPFORWARD_TABLE2,
        MIB_IPINTERFACE_ROW,
    },
    Networking::WinSock::AF_INET,
};

pub(super) fn discover() -> Result<Ipv4Addr, PeerSyncError> {
    let interfaces = if_addrs::get_if_addrs()?;
    let costs = default_route_costs()?;
    select_address(&interfaces, &costs).ok_or_else(|| {
        PeerSyncError::Validation("no private IPv4 LAN address is available".to_owned())
    })
}

fn select_address(
    interfaces: &[if_addrs::Interface],
    default_costs: &HashMap<u32, u64>,
) -> Option<Ipv4Addr> {
    interfaces
        .iter()
        .filter(|interface| interface.is_oper_up())
        .filter_map(|interface| {
            let std::net::IpAddr::V4(address) = interface.ip() else {
                return None;
            };
            if !address.is_private() && !address.is_link_local() {
                return None;
            }
            let cost = interface.index.and_then(|index| default_costs.get(&index));
            // Prefer private over link-local, then adapters with a default
            // gateway over isolated/virtual networks. Among gateways, respect
            // Windows' combined route + interface metric. Ties are stable.
            Some((
                (
                    !address.is_private(),
                    cost.is_none(),
                    cost.copied().unwrap_or(u64::MAX),
                    interface.index.unwrap_or(u32::MAX),
                    address,
                ),
                address,
            ))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, address)| address)
}

fn default_route_costs() -> std::io::Result<HashMap<u32, u64>> {
    struct RouteTable(*mut MIB_IPFORWARD_TABLE2);
    impl Drop for RouteTable {
        fn drop(&mut self) {
            // SAFETY: GetIpForwardTable2 owns this allocation until FreeMibTable.
            unsafe { FreeMibTable(self.0.cast()) };
        }
    }

    let mut table = std::ptr::null_mut();
    // SAFETY: valid output pointer; no input allocation is required by the API.
    let result = unsafe { GetIpForwardTable2(AF_INET, &mut table) };
    if result != NO_ERROR {
        return Err(std::io::Error::from_raw_os_error(result as i32));
    }
    let table = RouteTable(table);
    let mut costs = HashMap::<u32, u64>::new();
    // SAFETY: on success Windows returns a table with NumEntries contiguous
    // rows. It remains allocated throughout this loop, including early returns.
    unsafe {
        let rows =
            std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize);
        for route in rows {
            if route.DestinationPrefix.PrefixLength != 0 || route.Loopback {
                continue;
            }
            let mut interface = MIB_IPINTERFACE_ROW {
                Family: AF_INET,
                InterfaceIndex: route.InterfaceIndex,
                ..Default::default()
            };
            // An adapter may disappear between enumeration and this lookup.
            if GetIpInterfaceEntry(&mut interface) != NO_ERROR || !interface.Connected {
                continue;
            }
            let cost = u64::from(route.Metric) + u64::from(interface.Metric);
            costs
                .entry(route.InterfaceIndex)
                .and_modify(|previous| *previous = (*previous).min(cost))
                .or_insert(cost);
        }
    }
    Ok(costs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(index: u32, address: &str, up: bool) -> if_addrs::Interface {
        if_addrs::Interface {
            name: format!("synthetic-{index}"),
            adapter_name: format!("synthetic-{index}"),
            index: Some(index),
            oper_status: if up {
                if_addrs::IfOperStatus::Up
            } else {
                if_addrs::IfOperStatus::Down
            },
            addr: if_addrs::IfAddr::V4(if_addrs::Ifv4Addr {
                ip: address.parse().unwrap(),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                prefixlen: 24,
                broadcast: None,
            }),
        }
    }

    #[test]
    fn public_internet_route_does_not_hide_private_lan() {
        let interfaces = [
            interface(1, "203.0.113.10", true),
            interface(2, "192.168.10.2", true), // isolated virtual switch
            interface(3, "192.168.20.2", true), // NAS network
            interface(4, "192.168.30.2", true), // Wi-Fi
        ];
        let costs = HashMap::from([(1, 25), (3, 271), (4, 30)]);
        assert_eq!(
            select_address(&interfaces, &costs),
            Some(Ipv4Addr::new(192, 168, 30, 2))
        );
        let reversed = interfaces.into_iter().rev().collect::<Vec<_>>();
        assert_eq!(
            select_address(&reversed, &costs),
            Some(Ipv4Addr::new(192, 168, 30, 2))
        );
    }

    #[test]
    fn disconnected_and_public_interfaces_are_rejected() {
        let interfaces = [
            interface(1, "192.168.10.2", false),
            interface(2, "203.0.113.10", true),
            interface(3, "127.0.0.1", true),
            interface(4, "0.0.0.0", true),
        ];
        assert_eq!(select_address(&interfaces, &HashMap::new()), None);
    }

    #[test]
    fn offline_lan_works_and_private_is_preferred_over_link_local() {
        let interfaces = [
            interface(1, "169.254.10.2", true),
            interface(2, "10.0.0.2", true),
        ];
        assert_eq!(
            select_address(&interfaces, &HashMap::new()),
            Some(Ipv4Addr::new(10, 0, 0, 2))
        );
        assert_eq!(
            select_address(&interfaces[..1], &HashMap::new()),
            Some(Ipv4Addr::new(169, 254, 10, 2))
        );
    }
}
