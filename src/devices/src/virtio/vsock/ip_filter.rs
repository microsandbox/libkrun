use ipnetwork::Ipv4Network;
use std::net::Ipv4Addr;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Configuration for IP-based filtering in the Vsock Muxer.
#[derive(Clone, Debug)]
pub struct IpFilterConfig {
    /// Defines the scope of allowed connections/bindings.
    /// 0: None (Block all IP communication)
    /// 1: Group (Allow within `subnet` if specified, otherwise behaves like scope 0)
    /// 2: Public (Allow public IPs, bind only to `ip` if specified)
    /// 3: Any (Allow any IP, bind only to `ip` if specified)
    pub scope: u8,

    /// If specified, binding/listening is ONLY allowed on this specific IP address
    /// (ignored if scope is 0).
    pub ip: Option<Ipv4Addr>,

    /// The allowed subnet for Scope 1 (Group). Optional - if not provided when scope is 1,
    /// all connections will be blocked (same as scope 0).
    pub subnet: Option<Ipv4Network>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl IpFilterConfig {
    /// Checks if the configuration is logically valid.
    pub fn is_valid(&self) -> bool {
        match self.scope {
            0 | 1 | 2 | 3 => true, // All valid scopes (subnet is optional for scope 1)
            _ => false,            // Invalid scope number
        }
    }

    /// Checks if an IP address is considered private.
    /// (Includes loopback, private ranges, link-local, broadcast, documentation, shared CGN)
    fn is_private(ip: Ipv4Addr) -> bool {
        ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_broadcast()
            || ip.is_documentation()
            || match ip.octets() {
                [100, b, _, _] if b >= 64 && b <= 127 => true, // Shared Address Space (RFC 6598)
                _ => false,
            }
    }

    /// Checks if connecting to a given destination IP is allowed by the filter rules.
    pub fn is_allowed_connect(&self, dest_ip: Ipv4Addr) -> bool {
        match self.scope {
            0 => false, // Scope 0: Deny all connections
            1 => {
                // Scope 1: Group - Allow connection only if dest_ip is within the specified subnet
                // If no subnet is specified, behaves like scope 0 (deny all)
                self.subnet.map_or(false, |subnet| subnet.contains(dest_ip))
            }
            2 => {
                // Scope 2: Public - Allow connection only if dest_ip is NOT private
                !Self::is_private(dest_ip)
            }
            3 => true,  // Scope 3: Any - Allow connection to any IP
            _ => false, // Invalid scope
        }
    }

    /// Checks if binding to a given IP is allowed by the filter rules.
    pub fn is_allowed_bind(&self, bind_ip: Ipv4Addr) -> bool {
        if self.scope == 0 {
            return false; // Scope 0: Deny all binding
        }

        // Rule: "if ip specified, only the ip can be bound to or listened on."
        if let Some(allowed_bind_ip) = self.ip {
            return bind_ip == allowed_bind_ip;
        }

        // No specific IP specified, check based on scope rules for the bind_ip itself
        match self.scope {
            // Scope 1: Group - Allow binding within the subnet if no specific IP given
            // If no subnet is specified, behaves like scope 0 (deny all)
            1 => self.subnet.map_or(false, |subnet| subnet.contains(bind_ip)),
            // Scope 2: Public - Allow binding to public IPs if no specific IP given
            2 => !Self::is_private(bind_ip),
            // Scope 3: Any - Allow binding to any IP if no specific IP given
            3 => true,
            _ => false, // Invalid scope (scope 0 already handled)
        }
    }
}
