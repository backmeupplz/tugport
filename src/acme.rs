//! Let's Encrypt certificates over HTTP-01, renewed 30 days before expiry. Failures
//! back off (5 min up to a day) and show their reason instead of silently giving up.

use crate::app::{App, AppState};
use crate::util;
use anyhow::{Context, Result, anyhow, bail};
use instant_acme::{Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder, OrderStatus, RetryPolicy};
use rusqlite::{OptionalExtension, params};
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

fn load_stored(app: &App) {
    let rows: Vec<(String, String, Vec<u8>)> = {
        let conn = app.db.conn();
        let mut stmt = match conn.prepare("SELECT host, cert_pem, key_pem FROM certificates WHERE cert_pem IS NOT NULL") {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|m| m.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    };
    for (host, cert, key) in rows.into_iter().filter(|(h, _, _)| !app.proxy.has_cert(h)) {
        match app.keys.open_str(&format!("cert:{host}"), &key).and_then(|k| crate::proxy::certified_key(&cert, &k)) {
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

async fn tick(app: &AppState) -> Result<()> {
    let now = util::now();
    for host in wanted(app)? {
        let row: Option<(Option<i64>, Option<i64>)> = app
            .db
            .conn()
            .query_row("SELECT not_after, next_attempt_at FROM certificates WHERE host = ?1", [&host], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let (not_after, next_attempt) = row.unwrap_or((None, None));
        if not_after.is_some_and(|na| na - now > RENEW_BEFORE) || next_attempt.is_some_and(|t| t > now) {
            continue;
        }
        match issue(app, &host).await {
            Ok((cert, key)) => {
                let expires = first_cert_not_after(&cert).unwrap_or(now + 90 * 86400);
                app.db.conn().execute(
                    "INSERT INTO certificates (host, cert_pem, key_pem, not_after, source, last_error, next_attempt_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'acme', NULL, NULL, ?5)
                     ON CONFLICT(host) DO UPDATE SET cert_pem = excluded.cert_pem, key_pem = excluded.key_pem, not_after = excluded.not_after, last_error = NULL, next_attempt_at = NULL, updated_at = excluded.updated_at",
                    params![host, cert, app.keys.seal_str(&format!("cert:{host}"), &key)?, expires, now],
                )?;
                app.db.conn().execute("DELETE FROM state WHERE key = ?1", [format!("acme_attempts:{host}")])?;
                app.proxy.set_cert(&host, crate::proxy::certified_key(&cert, &key)?);
                tracing::info!("certificate issued for {host}");
                app.emit("certificate", None, None, serde_json::json!({ "host": host, "ok": true }));
                crate::proxy::reload(app);
            }
            Err(e) => {
                let attempts: i64 = app.db.get_state(&format!("acme_attempts:{host}"))?.and_then(|v| v.parse().ok()).unwrap_or(0) + 1;
                let wait = match attempts {
                    1 => 300,
                    2 => 1800,
                    3 => 7200,
                    4 => 21600,
                    _ => 86400,
                };
                let msg = format!("{e:#}");
                app.db.set_state(&format!("acme_attempts:{host}"), &attempts.to_string())?;
                app.db.conn().execute(
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
    if let Some(sealed) = app.db.get_state("acme_account")? {
        let creds: AccountCredentials = serde_json::from_str(&app.keys.open_str("acme", &crate::crypto::unb64(&sealed)?)?)?;
        return Ok(Account::builder()?.from_credentials(creds).await?);
    }
    let email = app.settings().acme_email.map(|e| format!("mailto:{e}"));
    let contact: Vec<&str> = email.iter().map(|e| e.as_str()).collect();
    let directory = if std::env::var_os("TUGPORT_ACME_STAGING").is_some() { LetsEncrypt::Staging.url() } else { LetsEncrypt::Production.url() };
    let (acc, creds) = Account::builder()?
        .create(&NewAccount { contact: &contact, terms_of_service_agreed: true, only_return_existing: false }, directory.to_owned(), None)
        .await?;
    let sealed = app.keys.seal_str("acme", &serde_json::to_string(&creds)?)?;
    app.db.set_state("acme_account", &crate::crypto::b64(&sealed))?;
    Ok(acc)
}

async fn issue(app: &App, host: &str) -> Result<(String, String)> {
    let addrs: Vec<_> = tokio::net::lookup_host((host, 80)).await.map(|a| a.collect()).unwrap_or_default();
    if addrs.is_empty() {
        bail!("{host} does not resolve yet; point its DNS at this server");
    }
    let acc = account(app).await?;
    let mut order = acc.new_order(&NewOrder::new(&[Identifier::Dns(host.to_string())])).await?;
    let mut tokens = vec![];
    {
        let mut auths = order.authorizations();
        while let Some(a) = auths.next().await {
            let mut a = a?;
            match a.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => bail!("authorization is {other:?}"),
            }
            let mut ch = a.challenge(ChallengeType::Http01).ok_or_else(|| anyhow!("no HTTP-01 challenge offered"))?;
            crate::util::lock(&app.proxy.challenges).insert(ch.token.clone(), ch.key_authorization().as_str().to_string());
            tokens.push(ch.token.clone());
            ch.set_ready().await?;
        }
    }
    let result = async {
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
    let mut ch = crate::util::lock(&app.proxy.challenges);
    for t in tokens {
        ch.remove(&t);
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
    let body = d.get(hdr..hdr + len)?;
    Some((tag, body, &d[hdr + len..]))
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
