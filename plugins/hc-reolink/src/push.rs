//! Reolink's push: events the moment they happen (Baichuan, TCP port 9000).
//!
//! The same protocol Reolink's own apps speak. One connection per address:
//! sign in, subscribe (command 31 on channel 251), and from then on the
//! device sends an `AlarmEventList` (command 33) whenever what a camera sees
//! changes: motion (`MD`), the doorbell (`visitor`), and the AI types
//! (`people`, `vehicle`, `dog_cat`, `package`). The connection is kept with
//! a link message (command 93) when nothing has come for a while.
//!
//! Messages are a header — `f0debc0a`, the command, the body's length, the
//! channel and a message number, a status and a class — then a body of XML.
//! Signing in is XOR'd with Reolink's fixed key; after it, bodies are
//! AES-128-CFB with a key from the session's nonce and the password. The
//! Python library reolink_aio (MIT) was the reference for all of it.

use aes::cipher::{AsyncStreamCipher, KeyIvInit};
use anyhow::{anyhow, bail, Context, Result};
use md5::{Digest, Md5};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAGIC: [u8; 4] = [0xf0, 0xde, 0xbc, 0x0a];
const XML_KEY: [u8; 8] = [0x1f, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0xff];
const AES_IV: &[u8; 16] = b"0123456789abcdef";
/// The channel a message is about: 250 the device itself, 251 its events.
const HOST: u8 = 250;
const EVENTS: u8 = 251;
pub const LOGIN: u32 = 1;
pub const SUBSCRIBE: u32 = 31;
pub const ALARM: u32 = 33;
pub const LINK: u32 = 93;
/// Keep the connection with a link message when nothing has come for this long.
pub const KEEPALIVE: Duration = Duration::from_secs(30);
/// No message at all for this long: the connection is gone.
pub const SILENT: Duration = Duration::from_secs(90);
/// A message larger than this isn't one of ours.
const MAX_BODY: usize = 1 << 20;

#[cfg(test)]
type AesCfbEnc = cfb_mode::Encryptor<aes::Aes128>;
type AesCfbDec = cfb_mode::Decryptor<aes::Aes128>;

/// Reolink's XOR cipher, the same both ways. `offset` is the message's channel byte.
pub fn xor(buf: &[u8], offset: u8) -> Vec<u8> {
    buf.iter()
        .enumerate()
        .map(|(i, b)| b ^ (XML_KEY[(offset as usize + i) % 8] ^ offset))
        .collect()
}

/// MD5 as Reolink writes it: hex, upper case, its first 31 characters.
pub fn md5_31(s: &str) -> String {
    let hex: String = Md5::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex[..31].to_uppercase()
}

/// The session's AES key: from the nonce the device gave and the password.
pub fn aes_key(nonce: &str, password: &str) -> [u8; 16] {
    let k = md5_31(&format!("{nonce}-{password}"));
    let mut out = [0u8; 16];
    out.copy_from_slice(&k.as_bytes()[..16]);
    out
}

/// The other way (the plugin sends no encrypted bodies; its tests do).
#[cfg(test)]
pub fn aes_encrypt(key: &[u8; 16], body: &[u8]) -> Vec<u8> {
    let mut buf = body.to_vec();
    AesCfbEnc::new(key.into(), AES_IV.into()).encrypt(&mut buf);
    buf
}

pub fn aes_decrypt(key: &[u8; 16], body: &[u8]) -> Vec<u8> {
    let mut buf = body.to_vec();
    AesCfbDec::new(key.into(), AES_IV.into()).decrypt(&mut buf);
    buf
}

/// A message's header, as sent: the modern 24-byte form (class 1464) with a
/// status of 0, or the legacy 20-byte one (class 1465) for asking the nonce.
pub fn header(cmd: u32, len: u32, channel: u8, number: u32, legacy: bool) -> Vec<u8> {
    let mut h = MAGIC.to_vec();
    h.extend(cmd.to_le_bytes());
    h.extend(len.to_le_bytes());
    h.push(channel);
    h.extend(&number.to_le_bytes()[..3]);
    if legacy {
        h.extend([0x12, 0xdc, 0x14, 0x65]);
    } else {
        h.extend([0x00, 0x00, 0x14, 0x64]);
        // Where a binary payload would start after the XML: none, so 0 (as
        // Reolink's apps send it; the body's length here is refused).
        h.extend(0u32.to_le_bytes());
    }
    h
}

/// A message received.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub cmd: u32,
    pub channel: u8,
    /// HTTP-like: 200 is fine. The 20-byte header has none and reads 200.
    pub status: u16,
    /// The encryption the 20-byte header names (`01dd` XOR, `02dd` AES, `00dd` none).
    pub enc: [u8; 2],
    pub long_header: bool,
    pub body: Vec<u8>,
}

impl Message {
    /// Its body as XML text: AES with the session's key, else XOR, else as it is.
    pub fn text(&self, key: Option<&[u8; 16]>) -> String {
        if self.body.is_empty() {
            return String::new();
        }
        let plain = |b: &[u8]| String::from_utf8_lossy(b).trim_start().starts_with("<?xml");
        if let Some(k) = key {
            let d = aes_decrypt(k, &self.body);
            if plain(&d) {
                return String::from_utf8_lossy(&d).into_owned();
            }
        }
        let d = xor(&self.body, self.channel);
        if plain(&d) {
            return String::from_utf8_lossy(&d).into_owned();
        }
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Read one message: its header (20 or 24 bytes, by its class), then its body.
pub async fn read_message<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Message> {
    let mut h = [0u8; 20];
    r.read_exact(&mut h).await?;
    if h[..4] != MAGIC {
        bail!("not a Reolink message");
    }
    let cmd = u32::from_le_bytes(h[4..8].try_into().expect("4 bytes"));
    let len = u32::from_le_bytes(h[8..12].try_into().expect("4 bytes")) as usize;
    let channel = h[12];
    let class = [h[18], h[19]];
    let long_header = matches!(class, [0x14, 0x64] | [0x00, 0x00]);
    let status = if long_header {
        let mut more = [0u8; 4];
        r.read_exact(&mut more).await?;
        u16::from_le_bytes([h[16], h[17]])
    } else {
        200
    };
    if len > MAX_BODY {
        bail!("a message of {len} bytes");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Message {
        cmd,
        channel,
        status,
        enc: [h[16], h[17]],
        long_header,
        body,
    })
}

/// The text of the first `<tag>…</tag>` in `xml`.
pub fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))? + start;
    Some(xml[start..end].trim())
}

/// What one camera sees now, as an event says it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Seen {
    pub channel: u32,
    pub motion: bool,
    pub person: bool,
    pub vehicle: bool,
    pub animal: bool,
    pub package: bool,
    pub visitor: bool,
}

/// An `AlarmEventList`: each camera it names, and what it sees now.
pub fn parse_alarms(xml: &str) -> Vec<Seen> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<AlarmEvent") {
        let after = &rest[i..];
        // `<AlarmEventList` starts with the same letters: skip it.
        if let Some(list) = after.strip_prefix("<AlarmEventList") {
            rest = list;
            continue;
        }
        let end = after.find("</AlarmEvent>").map_or(after.len(), |e| e + 13);
        let ev = &after[..end];
        rest = &after[end..];
        let Some(channel) = tag(ev, "channelId").and_then(|c| c.parse().ok()) else {
            continue;
        };
        let status = tag(ev, "status").unwrap_or_default();
        let ai: Vec<&str> = tag(ev, "AItype")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .collect();
        let has = |t: &str| ai.contains(&t);
        out.push(Seen {
            channel,
            // Some firmware says AI-only motion as `other`.
            motion: status.split(',').any(|s| s.trim() == "MD") || has("other"),
            visitor: status.split(',').any(|s| s.trim() == "visitor"),
            person: has("people"),
            vehicle: has("vehicle"),
            animal: has("dog_cat"),
            package: has("package"),
        });
    }
    out
}

fn login_xml(user_hash: &str, pass_hash: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LoginUser version=\"1.1\">\n<userName>{user_hash}</userName>\n<password>{pass_hash}</password>\n<userVer>1</userVer>\n</LoginUser>\n<LoginNet version=\"1.1\">\n<type>LAN</type>\n<udpPort>0</udpPort>\n</LoginNet>\n</body>\n"
    )
}

/// A signed-in, subscribed push connection.
pub struct Push {
    stream: TcpStream,
    key: [u8; 16],
    number: u32,
}

impl Push {
    /// Connect, sign in and subscribe. A refused sign-in is said as such.
    pub async fn connect(host: &str, port: u16, user: &str, password: &str) -> Result<Self> {
        let addr = format!("{host}:{port}");
        let mut stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addr))
            .await
            .map_err(|_| anyhow!("no answer on {addr}"))?
            .with_context(|| format!("can't reach {addr}"))?;
        // The nonce: a bare legacy header asks for it.
        stream.write_all(&header(LOGIN, 0, HOST, 1, true)).await?;
        let nonce = loop {
            let m = timeout(read_message(&mut stream)).await?;
            if m.cmd == LOGIN {
                let t = m.text(None);
                break tag(&t, "nonce")
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("no nonce in its answer"))?;
            }
        };
        let body = xor(
            login_xml(
                &md5_31(&format!("{user}{nonce}")),
                &md5_31(&format!("{password}{nonce}")),
            )
            .as_bytes(),
            HOST,
        );
        stream
            .write_all(&[header(LOGIN, body.len() as u32, HOST, 2, false), body].concat())
            .await?;
        loop {
            let m = timeout(read_message(&mut stream)).await?;
            if m.cmd != LOGIN {
                continue;
            }
            match m.status {
                200 => break,
                401 => bail!(crate::api::LoginRefused(crate::api::ApiError {
                    rsp_code: 401,
                    detail: "push sign-in refused".into(),
                })),
                s => bail!("push sign-in failed ({s})"),
            }
        }
        let mut p = Self {
            stream,
            key: aes_key(&nonce, password),
            number: 2,
        };
        p.send(SUBSCRIBE, EVENTS).await?;
        Ok(p)
    }

    async fn send(&mut self, cmd: u32, channel: u8) -> Result<()> {
        self.number = (self.number + 1) % 0x00ff_ffff;
        self.stream
            .write_all(&header(cmd, 0, channel, self.number, false))
            .await?;
        Ok(())
    }

    /// The next events, sending a link message when it's been quiet. An error
    /// is the connection gone.
    pub async fn next(&mut self) -> Result<Vec<Seen>> {
        let mut quiet = Duration::ZERO;
        loop {
            match tokio::time::timeout(KEEPALIVE, read_message(&mut self.stream)).await {
                Ok(m) => {
                    let m = m?;
                    if m.cmd == ALARM {
                        return Ok(parse_alarms(&m.text(Some(&self.key))));
                    }
                    quiet = Duration::ZERO;
                }
                Err(_) => {
                    quiet += KEEPALIVE;
                    if quiet >= SILENT {
                        bail!("nothing from it for {} s", quiet.as_secs());
                    }
                    self.send(LINK, HOST).await?;
                }
            }
        }
    }
}

async fn timeout<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .map_err(|_| anyhow!("no answer"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values from Node's crypto, an independent implementation.
    #[test]
    fn hashes_and_keys_as_reolink_does() {
        assert_eq!(md5_31("admin0Gpkqv7r"), "8623EB35F43D8F41FAEB00F220C91DD");
        assert_eq!(
            md5_31("standin-pass0Gpkqv7r"),
            "1E068EF027CC1A30ABC25F745BE395D"
        );
        assert_eq!(&aes_key("0Gpkqv7r", "standin-pass"), b"F3FBEB65F8D35444");
    }

    #[test]
    fn aes_cfb_matches_and_goes_both_ways() {
        let k = aes_key("0Gpkqv7r", "standin-pass");
        let e = aes_encrypt(&k, b"<?xml version=\"1.0\"?><body/>");
        let hex: String = e.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "1c192605ad059e07f97b13b2c5580bc6767f4b5feebb3dfadb8f25ea"
        );
        assert_eq!(aes_decrypt(&k, &e), b"<?xml version=\"1.0\"?><body/>");
    }

    #[test]
    fn xor_matches_and_goes_both_ways() {
        assert_eq!(xor(b"<?xml", 250), vec![0xfa, 0x8e, 0xd8, 0xfe, 0xee]);
        assert_eq!(xor(&xor(b"<body/>", 3), 3), b"<body/>");
    }

    #[test]
    fn writes_both_headers() {
        let h = header(SUBSCRIBE, 0, EVENTS, 7, false);
        assert_eq!(h.len(), 24);
        assert_eq!(&h[..4], &MAGIC);
        assert_eq!(&h[4..8], &31u32.to_le_bytes());
        assert_eq!(h[12], 251);
        assert_eq!(&h[16..20], &[0, 0, 0x14, 0x64]);
        assert_eq!(&header(LOGIN, 300, HOST, 2, false)[20..24], &[0, 0, 0, 0]);
        assert_eq!(header(LOGIN, 0, HOST, 0, true).len(), 20);
    }

    #[tokio::test]
    async fn reads_a_message_and_its_text() {
        let k = aes_key("n", "p");
        let xml = "<?xml version=\"1.0\"?><body><AlarmEventList><AlarmEvent><channelId>2</channelId><status>MD</status><AItype>people,vehicle</AItype></AlarmEvent></AlarmEventList></body>";
        let body = aes_encrypt(&k, xml.as_bytes());
        let mut bytes = header(ALARM, body.len() as u32, EVENTS, 1, false);
        bytes[16] = 200;
        bytes.extend(&body);
        let m = read_message(&mut bytes.as_slice()).await.unwrap();
        assert_eq!((m.cmd, m.status, m.long_header), (ALARM, 200, true));
        let seen = parse_alarms(&m.text(Some(&k)));
        assert_eq!(
            seen,
            vec![Seen {
                channel: 2,
                motion: true,
                person: true,
                vehicle: true,
                ..Default::default()
            }]
        );
    }

    #[test]
    fn reads_what_each_camera_sees() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" ?>
<body>
<AlarmEventList version="1.1">
<AlarmEvent version="1.1">
<channelId>0</channelId>
<status>MD,visitor</status>
<AItype>none</AItype>
<recording>0</recording>
</AlarmEvent>
<AlarmEvent version="1.1">
<channelId>1</channelId>
<status>none</status>
<AItype>other,dog_cat,package</AItype>
</AlarmEvent>
</AlarmEventList>
</body>"#;
        let s = parse_alarms(xml);
        assert_eq!(s.len(), 2);
        assert!(s[0].motion && s[0].visitor && !s[0].person);
        assert!(s[1].motion && s[1].animal && s[1].package && !s[1].visitor);
        assert_eq!(s[1].channel, 1);
        // Quiet: a camera that sees nothing now.
        let q = parse_alarms("<AlarmEventList><AlarmEvent><channelId>3</channelId><status>none</status><AItype>none</AItype></AlarmEvent></AlarmEventList>");
        assert_eq!(
            q,
            vec![Seen {
                channel: 3,
                ..Default::default()
            }]
        );
    }
}
