//! MCP's OS-root policy, independent of process OpenSSL path overrides.
//!
//! Native 2.1.322 reads `tls.getCACertificates("system")`; it does not read
//! `SSL_CERT_FILE`/`SSL_CERT_DIR` as direct MCP trust inputs. The platform
//! enumeration below follows rustls-native-certs 0.8's native-store adapters,
//! while avoiding that crate's environment-first dispatcher. Ordinary HTTP
//! retains its existing rustls-native-certs cache and environment behavior.

use rustls::pki_types::CertificateDer;
use std::sync::OnceLock;

pub(crate) fn certificates() -> &'static [CertificateDer<'static>] {
    static ROOTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut roots = load();
        roots.sort_unstable_by(|left, right| left.as_ref().cmp(right.as_ref()));
        roots.dedup();
        roots
    })
}

#[cfg(target_os = "macos")]
fn load() -> Vec<CertificateDer<'static>> {
    use security_framework::trust_settings::{Domain, TrustSettings, TrustSettingsForCertificate};
    use std::collections::HashMap;

    // User declarations override Admin, which override System, including deny.
    let mut certificates = HashMap::new();
    for domain in [Domain::User, Domain::Admin, Domain::System] {
        let settings = TrustSettings::new(domain);
        let entries = match settings.iter() {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(%error, "MCP TLS: failed to enumerate an OS trust domain");
                continue;
            }
        };
        for certificate in entries {
            let trust = match settings.tls_trust_settings_for_certificate(&certificate) {
                Ok(trust) => trust.unwrap_or(TrustSettingsForCertificate::TrustRoot),
                Err(error) => {
                    tracing::warn!(%error, "MCP TLS: failed to read certificate trust");
                    continue;
                }
            };
            certificates.entry(certificate.to_der()).or_insert(trust);
        }
    }
    certificates
        .into_iter()
        .filter_map(|(certificate, trust)| {
            matches!(
                trust,
                TrustSettingsForCertificate::TrustRoot | TrustSettingsForCertificate::TrustAsRoot
            )
            .then(|| CertificateDer::from(certificate))
        })
        .collect()
}

#[cfg(windows)]
fn load() -> Vec<CertificateDer<'static>> {
    use schannel::cert_context::ValidUses;
    use schannel::cert_store::CertStore;

    let store = match CertStore::open_current_user("ROOT") {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(%error, "MCP TLS: failed to open the OS root store");
            return Vec::new();
        }
    };
    store
        .certs()
        .filter(|certificate| {
            let valid_usage = match certificate.valid_uses() {
                Ok(ValidUses::All) => true,
                Ok(ValidUses::Oids(oids)) => oids.iter().any(|oid| oid == "1.3.6.1.5.5.7.3.1"),
                Err(_) => false,
            };
            valid_usage && certificate.is_time_valid().unwrap_or(false)
        })
        .map(|certificate| CertificateDer::from(certificate.to_der().to_vec()))
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn load() -> Vec<CertificateDer<'static>> {
    use std::path::Path;

    // Known platform paths parallel openssl-probe 0.2; calling probe() itself
    // would reintroduce SSL_CERT_* overrides. No environment is ever mutated.
    const FILES: &[&str] = &[
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/ca-bundle.pem",
        "/etc/pki/tls/cacert.pem",
        "/etc/ssl/cert.pem",
        "/opt/etc/ssl/certs/ca-certificates.crt",
        "/etc/ssl/certs/cacert.pem",
        "/usr/local/etc/ssl/cert.pem",
        "/usr/local/share/certs/ca-root-nss.crt",
        "/etc/openssl/certs/ca-certificates.crt",
        "/etc/certs/ca-certificates.crt",
        "/etc/ssl/cacert.pem",
        "/data/data/com.termux/files/usr/etc/tls/cert.pem",
        "/boot/system/data/ssl/CARootCertificates.pem",
    ];
    let file = FILES.iter().map(Path::new).find(|path| path.exists());
    let mut roots = rustls_native_certs::load_certs_from_paths(file, None).certs;
    for directory in openssl_probe::candidate_cert_dirs() {
        roots.extend(rustls_native_certs::load_certs_from_paths(None, Some(directory)).certs);
    }
    roots
}

#[cfg(not(any(unix, windows)))]
fn load() -> Vec<CertificateDer<'static>> {
    Vec::new()
}
