//! Cloudflare R2 through its S3 API: just enough to put a file into a bucket (signed with AWS
//! Signature Version 4) and check whether it's there. Used to give Discord a web address for
//! a track's own cover.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use ring::{digest, hmac};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The Cloudflare account ID (in the R2 overview).
    pub account_id: String,
    /// An R2 API token's S3 credentials.
    pub access_key_id: String,
    pub secret_access_key: String,
    pub bucket: String,
    /// Where the bucket is public: its r2.dev address or a custom domain.
    pub public_url: String,
}

impl Config {
    pub fn ready(&self) -> bool {
        [&self.account_id, &self.access_key_id, &self.secret_access_key, &self.bucket, &self.public_url].iter().all(|s| !s.trim().is_empty())
    }

    fn endpoint(&self) -> String {
        std::env::var("NODEPLAYER_R2_ENDPOINT").unwrap_or_else(|_| format!("https://{}.r2.cloudflarestorage.com", self.account_id.trim()))
    }

    /// The object's public address.
    pub fn public(&self, key: &str) -> String {
        let base = self.public_url.trim().trim_end_matches('/');
        let base = if base.contains("://") { base.to_string() } else { format!("https://{base}") };
        format!("{base}/{key}")
    }

    /// Whether `key` is in the bucket already.
    pub fn exists(&self, key: &str) -> Result<bool> {
        let resp = self.request("HEAD", key, &[], None)?;
        match resp.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            s => bail!("R2: HTTP {s}"),
        }
    }

    pub fn put(&self, key: &str, body: &[u8], content_type: &str) -> Result<()> {
        let mut resp = self.request("PUT", key, body, Some(content_type))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            // S3 errors are XML: <Error><Code>…</Code><Message>…</Message></Error>.
            let tag = |t: &str| text.split(&format!("<{t}>")).nth(1).and_then(|r| r.split(&format!("</{t}>")).next()).map(str::to_string);
            let msg = tag("Message").or_else(|| tag("Code")).unwrap_or_default();
            bail!("R2: HTTP {status} {msg}");
        }
        Ok(())
    }

    fn request(&self, method: &str, key: &str, body: &[u8], content_type: Option<&str>) -> Result<ureq::http::Response<ureq::Body>> {
        let endpoint = self.endpoint();
        let host = endpoint.split_once("://").map_or(endpoint.as_str(), |(_, h)| h).trim_end_matches('/');
        let path = format!("/{}/{}", encode(self.bucket.trim()), key.split('/').map(encode).collect::<Vec<_>>().join("/"));
        let hash = hex(digest::digest(&digest::SHA256, body).as_ref());
        let amz_date = amz_date(SystemTime::now());
        let mut headers = vec![("host", host.to_string()), ("x-amz-content-sha256", hash.clone()), ("x-amz-date", amz_date.clone())];
        if let Some(t) = content_type {
            headers.push(("content-type", t.to_string()));
        }
        let auth = authorization(self.access_key_id.trim(), self.secret_access_key.trim(), "auto", method, &path, &mut headers, &hash, &amz_date);
        let agent: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).http_status_as_error(false).build().into();
        let url = format!("{}{path}", endpoint.trim_end_matches('/'));
        let mut req = ureq::http::Request::builder().method(method).uri(url).header("authorization", auth);
        for (k, v) in &headers {
            if *k != "host" {
                req = req.header(*k, v);
            }
        }
        let resp = if method == "PUT" { agent.run(req.body(body.to_vec())?)? } else { agent.run(req.body(())?)? };
        Ok(resp)
    }
}

/// RFC 3986 percent-encoding of one path segment, as SigV4 wants it.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes()).as_ref().to_vec()
}

/// "20130524T000000Z".
fn amz_date(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// The Authorization header for a request whose `headers` (lowercase names) are all signed.
#[allow(clippy::too_many_arguments)]
fn authorization(
    access_key: &str,
    secret: &str,
    region: &str,
    method: &str,
    path: &str,
    headers: &mut [(&str, String)],
    payload_hash: &str,
    amz_date: &str,
) -> String {
    headers.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
    let signed = signed.join(";");
    let request = format!("{method}\n{path}\n\n{canonical_headers}\n{signed}\n{payload_hash}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(digest::digest(&digest::SHA256, request.as_bytes()).as_ref()));
    let key = hmac(&hmac(&hmac(&hmac(format!("AWS4{secret}").as_bytes(), date), region), "s3"), "aws4_request");
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={}", hex(&hmac(&key, &to_sign)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "GET Object" example from AWS's Signature Version 4 documentation.
    #[test]
    fn signs_like_aws() {
        let empty = hex(digest::digest(&digest::SHA256, b"").as_ref());
        let mut headers = vec![
            ("host", "examplebucket.s3.amazonaws.com".to_string()),
            ("range", "bytes=0-9".to_string()),
            ("x-amz-content-sha256", empty.clone()),
            ("x-amz-date", "20130524T000000Z".to_string()),
        ];
        let auth = authorization(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "GET",
            "/test.txt",
            &mut headers,
            &empty,
            "20130524T000000Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn dates_and_names() {
        assert_eq!(amz_date(UNIX_EPOCH + Duration::from_secs(1_369_353_600)), "20130524T000000Z");
        assert_eq!(amz_date(UNIX_EPOCH + Duration::from_secs(951_782_400 + 3661)), "20000229T010101Z");
        assert_eq!(encode("a b/ü"), "a%20b%2F%C3%BC");
        let c = Config { public_url: "pub-1.r2.dev/".into(), ..Default::default() };
        assert_eq!(c.public("covers/x.jpg"), "https://pub-1.r2.dev/covers/x.jpg");
    }
}
