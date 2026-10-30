//! Requests between Bonfire and other Fediverse servers.
//!
//! Every request is signed with the RSA key of a local user, following the
//! HTTP Signatures draft (cavage-12) that Mastodon and the others use: the
//! `Signature` header signs the method, path, host, date and a `Digest` of the
//! body. Incoming requests are checked the same way with the sender's key.
//!
//! Other servers' addresses come from requests anyone can send, so outgoing
//! requests only go to public addresses: without that, someone could make
//! Bonfire call services of its own network.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use axum::http::{HeaderMap, Method, Uri, header};
use reqwest::{
    StatusCode, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    redirect,
};
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs1::DecodeRsaPublicKey,
    pkcs1v15::{Signature, SigningKey, VerifyingKey},
    pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, LineEnding},
    sha2::{Digest, Sha256},
    signature::{SignatureEncoding, Signer as _, Verifier},
};
use serde_json::Value;
use sqlx::{
    PgPool,
    types::chrono::{DateTime, Utc},
};
use tokio::task::JoinSet;

use super::{ACTIVITY_JSON, Federation, base64};
use crate::entities::federation::{self as store, Delivery, NewRemoteActor, RemoteActor, Signer};

/// Larger answers from other servers are refused: an actor or an activity is
/// a few kilobytes.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const RSA_BITS: usize = 2048;
/// Signed requests older than this are refused (Mastodon uses the same window),
/// so a captured request can't be replayed later.
const MAX_SIGNATURE_AGE_HOURS: i64 = 12;
/// Allowed clock difference with the other server, for dates in the future.
const MAX_CLOCK_SKEW_MINUTES: i64 = 5;
const DELIVERY_BATCH: i64 = 20;
/// Waits between attempts: about 2.5 days in total, then the delivery is abandoned.
const RETRY_DELAYS_SECONDS: [f64; 8] = [60.0, 300.0, 1800.0, 7200.0, 21600.0, 43200.0, 86400.0, 86400.0];

/// What `Accept` asks for when fetching ActivityPub JSON.
const ACCEPT_ACTIVITY: &str =
    "application/activity+json, application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"";

// --- Outgoing requests ---

/// HTTP client for other servers: short timeouts, few redirects, and public
/// addresses only unless `allow_insecure`.
pub fn client(base_url: &str, allow_insecure: bool) -> Result<reqwest::Client, String> {
    let redirects = redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 3 {
            attempt.error("too many redirects")
        } else if let Err(err) = check_url(attempt.url(), allow_insecure) {
            attempt.error(err)
        } else {
            attempt.follow()
        }
    });
    let builder = reqwest::Client::builder()
        .user_agent(format!("Bonfire/{} (+{base_url})", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(redirects)
        // A proxy from the environment would bypass the address check.
        .no_proxy();
    let builder = if allow_insecure {
        builder
    } else {
        builder.dns_resolver(PublicResolver)
    };
    builder
        .build()
        .map_err(|err| format!("failed to create the federation HTTP client: {err}"))
}

/// Resolves domains like the system does, but drops private addresses. Done at
/// connection time, so a domain can't pass a check then point elsewhere.
struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|address| is_public(address.ip()))
                .collect();
            if addresses.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// Checks what the resolver can't: the scheme, and addresses written as IPs.
pub fn check_url(url: &Url, allow_insecure: bool) -> Result<(), String> {
    match url.scheme() {
        "https" => {}
        "http" if allow_insecure => {}
        scheme => return Err(format!("{scheme}:// addresses are not allowed")),
    }
    let host = url.host_str().ok_or("address without a domain")?;
    let literal_ip = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>();
    match literal_ip {
        Ok(ip) if !allow_insecure && !is_public(ip) => Err(format!("{ip} is a private address")),
        _ => Ok(()),
    }
}

/// `false` for loopback, private, link-local and reserved addresses.
/// Written by hand since `IpAddr::is_global` is not stable yet.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => is_public_v4(mapped),
            None => is_public_v6(ip),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        // Carrier-grade NAT, 100.64.0.0/10.
        || (a == 100 && (64..128).contains(&b))
        // IETF protocol assignments, 192.0.0.0/24.
        || (a == 192 && b == 0 && c == 0)
        // Benchmarking, 198.18.0.0/15.
        || (a == 198 && (b == 18 || b == 19))
        // Reserved, 240.0.0.0/4.
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Unique local, fc00::/7.
        || (first & 0xFE00) == 0xFC00
        // Link-local, fe80::/10.
        || (first & 0xFFC0) == 0xFE80
        // Documentation, 2001:db8::/32.
        || (first == 0x2001 && ip.segments()[1] == 0x0DB8))
}

/// Why a request failed. `Gone` when the other server says the thing doesn't
/// exist (404 or 410), for example a deleted account.
#[derive(Debug)]
pub enum FetchError {
    Gone,
    Failed(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone => write!(f, "not found on the other server"),
            Self::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

/// GETs ActivityPub JSON, signed by `key` when given: servers in "secure mode"
/// (authorized fetch) only answer signed requests.
pub async fn fetch(federation: &Federation, url: &str, key: Option<&LocalKey>) -> Result<Value, FetchError> {
    let url = Url::parse(url).map_err(|err| FetchError::Failed(format!("invalid address {url}: {err}")))?;
    check_url(&url, federation.allow_insecure).map_err(FetchError::Failed)?;

    let mut request = federation.client.get(url.clone()).header(header::ACCEPT, ACCEPT_ACTIVITY);
    if let Some(key) = key {
        for (name, value) in signed_headers(key, &Method::GET, &url, None) {
            request = request.header(name, value);
        }
    }
    let mut response = request
        .send()
        .await
        .map_err(|err| FetchError::Failed(format!("request to {url} failed: {err}")))?;
    match response.status() {
        status if status.is_success() => {}
        StatusCode::NOT_FOUND | StatusCode::GONE => return Err(FetchError::Gone),
        status => return Err(FetchError::Failed(format!("{url} answered {status}"))),
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| FetchError::Failed(format!("failed to read {url}: {err}")))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(FetchError::Failed(format!("{url} answered more than {MAX_RESPONSE_BYTES} bytes")));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|err| FetchError::Failed(format!("{url} is not JSON: {err}")))
}

/// Why a delivery failed. `GiveUp` when trying again can't help, like a 403.
pub enum SendError {
    Retry(String),
    GiveUp(String),
}

/// POSTs a signed activity to an inbox.
pub async fn send(federation: &Federation, key: &LocalKey, inbox: &str, activity: &str) -> Result<(), SendError> {
    let url = Url::parse(inbox).map_err(|err| SendError::GiveUp(format!("invalid inbox {inbox}: {err}")))?;
    check_url(&url, federation.allow_insecure).map_err(SendError::GiveUp)?;

    let mut request = federation
        .client
        .post(url.clone())
        .header(header::CONTENT_TYPE, ACTIVITY_JSON)
        .body(activity.to_string());
    for (name, value) in signed_headers(key, &Method::POST, &url, Some(activity.as_bytes())) {
        request = request.header(name, value);
    }
    let status = request
        .send()
        .await
        .map_err(|err| SendError::Retry(format!("request failed: {err}")))?
        .status();
    match status {
        status if status.is_success() => Ok(()),
        // Busy or broken for now.
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => Err(SendError::Retry(status.to_string())),
        status if status.is_server_error() => Err(SendError::Retry(status.to_string())),
        // Refused: blocked, unknown inbox, invalid activity...
        status => Err(SendError::GiveUp(status.to_string())),
    }
}

// --- HTTP Signatures ---

/// Private key of a local user, ready to sign.
pub struct LocalKey {
    pub key_id: String,
    private_key: RsaPrivateKey,
}

/// `Date`, `Digest` (with a body) and `Signature` headers for a request.
/// `Host` isn't returned: reqwest sends the same value itself.
fn signed_headers(key: &LocalKey, method: &Method, url: &Url, body: Option<&[u8]>) -> Vec<(&'static str, String)> {
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    let target = match url.query() {
        Some(query) => format!("{} {}?{query}", method.as_str().to_lowercase(), url.path()),
        None => format!("{} {}", method.as_str().to_lowercase(), url.path()),
    };
    let date = http_date(Utc::now());

    let mut signed = vec![("(request-target)", target), ("host", host), ("date", date.clone())];
    let mut headers = vec![("date", date)];
    if let Some(body) = body {
        let digest = digest(body);
        signed.push(("digest", digest.clone()));
        headers.push(("digest", digest));
    }

    let names: Vec<&str> = signed.iter().map(|(name, _)| *name).collect();
    let signing_string = signed
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let signature = SigningKey::<Sha256>::new(key.private_key.clone()).sign(signing_string.as_bytes());
    headers.push((
        "signature",
        format!(
            "keyId=\"{}\",algorithm=\"rsa-sha256\",headers=\"{}\",signature=\"{}\"",
            key.key_id,
            names.join(" "),
            base64::encode(&signature.to_vec())
        ),
    ));
    headers
}

/// Like `Sun, 06 Nov 1994 08:49:37 GMT`.
fn http_date(time: DateTime<Utc>) -> String {
    time.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn digest(body: &[u8]) -> String {
    format!("SHA-256={}", base64::encode(&Sha256::digest(body)))
}

/// The `Signature` header of an incoming request.
#[derive(Debug, PartialEq)]
struct SignatureHeader {
    key_id: String,
    /// Header names in signing order, lowercase.
    headers: Vec<String>,
    signature: Vec<u8>,
}

/// Parses `keyId="...",algorithm="...",headers="...",signature="..."`.
fn parse_signature_header(raw: &str) -> Result<SignatureHeader, String> {
    let mut key_id = None;
    let mut headers = None;
    let mut signature = None;
    let mut rest = raw.trim();
    while !rest.is_empty() {
        let (name, after) = rest.split_once('=').ok_or("malformed Signature header")?;
        let after = after.strip_prefix('"').ok_or("Signature values must be quoted")?;
        let (value, after) = after.split_once('"').ok_or("unclosed quote in Signature header")?;
        match name.trim() {
            "keyId" => key_id = Some(value.to_string()),
            "headers" => headers = Some(value.to_lowercase()),
            "signature" => signature = Some(value.to_string()),
            // hs2019 means "deduced from the key": RSA here.
            "algorithm" if !matches!(value, "rsa-sha256" | "hs2019") => {
                return Err(format!("unsupported signature algorithm {value}"));
            }
            _ => {}
        }
        rest = after.trim_start().trim_start_matches(',').trim_start();
    }
    Ok(SignatureHeader {
        key_id: key_id.ok_or("Signature without keyId")?,
        // The draft's default when the list is missing.
        headers: headers
            .unwrap_or_else(|| "date".to_string())
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        signature: base64::decode(&signature.ok_or("Signature without signature")?)
            .ok_or("Signature is not base64")?,
    })
}

/// Rebuilds the text the sender signed from the received request.
fn signing_string(method: &Method, uri: &Uri, headers: &HeaderMap, names: &[String]) -> Result<String, String> {
    names
        .iter()
        .map(|name| {
            let value = if name == "(request-target)" {
                let target = uri.path_and_query().map(|target| target.as_str()).unwrap_or("/");
                format!("{} {target}", method.as_str().to_lowercase())
            } else {
                let values: Vec<&str> = headers
                    .get_all(name.as_str())
                    .iter()
                    .map(|value| value.to_str().map(str::trim))
                    .collect::<Result<_, _>>()
                    .map_err(|_| format!("header {name} is not text"))?;
                if values.is_empty() {
                    return Err(format!("signed header {name} is missing"));
                }
                values.join(", ")
            };
            Ok(format!("{name}: {value}"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|lines| lines.join("\n"))
}

fn parse_public_key(pem: &str) -> Result<RsaPublicKey, String> {
    // Most servers send SPKI ("PUBLIC KEY"), a few the older PKCS#1 ("RSA PUBLIC KEY").
    if pem.contains("BEGIN RSA PUBLIC KEY") {
        RsaPublicKey::from_pkcs1_pem(pem.trim()).map_err(|err| format!("invalid public key: {err}"))
    } else {
        RsaPublicKey::from_public_key_pem(pem.trim()).map_err(|err| format!("invalid public key: {err}"))
    }
}

fn signature_matches(public_key_pem: &str, signing_string: &str, signature: &[u8]) -> bool {
    let Ok(public_key) = parse_public_key(public_key_pem) else {
        return false;
    };
    let Ok(signature) = Signature::try_from(signature) else {
        return false;
    };
    VerifyingKey::<Sha256>::new(public_key)
        .verify(signing_string.as_bytes(), &signature)
        .is_ok()
}

/// Where the sender's key may come from.
pub enum KeySource<'a> {
    /// Only an already known actor, for activities that don't need more
    /// (a deleted account can't be fetched anymore).
    Known,
    /// Fetched from the other server when unknown or changed, the request
    /// signed with this local key.
    Fetch(&'a LocalKey),
}

/// Checks the signature, date and digest of an incoming POST. Returns the actor
/// who signed it.
pub async fn verify_request(
    federation: &Federation,
    pool: &PgPool,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
    key_source: KeySource<'_>,
) -> Result<RemoteActor, String> {
    let raw = headers
        .get("signature")
        .ok_or("request is not signed")?
        .to_str()
        .map_err(|_| "Signature header is not text")?;
    let signature = parse_signature_header(raw)?;

    for required in ["(request-target)", "host", "date", "digest"] {
        if !signature.headers.iter().any(|name| name == required) {
            return Err(format!("the signature must cover {required}"));
        }
    }
    check_date(headers)?;
    check_digest(headers, body)?;
    let signing_string = signing_string(method, uri, headers, &signature.headers)?;

    let known = store::find_actor_by_key(pool, &signature.key_id)
        .await
        .map_err(|err| format!("failed to load key: {err}"))?;
    if let Some(actor) = known
        .as_ref()
        .filter(|actor| signature_matches(&actor.public_key_pem, &signing_string, &signature.signature))
    {
        return Ok(actor.clone());
    }
    // Unknown key, or the actor changed it: fetch it again once.
    let KeySource::Fetch(local_key) = key_source else {
        return Err(format!("unknown or invalid key {}", signature.key_id));
    };
    let actor = refresh_actor_by_key(federation, pool, &signature.key_id, local_key).await?;
    if signature_matches(&actor.public_key_pem, &signing_string, &signature.signature) {
        Ok(actor)
    } else {
        Err(format!("invalid signature from {}", signature.key_id))
    }
}

fn check_date(headers: &HeaderMap) -> Result<(), String> {
    let raw = headers
        .get(header::DATE)
        .and_then(|value| value.to_str().ok())
        .ok_or("missing Date header")?;
    let date = DateTime::parse_from_rfc2822(raw).map_err(|_| format!("invalid Date header {raw}"))?;
    let age_seconds = Utc::now().signed_duration_since(date).num_seconds();
    if age_seconds > MAX_SIGNATURE_AGE_HOURS * 3600 || age_seconds < -MAX_CLOCK_SKEW_MINUTES * 60 {
        return Err(format!("Date header {raw} is too far from now"));
    }
    Ok(())
}

/// The `Digest` header proves the body is the one that was signed.
fn check_digest(headers: &HeaderMap, body: &[u8]) -> Result<(), String> {
    let raw = headers
        .get("digest")
        .and_then(|value| value.to_str().ok())
        .ok_or("missing Digest header")?;
    // May list several algorithms: "SHA-256=...,SHA-512=...".
    let expected = base64::encode(&Sha256::digest(body));
    let matches = raw.split(',').map(str::trim).any(|entry| {
        entry
            .split_once('=')
            .is_some_and(|(algorithm, value)| algorithm.eq_ignore_ascii_case("SHA-256") && value == expected)
    });
    if matches {
        Ok(())
    } else {
        Err("Digest doesn't match the body".to_string())
    }
}

// --- Remote actors ---

/// Fetches the actor owning `key_id` and saves it.
async fn refresh_actor_by_key(
    federation: &Federation,
    pool: &PgPool,
    key_id: &str,
    local_key: &LocalKey,
) -> Result<RemoteActor, String> {
    let without_fragment = key_id.split('#').next().unwrap_or(key_id);
    let document = fetch(federation, without_fragment, Some(local_key))
        .await
        .map_err(|err| format!("failed to fetch key {key_id}: {err}"))?;
    // Usually the actor itself, sometimes only the key with its owner.
    let document = if document.get("publicKey").is_some() {
        document
    } else {
        let owner = document
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{key_id} is neither an actor nor a key"))?;
        if !same_host(owner, key_id) {
            return Err(format!("key {key_id} belongs to another server"));
        }
        fetch(federation, owner, Some(local_key))
            .await
            .map_err(|err| format!("failed to fetch actor {owner}: {err}"))?
    };

    let actor = parse_actor(&document)?;
    if actor.key_id != key_id {
        return Err(format!("actor {} doesn't own key {key_id}", actor.actor_id));
    }
    if !same_host(&actor.actor_id, key_id) {
        return Err(format!("key {key_id} belongs to another server"));
    }
    store::save_actor(pool, &actor)
        .await
        .map_err(|err| format!("failed to save actor {}: {err}", actor.actor_id))
}

/// An address given as a string, or as an object with `href` / `url`
/// (Misskey sends `url` and `icon` both ways).
fn link(value: &Value) -> Option<String> {
    match value {
        Value::String(url) => Some(url.clone()),
        Value::Object(fields) => fields
            .get("href")
            .or_else(|| fields.get("url"))
            .and_then(link),
        Value::Array(items) => items.iter().find_map(link),
        _ => None,
    }
}

/// Reads what Bonfire needs from an actor's JSON.
pub fn parse_actor(document: &Value) -> Result<NewRemoteActor, String> {
    let text = |value: &Value, name: &str| value.get(name).and_then(Value::as_str).map(str::to_string);
    let actor_id = text(document, "id").ok_or("actor without id")?;
    let key = document.get("publicKey").ok_or("actor without publicKey")?;
    // Some servers list several keys.
    let key = key.as_array().and_then(|keys| keys.first()).unwrap_or(key);
    let owner = text(key, "owner").ok_or("key without owner")?;
    if owner != actor_id {
        return Err(format!("the key of {actor_id} belongs to {owner}"));
    }
    let inbox = text(document, "inbox").ok_or("actor without inbox")?;
    let shared_inbox = document
        .get("endpoints")
        .and_then(|endpoints| text(endpoints, "sharedInbox"));
    for address in std::iter::once(&inbox).chain(&shared_inbox) {
        if !same_host(address, &actor_id) {
            return Err(format!("inbox {address} is on another server than {actor_id}"));
        }
    }
    Ok(NewRemoteActor {
        username: text(document, "preferredUsername").unwrap_or_default(),
        display_name: text(document, "name").unwrap_or_default(),
        // Only web addresses: shown as images and links on Bonfire's pages.
        icon_url: document
            .get("icon")
            .and_then(link)
            .filter(|url| url.starts_with("https://") || url.starts_with("http://")),
        profile_url: document
            .get("url")
            .and_then(link)
            .filter(|url| url.starts_with("https://") || url.starts_with("http://")),
        inbox,
        shared_inbox,
        key_id: text(key, "id").ok_or("key without id")?,
        public_key_pem: text(key, "publicKeyPem").ok_or("key without publicKeyPem")?,
        actor_id,
    })
}

/// Whether two addresses are on the same server (same scheme, host and port).
pub fn same_host(first: &str, second: &str) -> bool {
    match (Url::parse(first), Url::parse(second)) {
        (Ok(first), Ok(second)) => first.origin() == second.origin(),
        _ => false,
    }
}

// --- Local keys ---

/// The signing key of a user or community, made the first time it's needed.
pub async fn local_key(federation: &Federation, pool: &PgPool, signer: Signer) -> Result<LocalKey, String> {
    let (public_id, _, private_pem) = keys(pool, signer).await?;
    let private_key =
        RsaPrivateKey::from_pkcs8_pem(&private_pem).map_err(|err| format!("invalid key of {signer:?}: {err}"))?;
    Ok(LocalKey {
        key_id: match signer {
            Signer::User(_) => federation.key_id(public_id),
            Signer::Community(_) => federation.community_key_id(public_id),
        },
        private_key,
    })
}

/// (public id, public PEM, private PEM) of a user or community, made the first time.
pub async fn keys(pool: &PgPool, signer: Signer) -> Result<(i64, String, String), String> {
    let load = || async {
        store::local_keys(pool, signer)
            .await
            .map_err(|err| format!("failed to load keys of {signer:?}: {err}"))
    };
    if let Some(keys) = load().await? {
        return Ok(keys);
    }

    // Takes a moment of CPU: kept away from the threads serving requests.
    let (public_pem, private_pem) = tokio::task::spawn_blocking(generate_keys)
        .await
        .map_err(|err| format!("key generation stopped: {err}"))??;
    store::save_local_keys(pool, signer, &public_pem, &private_pem)
        .await
        .map_err(|err| format!("failed to save keys of {signer:?}: {err}"))?;
    tracing::info!("made Fediverse keys for {:?}", signer);
    // Reloaded: another request may have saved its keys first.
    load().await?.ok_or_else(|| format!("{signer:?} does not exist"))
}

fn generate_keys() -> Result<(String, String), String> {
    let private_key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, RSA_BITS)
        .map_err(|err| format!("failed to make a key: {err}"))?;
    let public_pem = RsaPublicKey::from(&private_key)
        .to_public_key_pem(LineEnding::LF)
        .map_err(|err| format!("failed to encode the public key: {err}"))?;
    let private_pem = private_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|err| format!("failed to encode the private key: {err}"))?;
    Ok((public_pem, private_pem.to_string()))
}

// --- Delivery queue ---

/// Sends queued activities, forever. Retries failed ones later, see
/// `RETRY_DELAYS_SECONDS`.
pub async fn run_deliveries(federation: Arc<Federation>, pool: PgPool) {
    loop {
        let batch = match store::claim_deliveries(&pool, DELIVERY_BATCH).await {
            Ok(batch) => batch,
            Err(err) => {
                tracing::warn!("failed to load Fediverse deliveries: {}", err);
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };

        if batch.is_empty() {
            // Until the next retry, a new activity, or a minute at most.
            let wait = match store::next_delivery_at(&pool).await {
                Ok(Some(next)) => (next - Utc::now()).to_std().unwrap_or_default(),
                _ => Duration::from_secs(60),
            }
            .min(Duration::from_secs(60));
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = federation.deliveries.notified() => {}
            }
            continue;
        }

        let mut sending = JoinSet::new();
        for delivery in batch {
            let federation = federation.clone();
            let pool = pool.clone();
            sending.spawn(async move { deliver(&federation, &pool, delivery).await });
        }
        while sending.join_next().await.is_some() {}
    }
}

async fn deliver(federation: &Federation, pool: &PgPool, delivery: Delivery) {
    let result = match local_key(federation, pool, delivery.signer).await {
        Ok(key) => send(federation, &key, &delivery.inbox, &delivery.activity).await,
        Err(err) => Err(SendError::Retry(err)),
    };
    let saved = match result {
        Ok(()) => {
            tracing::debug!("delivered activity to {}", delivery.inbox);
            store::finish_delivery(pool, delivery.id).await
        }
        Err(SendError::Retry(err)) if (delivery.attempts as usize) < RETRY_DELAYS_SECONDS.len() => {
            let delay = RETRY_DELAYS_SECONDS[delivery.attempts as usize];
            tracing::info!("delivery to {} failed, retrying in {}s: {}", delivery.inbox, delay, err);
            store::retry_delivery(pool, delivery.id, delay, &err).await
        }
        Err(SendError::Retry(err) | SendError::GiveUp(err)) => {
            tracing::warn!(
                "abandoned delivery to {} after {} attempts: {}",
                delivery.inbox,
                delivery.attempts + 1,
                err
            );
            store::finish_delivery(pool, delivery.id).await
        }
    };
    if let Err(err) = saved {
        tracing::warn!("failed to update delivery {}: {}", delivery.id, err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_header_round_trip() {
        let parsed = parse_signature_header(
            "keyId=\"https://a.example/users/x#main-key\",algorithm=\"rsa-sha256\",\
             headers=\"(request-target) host date digest\",signature=\"Zm9v\"",
        )
        .unwrap();
        assert_eq!(
            parsed,
            SignatureHeader {
                key_id: "https://a.example/users/x#main-key".to_string(),
                headers: ["(request-target)", "host", "date", "digest"].map(str::to_string).to_vec(),
                signature: b"foo".to_vec(),
            }
        );
        assert!(parse_signature_header("keyId=\"a\",algorithm=\"hmac-sha256\",signature=\"Zm9v\"").is_err());
    }

    /// A request signed by `signed_headers` passes the checks of `verify_request`.
    #[test]
    fn signatures_verify() {
        let (public_pem, private_pem) = generate_keys().unwrap();
        let key = LocalKey {
            key_id: "https://a.example/ap/users/1#main-key".to_string(),
            private_key: RsaPrivateKey::from_pkcs8_pem(&private_pem).unwrap(),
        };
        let url = Url::parse("https://b.example:8443/ap/inbox?x=1").unwrap();
        let body = br#"{"type":"Follow"}"#;

        let mut headers = HeaderMap::new();
        headers.insert("host", "b.example:8443".parse().unwrap());
        for (name, value) in signed_headers(&key, &Method::POST, &url, Some(body)) {
            headers.insert(name, value.parse().unwrap());
        }
        let parsed = parse_signature_header(headers["signature"].to_str().unwrap()).unwrap();
        let uri: Uri = "/ap/inbox?x=1".parse().unwrap();
        let text = signing_string(&Method::POST, &uri, &headers, &parsed.headers).unwrap();

        check_date(&headers).unwrap();
        check_digest(&headers, body).unwrap();
        assert!(check_digest(&headers, b"{}").is_err());
        assert!(signature_matches(&public_pem, &text, &parsed.signature));
        assert!(!signature_matches(&public_pem, &text.replace("x=1", "x=2"), &parsed.signature));
    }

    #[test]
    fn private_addresses_are_refused() {
        for url in ["http://example.com", "https://127.0.0.1", "https://[::1]/", "https://10.1.2.3", "https://[fd00::1]"] {
            assert!(check_url(&Url::parse(url).unwrap(), false).is_err(), "{url}");
        }
        assert!(check_url(&Url::parse("https://example.com/inbox").unwrap(), false).is_ok());
        assert!(check_url(&Url::parse("https://93.184.215.14").unwrap(), false).is_ok());
        assert!(check_url(&Url::parse("http://127.0.0.1:8080").unwrap(), true).is_ok());
    }
}
