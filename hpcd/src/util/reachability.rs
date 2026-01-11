// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::state::db::Address;
use crate::util::net::{self, NetError};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

const CONNECT_TIMEOUT_SECS: u64 = 3;

pub async fn check_host_reachable(address: &Address, port: u16) -> Result<bool, NetError> {
    let timeout = Duration::from_secs(CONNECT_TIMEOUT_SECS);
    match address {
        Address::Ip(ip) => Ok(is_socket_reachable(SocketAddr::new(*ip, port), timeout).await),
        Address::Hostname(host) => {
            let addrs = net::lookup_addrs(host, port).await?;
            for addr in addrs {
                if is_socket_reachable(addr, timeout).await {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

async fn is_socket_reachable(addr: SocketAddr, timeout_duration: Duration) -> bool {
    match timeout(timeout_duration, TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => true,
        _ => false,
    }
}
