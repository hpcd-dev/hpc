use std::{io, net::SocketAddr};
use thiserror::Error as ThisError;
use tokio::net::lookup_host;

#[derive(ThisError, Debug)]
pub enum NetError {
    #[error("DNS name not found for {0}")]
    DnsNotFound(String), // should contain hostname inside

    #[error("couldn't resolve host: {0:?}")]
    Resolve(io::Error), // will contain io::Error inside

    #[error("no addreses resolved from {0}")]
    NoAddrs(String), // should contain hostname inside
}

pub async fn lookup_first_addr(host: &str, port: u16) -> Result<SocketAddr, NetError> {
    // 1) Map lookup errors to domain errors
    let mut addrs = lookup_host((host, port))
        .await
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => NetError::DnsNotFound(host.to_owned()),
            _ => NetError::Resolve(e),
        })?;

    // 2) Take the first resolved address, or error if none
    addrs
        .next()
        .ok_or_else(|| NetError::NoAddrs(host.to_owned()))
}
