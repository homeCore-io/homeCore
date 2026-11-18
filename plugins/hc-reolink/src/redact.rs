//! Keeping secrets out of words people read. Errors end up in logs, alerts
//! and notices that every role can see, and a camera's URLs carry its session
//! token, or its user name and password (RTSP sources for go2rtc).

/// Query keys whose values are never shown.
const SECRET_KEYS: &[&str] = &[
    "token", "password", "passwd", "pass", "pwd", "user", "username", "auth",
];

/// `s` with credentials in it masked: the user and password of any
/// `scheme://user:pass@host` (plain or percent-encoded, as in a go2rtc
/// `src=` value), and the values of `token=`, `password=`, `user=` and the
/// like.
pub fn redact(s: &str) -> String {
    redact_query(&redact_userinfo(&redact_userinfo_encoded(s)))
}

/// `scheme://user:pass@` becomes `scheme://***@`.
fn redact_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        // The authority runs to the first `/`, `?`, `#` or whitespace.
        let end = tail
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The same inside a percent-encoded value: `%3A%2F%2Fuser%3Apass%40`.
fn redact_userinfo_encoded(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut from = 0;
    while let Some(i) = lower[from..].find("%3a%2f%2f") {
        let start = from + i + "%3a%2f%2f".len();
        out.push_str(&s[from..start]);
        let tail = &lower[start..];
        let end = tail
            .find(|c: char| c == '&' || c == '#' || c.is_whitespace())
            .unwrap_or(tail.len());
        let slash = tail[..end].find("%2f").unwrap_or(end);
        let authority = &tail[..slash];
        match authority.rfind("%40") {
            Some(at) => {
                out.push_str("***");
                out.push_str(&s[start + at..start + slash]);
            }
            None => out.push_str(&s[start..start + slash]),
        }
        from = start + slash;
    }
    out.push_str(&s[from..]);
    out
}

/// `token=abc` becomes `token=***`, for each secret key, in any case.
fn redact_query(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut copied = 0;
    while i < s.len() {
        let at_start = i == 0 || matches!(bytes[i - 1], b'?' | b'&' | b';' | b' ' | b'"');
        let key = at_start
            .then(|| {
                SECRET_KEYS
                    .iter()
                    .find(|k| lower[i..].starts_with(*k) && bytes.get(i + k.len()) == Some(&b'='))
            })
            .flatten();
        if let Some(k) = key {
            let v = i + k.len() + 1;
            let end = s[v..]
                .find(|c: char| c == '&' || c == '#' || c == '"' || c.is_whitespace())
                .map_or(s.len(), |e| v + e);
            out.push_str(&s[copied..v]);
            out.push_str("***");
            copied = end;
            i = end;
        } else {
            i += s[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out.push_str(&s[copied..]);
    out
}

/// A reqwest error in words, without its URL (which carries the token or
/// the camera's password): what went wrong and, if there was one, the status.
pub fn http_error(e: reqwest::Error) -> anyhow::Error {
    let e = e.without_url();
    anyhow::anyhow!(redact(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_a_token_in_a_query() {
        assert_eq!(
            redact("http://10.0.0.5/cgi-bin/api.cgi?cmd=Snap&channel=0&token=abc123def"),
            "http://10.0.0.5/cgi-bin/api.cgi?cmd=Snap&channel=0&token=***"
        );
        assert_eq!(
            redact("error sending request for url (https://cam/api.cgi?cmd=Login&TOKEN=x&rs=1)"),
            "error sending request for url (https://cam/api.cgi?cmd=Login&TOKEN=***&rs=1)"
        );
    }

    #[test]
    fn hides_a_user_and_password_in_a_query() {
        assert_eq!(
            redact("http://cam/cgi-bin/api.cgi?cmd=Snap&user=admin&password=s3cret&rs=9"),
            "http://cam/cgi-bin/api.cgi?cmd=Snap&user=***&password=***&rs=9"
        );
        // A key that only ends in a secret word is left alone.
        assert_eq!(redact("?cmd=x&nottoken=1"), "?cmd=x&nottoken=1");
    }

    #[test]
    fn hides_the_userinfo_of_an_rtsp_url() {
        assert_eq!(
            redact("go2rtc can't open rtsp://admin:p%40ss@10.0.0.5:554/h264Preview_01_sub"),
            "go2rtc can't open rtsp://***@10.0.0.5:554/h264Preview_01_sub"
        );
        assert_eq!(
            redact("rtmp://admin:pw@cam/bcs/channel0_main.bcs and http://plain/x"),
            "rtmp://***@cam/bcs/channel0_main.bcs and http://plain/x"
        );
    }

    #[test]
    fn hides_the_userinfo_of_an_encoded_rtsp_source() {
        let u = crate::go2rtc::put_url(
            "http://127.0.0.1:1984",
            "reolink_nvr_1",
            "rtsp://admin:p%40ss@10.0.0.5:554/h264Preview_01_sub",
        );
        let r = redact(&u);
        assert!(!r.contains("admin"), "{r}");
        assert!(!r.contains("p%2540ss"), "{r}");
        assert_eq!(
            r,
            "http://127.0.0.1:1984/api/streams?name=reolink_nvr_1&src=rtsp%3A%2F%2F***%4010.0.0.5%3A554%2Fh264Preview_01_sub"
        );
    }

    #[test]
    fn leaves_plain_words_alone() {
        let s = "Front door didn't answer in 15 s. Is the address right?";
        assert_eq!(redact(s), s);
        assert_eq!(redact("http://10.0.0.5:80/api"), "http://10.0.0.5:80/api");
    }

    #[tokio::test]
    async fn a_reqwest_error_loses_its_url() {
        // Nothing listens on a reserved port of the loopback: no camera is asked.
        let err = reqwest::Client::new()
            .get("http://127.0.0.1:9/cgi-bin/api.cgi?cmd=Snap&token=topsecret&password=hunter2")
            .send()
            .await
            .unwrap_err();
        let shown = http_error(err);
        let s = format!("{shown:#} {shown:?}");
        assert!(!s.contains("topsecret"), "{s}");
        assert!(!s.contains("hunter2"), "{s}");
        assert!(!s.contains("api.cgi"), "{s}");
    }
}
