//! Compiler results stored through the existing Bazel HTTP cache protocol.

use std::io::Cursor;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::ensure;
use async_trait::async_trait;
use bytes::Bytes;
use prost::Message;
use reqwest::{Client, Method, Response, StatusCode, Url};
use sha2::{Digest, Sha256};

use super::cache_io::{Cache, CacheRead, CacheWrite};
use super::Storage;
use crate::errors::*;

// Wire fields from build.bazel.remote.execution.v2. The existing server keeps
// its normal ActionResult validation; the compiler payload lives in the CAS.
#[derive(Clone, PartialEq, Message)]
struct ActionResult {
    #[prost(message, repeated, tag = "2")]
    output_files: Vec<OutputFile>,
}

#[derive(Clone, PartialEq, Message)]
struct OutputFile {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(message, optional, tag = "2")]
    digest: Option<BlobDigest>,
}

#[derive(Clone, PartialEq, Message)]
struct BlobDigest {
    #[prost(string, tag = "1")]
    hash: String,
    #[prost(int64, tag = "2")]
    size_bytes: i64,
}

pub struct BazelHttpCache {
    endpoint: String,
    client: Client,
    basedirs: Vec<Vec<u8>>,
}

impl BazelHttpCache {
    pub fn new(endpoint: &str, basedirs: Vec<Vec<u8>>) -> Result<Self> {
        let url = Url::parse(endpoint).context("Invalid persistent-cache URL")?;
        ensure!(
            url.scheme() == "http"
                && url
                    .host_str()
                    .and_then(|host| host.parse::<IpAddr>().ok())
                    .is_some_and(|host| host.is_loopback())
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Persistent compiler cache requires the approved loopback SSH tunnel"
        );
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(/*secs*/ 60))
                .build()?,
            basedirs,
        })
    }

    async fn request(&self, method: Method, path: &str, body: Bytes) -> Result<Response> {
        for attempt in 0..5 {
            let response = self
                .client
                .request(method.clone(), format!("{}{path}", self.endpoint))
                .body(body.clone())
                .send()
                .await;
            match response {
                Ok(response)
                    if !response.status().is_server_error()
                        && response.status() != StatusCode::TOO_MANY_REQUESTS =>
                {
                    return Ok(response)
                }
                Ok(response) if attempt == 4 => bail!(
                    "Persistent cache HTTP failure: {}",
                    response.status().as_u16()
                ),
                Err(_) if attempt == 4 => {
                    bail!("Persistent cache transport failed after bounded retries")
                }
                Ok(_) | Err(_) => {
                    let delay = f64::from(1_u32 << attempt) + rand::random::<f64>();
                    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                }
            }
        }
        unreachable!("bounded retry returns on its final attempt")
    }
}

#[async_trait]
impl Storage for BazelHttpCache {
    async fn get(&self, key: &str) -> Result<Cache> {
        ensure!(
            key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid compiler-cache key"
        );
        let response = self
            .request(Method::GET, &format!("/ac/{key}"), Bytes::new())
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Cache::Miss);
        }
        ensure!(
            response.status().is_success(),
            "Persistent cache action lookup failed"
        );
        let record = ActionResult::decode(response.bytes().await?)
            .context("Invalid persistent cache action record")?;
        let [output] = record.output_files.as_slice() else {
            bail!("Persistent compiler action must contain exactly one output");
        };
        ensure!(
            output.path == "sccache",
            "Unexpected persistent compiler output"
        );
        let digest = output
            .digest
            .as_ref()
            .context("Persistent compiler output has no digest")?;
        ensure!(
            digest.hash.len() == 64
                && digest.hash.bytes().all(|b| b.is_ascii_hexdigit())
                && digest.size_bytes >= 0,
            "Invalid persistent compiler output digest"
        );
        let response = self
            .request(Method::GET, &format!("/cas/{}", digest.hash), Bytes::new())
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Cache::Miss);
        }
        ensure!(
            response.status().is_success(),
            "Persistent cache blob lookup failed"
        );
        let data = response.bytes().await?;
        ensure!(
            i64::try_from(data.len()).ok() == Some(digest.size_bytes)
                && format!("{:x}", Sha256::digest(&data)) == digest.hash,
            "Persistent compiler output digest mismatch"
        );
        Ok(Cache::Hit(CacheRead::from(Cursor::new(data))?))
    }

    async fn put(&self, key: &str, entry: CacheWrite) -> Result<Duration> {
        ensure!(
            key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid compiler-cache key"
        );
        let started = Instant::now();
        let data = Bytes::from(entry.finish()?);
        let hash = format!("{:x}", Sha256::digest(&data));
        let size_bytes = i64::try_from(data.len()).context("Compiler cache output is too large")?;
        let response = self
            .request(Method::PUT, &format!("/cas/{hash}"), data)
            .await?;
        ensure!(
            response.status().is_success(),
            "Persistent compiler blob upload failed"
        );
        let record = ActionResult {
            output_files: vec![OutputFile {
                path: "sccache".to_owned(),
                digest: Some(BlobDigest { hash, size_bytes }),
            }],
        };
        let response = self
            .request(
                Method::PUT,
                &format!("/ac/{key}"),
                Bytes::from(record.encode_to_vec()),
            )
            .await?;
        ensure!(
            response.status().is_success(),
            "Persistent compiler action upload failed"
        );
        Ok(started.elapsed())
    }

    fn location(&self) -> String {
        "bazel-http".to_owned()
    }
    fn cache_type_name(&self) -> &'static str {
        "bazel-http"
    }
    fn basedirs(&self) -> &[Vec<u8>] {
        &self.basedirs
    }
    async fn current_size(&self) -> Result<Option<u64>> {
        Ok(None)
    }
    async fn max_size(&self) -> Result<Option<u64>> {
        Ok(None)
    }
}
