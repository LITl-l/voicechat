use anyhow::Result;
use igd_next::{Gateway, PortMappingProtocol, SearchOptions};
use std::net::SocketAddr;

const LEASE_DURATION_SECS: u32 = 3600;
const DESCRIPTION: &str = "VoiceChat UDP";

/// Holds a UPnP port mapping and removes it on drop.
pub struct UpnpMapping {
    gateway: Gateway,
    external_port: u16,
}

impl UpnpMapping {
    /// Discover the UPnP gateway, add a UDP port mapping, and return
    /// the external address peers should connect to.
    pub fn setup(local_addr: SocketAddr) -> Result<(SocketAddr, Self)> {
        let opts = SearchOptions::default();
        let gateway =
            igd_next::search_gateway(opts).map_err(|e| anyhow::anyhow!("UPnP discovery: {e}"))?;

        let external_ip = gateway
            .get_external_ip()
            .map_err(|e| anyhow::anyhow!("UPnP external IP: {e}"))?;

        gateway
            .add_port(
                PortMappingProtocol::UDP,
                local_addr.port(),
                local_addr,
                LEASE_DURATION_SECS,
                DESCRIPTION,
            )
            .map_err(|e| anyhow::anyhow!("UPnP add port: {e}"))?;

        let external_addr = SocketAddr::new(external_ip, local_addr.port());
        log::info!("UPnP: mapped {external_addr} -> {local_addr}");

        Ok((
            external_addr,
            Self {
                gateway,
                external_port: local_addr.port(),
            },
        ))
    }
}

impl Drop for UpnpMapping {
    fn drop(&mut self) {
        match self
            .gateway
            .remove_port(PortMappingProtocol::UDP, self.external_port)
        {
            Ok(()) => log::info!("UPnP: removed port mapping for {}", self.external_port),
            Err(e) => log::warn!("UPnP: failed to remove mapping: {e}"),
        }
    }
}
