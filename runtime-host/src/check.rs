//! Testing a key before it is saved: can this container reach homeCore, and
//! will the broker take the login?
//!
//! Both are checked because they fail separately and are fixed separately: a
//! wrong address in the key, a broker that listens only on homeCore's own
//! machine, a firewall, a reset login. The page shows whichever answer came
//! back, so each says what to do.

use anyhow::{anyhow, bail, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(8);

/// Log in to the broker once and log straight out.
///
/// A plain MQTT 3.1.1 CONNECT, so the answer is the broker's own: accepted,
/// refused, or nothing listening. The client id is not the runtime's, so the
/// check never knocks a running runtime off the broker.
pub async fn broker_login(host: &str, port: u16, username: &str, password: &str) -> Result<()> {
    let addr = format!("{host}:{port}");
    let mut stream = tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect(&addr))
        .await
        .map_err(|_| anyhow!("nothing answered at the broker, {addr}"))?
        .map_err(|e| anyhow!("couldn't reach the broker at {addr}: {e}"))?;

    stream
        .write_all(&connect_packet(
            &format!("{username}.keycheck"),
            username,
            password,
        ))
        .await?;
    let mut ack = [0u8; 4];
    match tokio::time::timeout(TIMEOUT, stream.read_exact(&mut ack)).await {
        Err(_) => bail!("the broker at {addr} didn't answer the login"),
        // rumqttd drops a refused login without a CONNACK.
        Ok(Err(_)) => bail!(
            "the broker at {addr} refused the login: the key was reset, or the runtime \
             was removed in homeCore"
        ),
        Ok(Ok(_)) => {}
    }
    let _ = stream.write_all(&[0xE0, 0x00]).await; // DISCONNECT
    match ack {
        [0x20, 0x02, _, 0] => Ok(()),
        [0x20, 0x02, _, 4 | 5] => bail!(
            "the broker at {addr} refused the login: the key was reset, or the runtime \
             was removed in homeCore"
        ),
        [0x20, 0x02, _, code] => bail!("the broker at {addr} refused the login (code {code})"),
        _ => bail!("something at {addr} answered, but not as an MQTT broker"),
    }
}

fn connect_packet(client_id: &str, username: &str, password: &str) -> Vec<u8> {
    fn string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut body = Vec::new();
    string(&mut body, "MQTT");
    body.push(4); // protocol level 3.1.1
    body.push(0b1100_0010); // username, password, clean session
    body.extend_from_slice(&10u16.to_be_bytes()); // keep alive
    string(&mut body, client_id);
    string(&mut body, username);
    string(&mut body, password);

    let mut packet = vec![0x10];
    let mut len = body.len();
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        packet.push(byte);
        if len == 0 {
            break;
        }
    }
    packet.extend_from_slice(&body);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connect_packet_is_well_formed() {
        let p = connect_packet("c", "u", "p");
        assert_eq!(p[0], 0x10);
        assert_eq!(p[1] as usize, p.len() - 2, "remaining length");
        assert_eq!(&p[2..8], &[0, 4, b'M', b'Q', b'T', b'T']);
        assert_eq!(p[8], 4);
        assert_eq!(p[9], 0b1100_0010);
    }

    #[test]
    fn a_long_password_gets_a_multi_byte_length() {
        let p = connect_packet("c", "u", &"x".repeat(200));
        assert!(
            p[1] & 0x80 != 0,
            "a body over 127 bytes needs two length bytes"
        );
        let len = (p[1] & 0x7f) as usize + (p[2] as usize) * 128;
        assert_eq!(len, p.len() - 3);
    }

    #[tokio::test]
    async fn nothing_listening_says_so() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = broker_login("127.0.0.1", port, "u", "p").await.unwrap_err();
        assert!(
            err.to_string().contains("couldn't reach the broker"),
            "{err}"
        );
    }
}
