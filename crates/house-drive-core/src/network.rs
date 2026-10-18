//! The home network gate: away from home, a machine neither seeks nor offers the share.
//!
//! The current network must be on Windows' Private profile and carry the network ID recorded at
//! `join` (Windows' Network List Manager). A network's name alone is not trusted: anyone can give
//! a network the same name. Encryption and the dedicated account remain the real protection.

use windows::Win32::Networking::NetworkListManager::{
    INetworkListManager, NLM_ENUM_NETWORK_CONNECTED, NLM_NETWORK_CATEGORY_PRIVATE, NetworkListManager,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::GUID;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    /// Windows' ID for the network, `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.
    pub id: String,
    pub name: String,
    pub private: bool,
}

/// The networks this machine is connected to now.
pub fn connected_networks() -> Result<Vec<Network>, String> {
    let error = |error: windows::core::Error| {
        return format!("Network List Manager: {error}");
    };
    let mut found = Vec::new();
    // SAFETY: plain COM calls on interfaces the calls themselves return; COM is initialised for
    // this thread first (a repeated initialisation is harmless and ignored).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let manager: INetworkListManager =
            CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL).map_err(error)?;
        let networks = manager.GetNetworks(NLM_ENUM_NETWORK_CONNECTED).map_err(error)?;
        loop {
            let mut slot = [None];
            let mut fetched = 0u32;
            networks.Next(&mut slot, Some(&mut fetched as *mut u32)).map_err(error)?;
            if fetched == 0 {
                break;
            }
            let Some(network) = slot[0].take() else {
                break;
            };
            found.push(Network {
                id: format_guid(&network.GetNetworkId().map_err(error)?),
                name: network.GetName().map(|name| name.to_string()).unwrap_or_default(),
                private: network.GetCategory().map_err(error)? == NLM_NETWORK_CATEGORY_PRIVATE,
            });
        }
    }
    return Ok(found);
}

/// True when one of `networks` is the home network: Private, with the recorded ID.
pub fn on_home_network(networks: &[Network], home_network_id: &str) -> bool {
    let home = home_network_id.trim();
    return !home.is_empty() && networks.iter().any(|network| network.private && network.id.eq_ignore_ascii_case(home));
}

pub fn format_guid(guid: &GUID) -> String {
    let d = guid.data4;
    return format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1, guid.data2, guid.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "{00112233-4455-6677-8899-AABBCCDDEEFF}";

    fn network(id: &str, private: bool) -> Network {
        return Network { id: id.to_string(), name: "a name".to_string(), private };
    }

    #[test]
    fn formats_like_windows() {
        let guid = GUID { data1: 0x00112233, data2: 0x4455, data3: 0x6677, data4: [0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF] };
        assert_eq!(format_guid(&guid), HOME);
    }

    #[test]
    fn the_gate_needs_the_id_and_the_private_profile() {
        assert!(on_home_network(&[network(HOME, true)], HOME));
        assert!(on_home_network(&[network("{other}", true), network(HOME, true)], &HOME.to_lowercase()));
        assert!(!on_home_network(&[network(HOME, false)], HOME));
        assert!(!on_home_network(&[network("{other}", true)], HOME));
        assert!(!on_home_network(&[], HOME));
        assert!(!on_home_network(&[network("", true)], ""));
    }

    #[test]
    fn listing_networks_works_here() {
        // Whatever this machine is connected to; the call itself must succeed.
        assert!(connected_networks().is_ok());
    }
}
