//! Fixed-bucket synchronous S3 data plane. No administration or arbitrary requests.
mod settings;
mod signing;
mod transport;
use serde::Deserialize;
use std::io::Seek;
use url::Url;
pub type Result<T> = std::result::Result<T, &'static str>;
pub struct Client {
    endpoint: Url,
    bucket: String,
    region: String,
    access: String,
    secret: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    pub key: String,
    pub size: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Page {
    #[serde(default)]
    pub contents: Vec<Item>,
    #[serde(default)]
    pub next_continuation_token: String,
}
impl Client {
    pub fn new(
        endpoint: &str,
        bucket: String,
        region: String,
        access: String,
        secret: String,
    ) -> Result<Self> {
        let endpoint = Url::parse(endpoint).map_err(|_| "endpoint")?;
        let host = endpoint.host_str().ok_or("endpoint")?;
        let local = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && local)
            || endpoint.path() != "/"
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || bucket.is_empty()
            || bucket.len() > 63
            || !bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || region.is_empty()
            || !region
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || access.is_empty()
            || !access.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err("configuration");
        }
        Ok(Self {
            endpoint,
            bucket,
            region,
            access,
            secret,
        })
    }
    pub fn list(&self, cursor: &str, limit: u32) -> Result<Page> {
        if limit == 0 || limit > 100 || cursor.len() > 1024 {
            return Err("list bounds");
        }
        let mut query = vec![
            ("list-type", "2".to_owned()),
            ("max-keys", limit.to_string()),
        ];
        if !cursor.is_empty() {
            query.push(("continuation-token", cursor.to_owned()));
        }
        let body = self.request("GET", "", query, None, "", signing::empty())?;
        let bytes = body.bytes(settings::LIST_LIMIT)?;
        let page: Page = quick_xml::de::from_reader(bytes.as_slice()).map_err(|_| "listing")?;
        if page.contents.len() > limit as usize || page.next_continuation_token.len() > 1024 {
            return Err("listing bounds");
        }
        Ok(page)
    }
    pub fn head(&self, key: &str) -> Result<Option<u64>> {
        match self.request("HEAD", key, vec![], None, "", signing::empty()) {
            Ok(body) => Ok(Some(body.length)),
            Err("missing") => Ok(None),
            Err(e) => Err(e),
        }
    }
    pub fn get(&self, key: &str) -> Result<transport::Body> {
        self.request("GET", key, vec![], None, "", signing::empty())
    }
    pub fn put(
        &self,
        key: &str,
        file: &mut std::fs::File,
        mime: &str,
        hash: &str,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        let length = file.metadata().map_err(|_| "spool")?.len();
        file.rewind().map_err(|_| "spool")?;
        self.request(
            "PUT",
            key,
            vec![],
            Some((file, length, cancelled)),
            mime,
            hash.to_owned(),
        )?
        .bytes(0)?;
        Ok(())
    }
    fn request(
        &self,
        method: &str,
        key: &str,
        query: Vec<(&str, String)>,
        data: Option<(&mut std::fs::File, u64, &std::sync::atomic::AtomicBool)>,
        mime: &str,
        hash: String,
    ) -> Result<transport::Body> {
        if key.contains(['\r', '\n', '?', '#'])
            || key.starts_with('/')
            || key.split('/').any(|p| p == "." || p == "..")
            || mime.bytes().any(|b| !(32..127).contains(&b))
        {
            return Err("request");
        }
        let path = format!(
            "/{}{}{}",
            self.bucket,
            if key.is_empty() { "" } else { "/" },
            key
        );
        let path = path
            .split('/')
            .map(signing::encode)
            .collect::<Vec<_>>()
            .join("/");
        let mut query = query
            .into_iter()
            .map(|(k, v)| (signing::encode(k), signing::encode(&v)))
            .collect::<Vec<_>>();
        query.sort();
        let query = query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let host = self.endpoint[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
        let headers = signing::headers(
            method,
            &path,
            &query,
            &host,
            &self.region,
            &self.access,
            &self.secret,
            mime,
            &hash,
        )?;
        transport::request(&self.endpoint, method, &path, &query, &headers, data)
    }
}
