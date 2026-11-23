//! Shared HTTP client construction.
//!
//! Every outbound client (Graph, OAuth, updater) starts from [`client_builder`] so they
//! all send the same user agent and trust the same roots: the bundled Mozilla set, the operating
//! system trust store, and any PEM bundles named by the environment variables below. This is what
//! makes the CLI usable behind a TLS-inspecting corporate proxy.
//!
//! There is deliberately no switch that turns certificate verification off.

use std::error::Error as StdError;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

pub(crate) const USER_AGENT: &str = concat!("onenote-cli/", env!("CARGO_PKG_VERSION"));

/// The project's own variable. A file named here must load, otherwise the client is not built.
pub(crate) const EXTRA_CA_ENV: &str = "ONENOTE_EXTRA_CA_PEM";

/// Variables shared with other tools. They are honoured on a best-effort basis: a file that
/// cannot be used is reported once on stderr and skipped.
const GENERIC_CA_ENVS: [&str; 2] = ["SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"];

pub(crate) const TLS_HINT: &str = "TLS certificate was not trusted (unknown issuer). If your network inspects HTTPS, export the corporate root CA as PEM and set ONENOTE_EXTRA_CA_PEM=<path> (SSL_CERT_FILE and NODE_EXTRA_CA_CERTS are also honoured).";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtraCaSource {
    pub(crate) var: &'static str,
    pub(crate) path: PathBuf,
    /// `true` for [`EXTRA_CA_ENV`]: failing to load it is an error rather than a warning.
    pub(crate) required: bool,
}

/// PEM files to load, in precedence order: `ONENOTE_EXTRA_CA_PEM`, `SSL_CERT_FILE`,
/// `NODE_EXTRA_CA_CERTS`. Every variable that is set contributes; a path named more than once is
/// loaded once, under the first variable that names it.
pub(crate) fn extra_ca_paths(lookup: impl Fn(&str) -> Option<String>) -> Vec<ExtraCaSource> {
    let mut out: Vec<ExtraCaSource> = Vec::new();
    let vars = std::iter::once((EXTRA_CA_ENV, true))
        .chain(GENERIC_CA_ENVS.iter().map(|var| (*var, false)));
    for (var, required) in vars {
        let Some(raw) = lookup(var) else { continue };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = PathBuf::from(trimmed);
        if out.iter().any(|seen| seen.path == path) {
            continue;
        }
        out.push(ExtraCaSource {
            var,
            path,
            required,
        });
    }
    out
}

/// Reads one PEM bundle and returns its certificates, or a description of why it is unusable.
fn load_pem_bundle(path: &Path) -> std::result::Result<Vec<reqwest::Certificate>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read file: {e}"))?;

    // reqwest only validates the DER when the client is built, where one bad certificate fails
    // the whole client. Check each certificate here so the caller can decide what to do.
    let mut probe = rustls::RootCertStore::empty();
    let mut count = 0usize;
    for item in CertificateDer::pem_slice_iter(&bytes) {
        let der = item.map_err(|e| format!("invalid PEM encoding: {e}"))?;
        count += 1;
        probe
            .add(der)
            .map_err(|e| format!("certificate #{count} is not a valid X.509 certificate: {e}"))?;
    }
    if count == 0 {
        return Err("no PEM certificates found".to_string());
    }

    reqwest::Certificate::from_pem_bundle(&bytes).map_err(|e| format!("invalid PEM bundle: {e}"))
}

/// Loads every extra root named by `lookup`. Problems with the generic variables are passed to
/// `warn`; a problem with [`EXTRA_CA_ENV`] is returned as an error naming the path.
fn load_extra_roots(
    lookup: impl Fn(&str) -> Option<String>,
    mut warn: impl FnMut(String),
) -> Result<Vec<reqwest::Certificate>> {
    let mut roots = Vec::new();
    for source in extra_ca_paths(lookup) {
        match load_pem_bundle(&source.path) {
            Ok(certs) => roots.extend(certs),
            Err(reason) if source.required => {
                return Err(anyhow!(
                    "{} points to '{}', which could not be loaded as a PEM certificate bundle: {reason}",
                    source.var,
                    source.path.display()
                ));
            }
            Err(reason) => warn(format!(
                "Warning: ignoring {}='{}': {reason}",
                source.var,
                source.path.display()
            )),
        }
    }
    Ok(roots)
}

/// Extra roots for this process, read from the real environment once. Warnings go to stderr,
/// never stdout, which the MCP stdio transport owns.
fn process_extra_roots() -> Result<Vec<reqwest::Certificate>> {
    static ROOTS: OnceLock<std::result::Result<Vec<reqwest::Certificate>, String>> =
        OnceLock::new();
    ROOTS
        .get_or_init(|| {
            load_extra_roots(|name| std::env::var(name).ok(), |line| eprintln!("{line}"))
                .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|message| anyhow!(message))
}

fn builder_with_roots(roots: Vec<reqwest::Certificate>) -> reqwest::ClientBuilder {
    roots.into_iter().fold(
        reqwest::Client::builder().user_agent(USER_AGENT),
        |builder, cert| builder.add_root_certificate(cert),
    )
}

/// Starting point for every HTTP client in the crate. Callers add their own timeouts, pool,
/// redirect and retry settings.
pub(crate) fn client_builder() -> Result<reqwest::ClientBuilder> {
    Ok(builder_with_roots(process_extra_roots()?))
}

/// [`client_builder`] with an injected environment, for tests.
#[cfg(test)]
pub(crate) fn client_builder_with(
    lookup: impl Fn(&str) -> Option<String>,
    warn: impl FnMut(String),
) -> Result<reqwest::ClientBuilder> {
    Ok(builder_with_roots(load_extra_roots(lookup, warn)?))
}

fn is_untrusted_issuer_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("unknownissuer")
        || lower.contains("unknown issuer")
        || lower.contains("self-signed certificate")
        || lower.contains("self signed certificate")
        || lower.contains("unable to get local issuer certificate")
}

fn is_untrusted_issuer(err: &(dyn StdError + 'static)) -> bool {
    if let Some(tls) = err.downcast_ref::<rustls::Error>() {
        if matches!(
            tls,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
        ) {
            return true;
        }
    }
    // An `io::Error` wrapping a TLS error does not expose it through `source()`.
    if let Some(inner) = err
        .downcast_ref::<std::io::Error>()
        .and_then(|io| io.get_ref())
    {
        if let Some(rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)) =
            inner.downcast_ref::<rustls::Error>()
        {
            return true;
        }
    }
    is_untrusted_issuer_text(&err.to_string())
}

/// Returns the recovery hint when `err` or anything in its `source()` chain says the server's
/// certificate chains to an issuer this client does not trust.
pub(crate) fn tls_hint(err: &(dyn StdError + 'static)) -> Option<&'static str> {
    let mut current = Some(err);
    while let Some(e) = current {
        if is_untrusted_issuer(e) {
            return Some(TLS_HINT);
        }
        current = e.source();
    }
    None
}

/// [`tls_hint`] for an `anyhow` error, including its context layers.
pub(crate) fn tls_hint_for(err: &anyhow::Error) -> Option<&'static str> {
    err.chain().find_map(tls_hint)
}

/// Wraps a transport failure in `message`, appending the recovery hint when it applies so that
/// callers printing only the top-level message still show it.
pub(crate) fn transport_error(err: reqwest::Error, message: impl Into<String>) -> anyhow::Error {
    let mut message = message.into();
    if let Some(hint) = tls_hint(&err) {
        message.push_str("\nhint: ");
        message.push_str(hint);
    }
    anyhow::Error::new(err).context(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::Arc;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    struct TestCa {
        cert: rcgen::Certificate,
        key: rcgen::KeyPair,
    }

    fn test_ca(common_name: &str) -> TestCa {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let cert = params.self_signed(&key).unwrap();
        TestCa { cert, key }
    }

    fn write_file(dir: &tempfile::TempDir, name: &str, contents: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn extra_ca_paths_orders_by_precedence_and_removes_duplicates() {
        let sources = extra_ca_paths(env(&[
            ("NODE_EXTRA_CA_CERTS", "/etc/node.pem"),
            ("SSL_CERT_FILE", "/etc/ssl.pem"),
            ("ONENOTE_EXTRA_CA_PEM", "/etc/onenote.pem"),
        ]));
        let got: Vec<(&str, &str, bool)> = sources
            .iter()
            .map(|s| (s.var, s.path.to_str().unwrap(), s.required))
            .collect();
        assert_eq!(
            got,
            vec![
                ("ONENOTE_EXTRA_CA_PEM", "/etc/onenote.pem", true),
                ("SSL_CERT_FILE", "/etc/ssl.pem", false),
                ("NODE_EXTRA_CA_CERTS", "/etc/node.pem", false),
            ]
        );

        // The same file under several variables is loaded once and stays "required".
        let sources = extra_ca_paths(env(&[
            ("ONENOTE_EXTRA_CA_PEM", " /etc/corp.pem "),
            ("SSL_CERT_FILE", "/etc/corp.pem"),
            ("NODE_EXTRA_CA_CERTS", "/etc/corp.pem"),
        ]));
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].var, "ONENOTE_EXTRA_CA_PEM");
        assert!(sources[0].required);

        // Unset and blank variables contribute nothing.
        assert!(extra_ca_paths(env(&[])).is_empty());
        assert!(extra_ca_paths(env(&[
            ("ONENOTE_EXTRA_CA_PEM", "   "),
            ("SSL_CERT_FILE", "")
        ]))
        .is_empty());
    }

    #[test]
    fn single_pem_and_multi_certificate_bundle_both_build_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let first = test_ca("onenote test CA 1").cert.pem();
        let second = test_ca("onenote test CA 2").cert.pem();
        let single = write_file(&dir, "single.pem", &first);
        let bundle = write_file(&dir, "bundle.pem", &format!("{first}\n{second}"));

        assert_eq!(load_pem_bundle(Path::new(&single)).unwrap().len(), 1);
        assert_eq!(load_pem_bundle(Path::new(&bundle)).unwrap().len(), 2);

        for path in [&single, &bundle] {
            let mut warnings = Vec::new();
            let builder =
                client_builder_with(env(&[("ONENOTE_EXTRA_CA_PEM", path)]), |w| warnings.push(w))
                    .unwrap();
            builder.build().unwrap();
            assert!(warnings.is_empty(), "{warnings:?}");
        }
    }

    #[test]
    fn unusable_onenote_extra_ca_pem_is_an_error_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir
            .path()
            .join("missing.pem")
            .to_string_lossy()
            .into_owned();
        let err = client_builder_with(env(&[("ONENOTE_EXTRA_CA_PEM", &missing)]), |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("ONENOTE_EXTRA_CA_PEM"), "{err}");
        assert!(err.contains(&missing), "{err}");

        let garbage = write_file(&dir, "garbage.pem", "this is not a certificate\n");
        let err = client_builder_with(env(&[("ONENOTE_EXTRA_CA_PEM", &garbage)]), |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains(&garbage), "{err}");
        assert!(err.contains("no PEM certificates found"), "{err}");

        let bad_der = write_file(
            &dir,
            "bad-der.pem",
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        );
        let err = client_builder_with(env(&[("ONENOTE_EXTRA_CA_PEM", &bad_der)]), |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains(&bad_der), "{err}");
    }

    #[test]
    fn unusable_generic_ca_variables_warn_and_the_client_still_builds() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = write_file(&dir, "garbage.pem", "not a certificate");
        let bad_der = write_file(
            &dir,
            "bad-der.pem",
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        );
        let good = write_file(&dir, "good.pem", &test_ca("onenote test CA").cert.pem());

        let mut warnings = Vec::new();
        let builder = client_builder_with(
            env(&[
                ("ONENOTE_EXTRA_CA_PEM", &good),
                ("SSL_CERT_FILE", &garbage),
                ("NODE_EXTRA_CA_CERTS", &bad_der),
            ]),
            |w| warnings.push(w),
        )
        .unwrap();
        builder.build().unwrap();

        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("SSL_CERT_FILE") && warnings[0].contains(&garbage));
        assert!(warnings[1].contains("NODE_EXTRA_CA_CERTS") && warnings[1].contains(&bad_der));
        assert!(
            warnings.iter().all(|w| !w.contains('\n')),
            "one line each: {warnings:?}"
        );
    }

    #[test]
    fn tls_hint_matches_unknown_issuer_chains_only() {
        let tls = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, tls);
        assert_eq!(tls_hint(&io), Some(TLS_HINT));

        let wrapped = anyhow::Error::new(io).context("Graph HTTP request failed (GET https://x)");
        assert_eq!(tls_hint_for(&wrapped), Some(TLS_HINT));

        let text_only = anyhow!("error:0A000086:SSL routines::self-signed certificate in chain");
        assert_eq!(tls_hint_for(&text_only), Some(TLS_HINT));

        let refused = std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "No connection could be made because the target machine actively refused it.",
        );
        assert_eq!(tls_hint(&refused), None);
        assert_eq!(
            tls_hint_for(&anyhow::Error::new(refused).context("request failed")),
            None
        );

        // Another certificate problem must not be reported as an unknown issuer.
        let expired = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::Expired),
        );
        assert_eq!(tls_hint(&expired), None);
    }

    /// Serves `ok` over TLS on 127.0.0.1 with a leaf certificate issued by `ca`.
    fn spawn_tls_server(ca: &TestCa) -> u16 {
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = params.signed_by(&leaf_key, &ca.cert, &ca.key).unwrap();

        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![leaf.der().clone()], key)
        .unwrap();
        let config = Arc::new(config);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut tcp) = stream else { continue };
                let Ok(mut conn) = rustls::ServerConnection::new(config.clone()) else {
                    continue;
                };
                let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                let complete = loop {
                    match tls.read(&mut buf) {
                        // A client that rejects the certificate ends up here.
                        Ok(0) | Err(_) => break false,
                        Ok(n) => {
                            request.extend_from_slice(&buf[..n]);
                            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                break true;
                            }
                        }
                    }
                };
                if complete {
                    let _ = tls.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    );
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn private_ca_fails_with_the_hint_and_succeeds_with_onenote_extra_ca_pem() {
        let ca = test_ca("onenote private test CA");
        let port = spawn_tls_server(&ca);
        let url = format!("https://127.0.0.1:{port}/");

        // Without the variable the private CA is unknown and the error carries the hint.
        let untrusting = client_builder_with(env(&[]), |_| {})
            .unwrap()
            .no_proxy()
            .build()
            .unwrap();
        let err = untrusting.get(&url).send().await.unwrap_err();
        assert_eq!(tls_hint(&err), Some(TLS_HINT), "{err:?}");
        let wrapped = transport_error(err, "Graph HTTP request failed (GET /)");
        let top = wrapped.to_string();
        assert!(
            top.starts_with("Graph HTTP request failed (GET /)"),
            "{top}"
        );
        assert!(top.contains("ONENOTE_EXTRA_CA_PEM=<path>"), "{top}");
        assert_eq!(tls_hint_for(&wrapped), Some(TLS_HINT));

        // With the variable the same server is trusted.
        let dir = tempfile::tempdir().unwrap();
        let ca_file = write_file(&dir, "corp-root.pem", &ca.cert.pem());
        let trusting = client_builder_with(env(&[("ONENOTE_EXTRA_CA_PEM", &ca_file)]), |_| {})
            .unwrap()
            .no_proxy()
            .build()
            .unwrap();
        let resp = trusting.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "ok");

        // A connection failure is not a certificate problem.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        let err = trusting
            .get(format!("https://127.0.0.1:{closed_port}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(tls_hint(&err), None, "{err:?}");
    }
}
