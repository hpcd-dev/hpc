use crate::state::db::Address;
use crate::util::net;
use crate::util::remote_path::normalize_path;
use proto::add_cluster_init;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use tonic::Status;

pub fn parse_add_cluster_host(
    host: Option<add_cluster_init::Host>,
) -> Result<Address, Status> {
    let host = match host {
        Some(v) => v,
        None => return Err(Status::invalid_argument("empty host in initial message")),
    };
    match host {
        add_cluster_init::Host::Hostname(v) => Ok(Address::Hostname(v)),
        add_cluster_init::Host::Ipaddr(addr) => {
            let ip: IpAddr = match addr.parse() {
                Ok(v) => v,
                Err(e) => {
                    return Err(Status::invalid_argument(format!(
                        "could not parse {} into ip address: {:?}",
                        addr, e
                    )));
                }
            };
            Ok(Address::Ip(ip))
        }
    }
}

pub fn parse_add_cluster_port(port: u32) -> Result<u16, Status> {
    match u16::try_from(port) {
        Ok(v) => Ok(v),
        Err(e) => {
            log::debug!("could not case u32 port to u16 port: {}", e);
            Err(Status::invalid_argument(format!(
                "invalid port value: {port}"
            )))
        }
    }
}

pub fn normalize_default_base_path(
    default_base_path: Option<String>,
) -> Result<Option<PathBuf>, Status> {
    let Some(base) = default_base_path else {
        return Ok(None);
    };
    let normalized = normalize_path(base);
    if normalized.is_absolute() {
        Ok(Some(normalized))
    } else {
        Err(Status::invalid_argument(format!(
            "default_base_path must be absolute, got: '{}'",
            normalized.to_string_lossy()
        )))
    }
}

pub async fn resolve_host_addr(
    addr: &Address,
    port: u16,
) -> Result<SocketAddr, Status> {
    match addr {
        Address::Ip(v) => Ok((*v, port).into()),
        Address::Hostname(hostname) => match net::lookup_first_addr(hostname, port).await {
            Ok(v) => Ok(v),
            Err(e) => Err(map_net_error(hostname, e)),
        },
    }
}

pub fn map_net_error(hostname: &str, err: net::NetError) -> Status {
    match err {
        net::NetError::DnsNotFound(_) => Status::invalid_argument(format!(
            "hostname {hostname} could not be resolved"
        )),
        net::NetError::NoAddrs(_) => Status::invalid_argument(format!(
            "couldn't find any IP addresses for {hostname}"
        )),
        net::NetError::Resolve(h) => {
            Status::internal(format!("encountered error when resolving {hostname}: {h}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_add_cluster_host_validates() {
        assert!(parse_add_cluster_host(None).is_err());

        let host = parse_add_cluster_host(Some(add_cluster_init::Host::Hostname(
            "example.com".to_string(),
        )))
        .unwrap();
        assert!(matches!(host, Address::Hostname(_)));

        let host = parse_add_cluster_host(Some(add_cluster_init::Host::Ipaddr(
            "127.0.0.1".to_string(),
        )))
        .unwrap();
        assert!(matches!(host, Address::Ip(_)));

        let err = parse_add_cluster_host(Some(add_cluster_init::Host::Ipaddr(
            "not-an-ip".to_string(),
        )))
        .unwrap_err();
        assert!(err.message().contains("could not parse not-an-ip into ip address"));
    }

    #[test]
    fn parse_add_cluster_port_validates() {
        assert_eq!(parse_add_cluster_port(22).unwrap(), 22u16);
        let err = parse_add_cluster_port(u32::from(u16::MAX) + 1).unwrap_err();
        assert_eq!(err.message(), "invalid port value: 65536");
    }

    #[test]
    fn normalize_default_base_path_validates() {
        let ok = normalize_default_base_path(Some("/tmp/base".to_string())).unwrap();
        assert!(ok.unwrap().is_absolute());

        let err = normalize_default_base_path(Some("relative/path".to_string())).unwrap_err();
        assert!(err.message().contains("default_base_path must be absolute"));
    }

    #[test]
    fn map_net_error_keeps_messages() {
        let err = map_net_error(
            "example.com",
            net::NetError::DnsNotFound("example.com".to_string()),
        );
        assert_eq!(err.message(), "hostname example.com could not be resolved");

        let err = map_net_error(
            "example.com",
            net::NetError::NoAddrs("example.com".to_string()),
        );
        assert_eq!(
            err.message(),
            "couldn't find any IP addresses for example.com"
        );

        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        let err = map_net_error("example.com", net::NetError::Resolve(io_err));
        assert!(err.message().contains("encountered error when resolving example.com"));
    }
}
