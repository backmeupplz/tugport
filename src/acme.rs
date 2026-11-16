//! Let's Encrypt certificates, proven over HTTP-01 or, for hosts in the Cloudflare zones of the
//! DNS token (and always for wildcards), DNS-01; renewed 30 days before expiry. Failures
//! back off (5 min up to a day) and show their reason instead of silently giving up.

use crate::app::{App, AppState};
use crate::util;
use anyhow::{Context, Result, anyhow, bail};
use instant_acme::{Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder, OrderStatus, RetryPolicy};
use rusqlite::params;
use std::time::Duration;

const RENEW_BEFORE: i64 = 30 * 86400;

pub async fn run(app: AppState) {
    loop {
        // Every round, so certificates imported while running (`tugport migrate`) are served too.
        load_stored(&app);
        if let Err(e) = tick(&app).await {
            tracing::warn!("certificate check: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// The sealing context of a certificate's private key.
pub fn key_context(host: &str) -> String {
    format!("cert:{host}")
}

/// Every stored certificate: (host, certificate PEM, sealed key).
pub fn stored(app: &App) -> Result<Vec<(String, String, Vec<u8>)>> {
    Ok(app.db.rows("SELECT host, cert_pem, key_pem FROM certificates WHERE cert_pem IS NOT NULL", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
}

pub fn load_stored(app: &App) {
    let rows = match stored(app) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("reading stored certificates: {e}");
            return;
        }
    };
    for (host, cert, key) in rows.into_iter().filter(|(h, _, _)| !app.proxy.has_cert(h)) {
        match app.keys.open_str(&key_context(&host), &key).and_then(|k| crate::proxy::certified_key(&cert, &k)) {
            Ok(ck) => app.proxy.set_cert(&host, ck),
            Err(e) => tracing::warn!("stored certificate for {host} is unusable: {e:#}"),
        }
    }
}

/// Hosts that should have a certificate.
fn wanted(app: &App) -> Result<Vec<String>> {
    let mut hosts: Vec<String> = crate::store::domains(app, None)?.into_iter().filter(|d| d.https && !d.via_tunnel).map(|d| d.host).collect();
    if let Some(p) = app.settings().panel_domain {
        hosts.push(p);
    }
    hosts.sort();
    hosts.dedup();
    Ok(hosts)
}

/// Sealed state holding the Cloudflare API token that proves domains with DNS records.
const DNS_TOKEN: &str = "acme_dns_token";
/// The zones that token covered when it was saved, for the settings page.
const DNS_ZONES: &str = "acme_dns_zones";

pub fn dns_token(app: &App) -> Result<Option<zeroize::Zeroizing<String>>> {
    app.sealed_state(DNS_TOKEN, DNS_TOKEN)
}

/// The zones the DNS token covers, or None without a token.
pub fn dns_zones(app: &App) -> Option<Vec<String>> {
    dns_token(app).ok().flatten()?;
    Some(app.db.state::<String>(DNS_ZONES).unwrap_or_default().split_whitespace().map(String::from).collect())
}

/// Check a Cloudflare API token, keep it and retry every failed certificate right away.
pub async fn set_dns_token(app: &App, token: &str) -> Result<Vec<String>> {
    let zones = crate::tunnel::cf(token, reqwest::Method::GET, "/zones?per_page=50", None)
        .await
        .map_err(|e| anyhow!("Cloudflare did not accept this token: {}", e.to_string().trim_start_matches("Cloudflare: ")))?;
    let names: Vec<String> = zones.as_array().into_iter().flatten().filter_map(|z| z["name"].as_str().map(String::from)).collect();
    if names.is_empty() {
        bail!("the token sees no zones; give it Zone > Zone > Read and Zone > DNS > Edit on your domains");
    }
    app.set_sealed_state(DNS_TOKEN, DNS_TOKEN, token)?;
    app.db.set_state(DNS_ZONES, &names.join(" "))?;
    retry(app, None)?;
    Ok(names)
}

pub fn clear_dns_token(app: &App) -> Result<()> {
    app.db.del_state(DNS_TOKEN)?;
    app.db.del_state(DNS_ZONES)?;
    Ok(())
}

/// Forget failed attempts for `host` (or every host), so the next round tries again.
pub fn retry(app: &App, host: Option<&str>) -> Result<()> {
    app.db.exec("UPDATE certificates SET next_attempt_at = NULL WHERE ?1 IS NULL OR host = ?1", [host])?;
    app.db.exec("DELETE FROM state WHERE key LIKE 'acme_attempts:%' AND (?1 IS NULL OR key = 'acme_attempts:' || ?1)", [host])?;
    Ok(())
}

/// A host's certificate state for the panel: expiry, how it was proven, the last failure.
pub fn status(app: &App, host: &str) -> Result<Option<serde_json::Value>> {
    Ok(app.db.row(
        "SELECT cert_pem IS NOT NULL, not_after, source, last_error, next_attempt_at FROM certificates WHERE host = ?1",
        [host],
        |r| {
            Ok(serde_json::json!({
                "issued": r.get::<_, bool>(0)?, "not_after": r.get::<_, Option<i64>>(1)?, "source": r.get::<_, String>(2)?,
                "last_error": r.get::<_, Option<String>>(3)?, "next_attempt_at": r.get::<_, Option<i64>>(4)?,
            }))
        },
    )?)
}

/// How Let's Encrypt checks that we control a host.
enum Proof {
    /// A file served on port 80.
    Http,
    /// A TXT record in the host's Cloudflare zone.
    Dns { token: String, zone: String },
}

/// DNS whenever the token's account holds the host's zone (it needs no open port), HTTP otherwise.
async fn proof(token: Option<&str>, host: &str) -> Result<Proof> {
    let wildcard = host.starts_with("*.");
    if let Some(t) = token {
        match crate::tunnel::zone_for(t, host.trim_start_matches("*.")).await {
            Ok(zone) => return Ok(Proof::Dns { token: t.to_string(), zone }),
            Err(e) if wildcard => return Err(e.context("a wildcard certificate is proven with a DNS record")),
            Err(_) => {}
        }
    }
    if wildcard {
        bail!("a wildcard certificate needs DNS verification; add a Cloudflare API token in Settings > Certificates");
    }
    Ok(Proof::Http)
}

async fn tick(app: &AppState) -> Result<()> {
    let listening = app.proxy.listening.load(std::sync::atomic::Ordering::Relaxed);
    let token = dns_token(app)?;
    let now = util::now();
    for host in wanted(app)? {
        let row = app.db.row("SELECT not_after, next_attempt_at FROM certificates WHERE host = ?1", [&host], |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)))?;
        let (not_after, next_attempt) = row.unwrap_or((None, None));
        let attempts_key = format!("acme_attempts:{host}");
        if not_after.is_some_and(|na| na - now > RENEW_BEFORE) || next_attempt.is_some_and(|t| t > now) {
            continue;
        }
        let proof = proof(token.as_ref().map(|t| t.as_str()), &host).await;
        // While another proxy (nginx, Caddy, Traefik) still holds port 80, Let's Encrypt would reach
        // it instead and every HTTP attempt would fail and back off; those wait until the port is ours.
        if matches!(proof, Ok(Proof::Http)) && !listening {
            continue;
        }
        let source = if matches!(proof, Ok(Proof::Dns { .. })) { "acme-dns" } else { "acme" };
        let issued = match proof {
            Ok(p) => issue(app, &host, p).await,
            Err(e) => Err(e),
        };
        match issued {
            Ok((cert, key)) => {
                let expires = first_cert_not_after(&cert).unwrap_or(now + 90 * 86400);
                app.db.exec(
                    "INSERT INTO certificates (host, cert_pem, key_pem, not_after, source, last_error, next_attempt_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?6, NULL, NULL, ?5)
                     ON CONFLICT(host) DO UPDATE SET cert_pem = excluded.cert_pem, key_pem = excluded.key_pem, not_after = excluded.not_after, source = excluded.source, last_error = NULL, next_attempt_at = NULL, updated_at = excluded.updated_at",
                    params![host, cert, app.keys.seal_str(&key_context(&host), &key)?, expires, now, source],
                )?;
                app.db.del_state(&attempts_key)?;
                app.proxy.set_cert(&host, crate::proxy::certified_key(&cert, &key)?);
                tracing::info!("certificate issued for {host}");
                app.emit("certificate", None, None, serde_json::json!({ "host": host, "ok": true }));
                crate::proxy::reload(app);
            }
            Err(e) => {
                let attempts: i64 = app.db.get_state(&attempts_key)?.and_then(|v| v.parse().ok()).unwrap_or(0) + 1;
                let wait = match attempts {
                    1 => 300,
                    2 => 1800,
                    3 => 7200,
                    4 => 21600,
                    _ => 86400,
                };
                let msg = format!("{e:#}");
                app.db.set_state(&attempts_key, &attempts.to_string())?;
                app.db.exec(
                    "INSERT INTO certificates (host, last_error, next_attempt_at, updated_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(host) DO UPDATE SET last_error = excluded.last_error, next_attempt_at = excluded.next_attempt_at, updated_at = excluded.updated_at",
                    params![host, msg, now + wait, now],
                )?;
                tracing::warn!("certificate for {host} failed (retrying in {}m): {msg}", wait / 60);
                app.emit("certificate", None, None, serde_json::json!({ "host": host, "ok": false, "error": msg }));
                if attempts >= 3 || not_after.is_some_and(|na| na - now < 7 * 86400) {
                    crate::notify::event(app, "certificate.failed", &format!("Certificate for {host} failed"), &msg, None).await;
                }
            }
        }
    }
    Ok(())
}

async fn account(app: &App) -> Result<Account> {
    if let Some(creds) = app.sealed_state("acme_account", "acme")? {
        let creds: AccountCredentials = serde_json::from_str(&creds)?;
        return Ok(Account::builder()?.from_credentials(creds).await?);
    }
    let email = app.settings().acme_email.map(|e| format!("mailto:{e}"));
    let contact: Vec<&str> = email.iter().map(|e| e.as_str()).collect();
    let directory = if std::env::var_os("TUGPORT_ACME_STAGING").is_some() { LetsEncrypt::Staging.url() } else { LetsEncrypt::Production.url() };
    let (acc, creds) = Account::builder()?
        .create(&NewAccount { contact: &contact, terms_of_service_agreed: true, only_return_existing: false }, directory.to_owned(), None)
        .await?;
    app.set_sealed_state("acme_account", "acme", &serde_json::to_string(&creds)?)?;
    Ok(acc)
}

async fn issue(app: &App, host: &str, proof: Proof) -> Result<(String, String)> {
    if matches!(proof, Proof::Http) && tokio::net::lookup_host((host, 80)).await.map(|mut a| a.next().is_none()).unwrap_or(true) {
        bail!("{host} does not resolve yet; point its DNS at this server");
    }
    let acc = account(app).await?;
    let mut order = acc.new_order(&NewOrder::new(&[Identifier::Dns(host.to_string())])).await?;
    let mut tokens = vec![];
    let mut records = vec![];
    let result = async {
        let mut auths = order.authorizations();
        while let Some(a) = auths.next().await {
            let mut a = a?;
            match a.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => bail!("authorization is {other:?}"),
            }
            match &proof {
                Proof::Http => {
                    let mut ch = a.challenge(ChallengeType::Http01).ok_or_else(|| anyhow!("no HTTP-01 challenge offered"))?;
                    crate::util::lock(&app.proxy.challenges).insert(ch.token.clone(), ch.key_authorization().as_str().to_string());
                    tokens.push(ch.token.clone());
                    ch.set_ready().await?;
                }
                Proof::Dns { token, zone } => {
                    let mut ch = a.challenge(ChallengeType::Dns01).ok_or_else(|| anyhow!("no DNS-01 challenge offered"))?;
                    // A record of our own, deleted by id afterwards: other ACME clients may keep
                    // records under the same name.
                    let body = serde_json::json!({
                        "type": "TXT", "name": format!("_acme-challenge.{}", host.trim_start_matches("*.")),
                        "content": ch.key_authorization().dns_value(), "ttl": 60, "comment": "Tugport certificate check",
                    });
                    let rec = crate::tunnel::cf(token, reqwest::Method::POST, &format!("/zones/{zone}/dns_records"), Some(body)).await.context("adding the DNS record")?;
                    records.push((token.clone(), zone.clone(), rec["id"].as_str().unwrap_or_default().to_string()));
                    // ponytail: fixed wait for Cloudflare to publish the record (it takes seconds);
                    // poll the zone's name servers if this ever proves too short.
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    ch.set_ready().await?;
                }
            }
        }
        let status = order.poll_ready(&RetryPolicy::default()).await?;
        if status != OrderStatus::Ready {
            let detail = order
                .authorizations()
                .next()
                .await
                .and_then(|a| a.ok())
                .and_then(|a| a.challenges.iter().find_map(|c| c.error.as_ref().and_then(|e| e.detail.clone())));
            bail!("Let's Encrypt could not verify {host}: {}", detail.unwrap_or_else(|| format!("{status:?}")));
        }
        let key = order.finalize().await.context("finalizing the order")?;
        let cert = order.poll_certificate(&RetryPolicy::default()).await?;
        Ok((cert, key))
    }
    .await;
    {
        let mut ch = crate::util::lock(&app.proxy.challenges);
        for t in tokens {
            ch.remove(&t);
        }
    }
    for (token, zone, id) in records.into_iter().filter(|r| !r.2.is_empty()) {
        if let Err(e) = crate::tunnel::cf(&token, reqwest::Method::DELETE, &format!("/zones/{zone}/dns_records/{id}"), None).await {
            tracing::warn!("removing the certificate check record for {host}: {e:#}");
        }
    }
    result
}

/// notAfter of the first certificate in a PEM chain (minimal DER walk).
pub fn first_cert_not_after(pem: &str) -> Option<i64> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let der = CertificateDer::pem_slice_iter(pem.as_bytes()).next()?.ok()?;
    not_after(&der)
}

fn tlv(d: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *d.first()?;
    let first = *d.get(1)? as usize;
    let (len, hdr) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *d.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = hdr.checked_add(len)?;
    Some((tag, d.get(hdr..end)?, d.get(end..)?))
}

fn not_after(der: &[u8]) -> Option<i64> {
    let (_, cert, _) = tlv(der)?;
    let (_, tbs, _) = tlv(cert)?;
    let mut rest = tbs;
    let (tag, _, r) = tlv(rest)?;
    if tag == 0xa0 {
        rest = r;
    }
    for _ in 0..3 {
        rest = tlv(rest)?.2; // serial, signature algorithm, issuer
    }
    let (_, validity, _) = tlv(rest)?;
    let (_, _, after_before) = tlv(validity)?;
    let (tag, time, _) = tlv(after_before)?;
    let s = std::str::from_utf8(time).ok()?;
    let full = match tag {
        0x17 => format!("{}{}", if s.get(..2)?.parse::<u32>().ok()? < 50 { "20" } else { "19" }, s),
        0x18 => s.to_string(),
        _ => return None,
    };
    chrono::NaiveDateTime::parse_from_str(full.trim_end_matches('Z'), "%Y%m%d%H%M%S").ok().map(|t| t.and_utc().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_expiry() {
        let mut params = rcgen::CertificateParams::new(vec!["example.com".into()]).unwrap();
        params.not_after = rcgen::date_time_ymd(2031, 7, 9);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let ts = first_cert_not_after(&cert.pem()).unwrap();
        assert_eq!(chrono::DateTime::from_timestamp(ts, 0).unwrap().format("%Y-%m-%d").to_string(), "2031-07-09");
    }
}
