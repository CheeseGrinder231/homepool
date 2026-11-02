use crate::Result;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description};
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn empty() -> String {
    hex(&Sha256::digest([]))
}
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn mac(key: &[u8], message: &str) -> Vec<u8> {
    let mut h = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
    h.update(message.as_bytes());
    h.finalize().into_bytes().to_vec()
}
#[allow(clippy::too_many_arguments)]
pub fn headers(
    method: &str,
    path: &str,
    query: &str,
    host: &str,
    region: &str,
    access: &str,
    secret: &str,
    mime: &str,
    hash: &str,
) -> Result<String> {
    let format =
        format_description::parse_borrowed::<2>("[year][month][day]T[hour][minute][second]Z")
            .map_err(|_| "clock")?;
    let stamp = OffsetDateTime::now_utc()
        .format(&format)
        .map_err(|_| "clock")?;
    let day = &stamp[..8];
    let scope = format!("{day}/{region}/s3/aws4_request");
    let mut headers = vec![
        ("host", host),
        ("x-amz-content-sha256", hash),
        ("x-amz-date", &stamp),
    ];
    if !mime.is_empty() {
        headers.push(("content-type", mime));
    }
    headers.sort();
    let names = headers
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");
    let canonical = headers
        .iter()
        .map(|(k, v)| {
            format!(
                "{k}:{}\n",
                v.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect::<String>();
    let request = format!("{method}\n{path}\n{query}\n{canonical}\n{names}\n{hash}");
    let string = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex(&Sha256::digest(request.as_bytes()))
    );
    let key = mac(format!("AWS4{secret}").as_bytes(), day);
    let key = mac(&key, region);
    let key = mac(&key, "s3");
    let key = mac(&key, "aws4_request");
    let signature = hex(&mac(&key, &string));
    let mut out = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect::<String>();
    out.push_str(&format!("authorization: AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={names}, Signature={signature}\r\n"));
    Ok(out)
}
