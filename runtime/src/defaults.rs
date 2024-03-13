use core::{num::NonZeroU16, time::Duration};

use bytesize::ByteSize;
use nonzero_ext::nonzero;
use p2p::NetworkConfig;

pub const DEFAULT_ETH1_DB_SIZE: ByteSize = ByteSize::gib(16);
pub const DEFAULT_ETH2_DB_SIZE: ByteSize = ByteSize::gib(256);
pub const DEFAULT_METRICS_PORT: u16 = 5054;
pub const DEFAULT_LIBP2P_IPV4_PORT: NonZeroU16 = nonzero!(9000_u16);
pub const DEFAULT_LIBP2P_IPV6_PORT: NonZeroU16 = nonzero!(9050_u16);
pub const DEFAULT_LIBP2P_QUIC_IPV4_PORT: NonZeroU16 = nonzero!(9001_u16);
pub const DEFAULT_LIBP2P_QUIC_IPV6_PORT: NonZeroU16 = nonzero!(9051_u16);
pub const DEFAULT_REQUEST_TIMEOUT: u64 = 30000;
pub const DEFAULT_TARGET_PEERS: usize = 100;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[must_use]
pub fn default_network_config() -> NetworkConfig {
    let mut config = NetworkConfig::default();
    config.identify_agent_version = Some(grandine_version::version_with_platform());
    config.enr_udp4_port = Some(DEFAULT_LIBP2P_IPV4_PORT);
    config
}
