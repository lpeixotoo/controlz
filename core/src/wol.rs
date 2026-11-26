use crate::{ControlError, Result};
use tokio::net::UdpSocket;

/// Sends a Wake-on-LAN magic packet. LG TVs need "Turn on via Wi-Fi"
/// (or "Mobile TV On") enabled in their settings for this to work.
pub async fn send_magic_packet(mac: &str) -> Result<()> {
    let bytes = parse_mac(mac)?;
    let mut packet = vec![0xFFu8; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }

    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(ControlError::network)?;
    socket.set_broadcast(true).map_err(ControlError::network)?;
    for port in [9, 7] {
        socket
            .send_to(&packet, ("255.255.255.255", port))
            .await
            .map_err(ControlError::network)?;
    }
    Ok(())
}

pub(crate) fn parse_mac(mac: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = mac.split([':', '-']).collect();
    if parts.len() != 6 {
        return Err(ControlError::invalid(format!("bad MAC address: {mac}")));
    }
    let mut out = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(part, 16)
            .map_err(|_| ControlError::invalid(format!("bad MAC address: {mac}")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mac() {
        let colon = "a8:23:fe:01:02:0F"; // privacy:allow (fake MAC)
        let dash = "A8-23-FE-01-02-0F"; // privacy:allow (fake MAC)
        assert_eq!(parse_mac(colon).unwrap(), [0xa8, 0x23, 0xfe, 1, 2, 0x0f]);
        assert_eq!(parse_mac(dash).unwrap(), [0xa8, 0x23, 0xfe, 1, 2, 0x0f]);
        assert!(parse_mac("a8:23:fe").is_err());
    }
}
