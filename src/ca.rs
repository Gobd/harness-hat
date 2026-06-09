/// Certificate Authority management for the harness-hat MITM proxy.
///
/// Generates a self-signed CA on first run and persists it to disk.
/// Derives per-domain leaf certificates on demand (cached in memory).
/// The CA cert PEM is exposed so it can be injected into containers.
use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use lru::LruCache;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

use crate::fs_util::{set_private_file_permissions, write_private_file};

/// Upper bound on the in-memory leaf-cert cache. The cache is keyed on the
/// attacker-controllable CONNECT/SNI host, so it must be bounded to avoid a
/// memory-exhaustion DoS. Evicted entries are simply regenerated on next use.
const CERT_CACHE_CAPACITY: usize = 1024;

/// CA signing material. Held behind an `Arc` so it can be moved into the
/// `spawn_blocking` closure that performs leaf-key generation without borrowing
/// `&self` across the await point.
struct SigningMaterial {
    ca_key: KeyPair,
    /// Reconstructed CA cert for signing leaf certs (may differ in validity
    /// period from the on-disk cert, but uses the same key and DN).
    ca_cert_for_signing: rcgen::Certificate,
    /// Original CA cert DER — included in leaf cert chains so TLS clients
    /// can verify the chain against what they imported.
    ca_cert_der: Vec<u8>,
}

pub struct CaStore {
    /// CA cert PEM — inject this into containers so they trust the proxy.
    pub cert_pem: String,
    signing: Arc<SigningMaterial>,
    /// Bounded cache of per-domain `ServerConfig`s. Each slot holds a
    /// `OnceCell` so that concurrent first-misses for the same host coalesce
    /// into a single keygen rather than duplicating work.
    cert_cache: Mutex<LruCache<String, Arc<OnceCell<Arc<ServerConfig>>>>>,
}

impl CaStore {
    /// Load the CA from `dir`, or generate and persist a new one.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let cert_path = dir.join("ca.crt");
        let key_path = dir.join("ca.key");

        if cert_path.exists() && key_path.exists() {
            return Self::load(&cert_path, &key_path);
        }
        Self::generate_and_save(&cert_path, &key_path)
    }

    fn new_cache() -> Mutex<LruCache<String, Arc<OnceCell<Arc<ServerConfig>>>>> {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(CERT_CACHE_CAPACITY).expect("cache capacity is non-zero"),
        ))
    }

    fn load(cert_path: &Path, key_path: &Path) -> Result<Self> {
        set_private_file_permissions(key_path)?;
        let cert_pem = std::fs::read_to_string(cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        let key_pem = std::fs::read_to_string(key_path)
            .with_context(|| format!("reading {}", key_path.display()))?;

        let ca_key = KeyPair::from_pem(&key_pem).context("parsing CA private key")?;

        // Reconstruct a signable Certificate from the same DN (fixed values).
        let ca_cert_for_signing = Self::build_ca_cert(&ca_key)?;

        // Extract the original DER bytes from the PEM for chain inclusion.
        let ca_cert_der = Self::pem_to_der(&cert_pem)?;

        Ok(Self {
            cert_pem,
            signing: Arc::new(SigningMaterial {
                ca_key,
                ca_cert_for_signing,
                ca_cert_der,
            }),
            cert_cache: Self::new_cache(),
        })
    }

    fn generate_and_save(cert_path: &Path, key_path: &Path) -> Result<Self> {
        let ca_key = KeyPair::generate().context("generating CA key pair")?;
        let ca_cert = Self::build_ca_cert(&ca_key)?;

        let cert_pem = ca_cert.pem();
        let key_pem = ca_key.serialize_pem();
        let ca_cert_der = ca_cert.der().to_vec();

        std::fs::write(cert_path, &cert_pem)
            .with_context(|| format!("writing {}", cert_path.display()))?;
        write_private_file(key_path, key_pem.as_bytes())
            .with_context(|| format!("writing {}", key_path.display()))?;

        Ok(Self {
            cert_pem,
            signing: Arc::new(SigningMaterial {
                ca_key,
                ca_cert_for_signing: ca_cert,
                ca_cert_der,
            }),
            cert_cache: Self::new_cache(),
        })
    }

    fn build_ca_cert(key: &KeyPair) -> Result<rcgen::Certificate> {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        // Constrain the CA key to what it is actually used for. Without this an
        // rcgen CA cert ships with no KeyUsage extension at all.
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
            .distinguished_name
            .push(DnType::CommonName, "Harness Hat Proxy CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "harness-hat");
        // 10-year validity (renew by deleting ~/.harness-hat/ca.{crt,key}).
        // rcgen emits a SubjectKeyIdentifier by default.
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2034, 1, 1);
        params.self_signed(key).context("generating CA certificate")
    }

    fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
        const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
        const END: &str = "-----END CERTIFICATE-----";

        let start = pem.find(BEGIN).context("no certificate found in CA PEM")? + BEGIN.len();
        let end = pem[start..]
            .find(END)
            .map(|idx| start + idx)
            .context("unterminated certificate PEM")?;
        let b64 = pem[start..end]
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        STANDARD.decode(b64).context("parsing CA cert PEM")
    }

    /// Synchronously sign a leaf `ServerConfig` for `domain`. CPU-bound (RSA/EC
    /// keygen + signing); callers run this on a blocking thread.
    fn sign_leaf(signing: &SigningMaterial, domain: &str) -> Result<Arc<ServerConfig>> {
        let leaf_key = KeyPair::generate().context("generating leaf key")?;
        let mut params =
            CertificateParams::new(vec![domain.to_string()]).context("building leaf params")?;
        params.is_ca = IsCa::NoCa;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2034, 1, 1);

        let leaf_cert = params
            .signed_by(&leaf_key, &signing.ca_cert_for_signing, &signing.ca_key)
            .context("signing leaf certificate")?;

        // Chain: leaf + original CA cert (what the container's trust store knows).
        let cert_chain: Vec<CertificateDer<'static>> = vec![
            CertificateDer::from(leaf_cert.der().to_vec()),
            CertificateDer::from(signing.ca_cert_der.clone()),
        ];

        // Private key for the leaf cert.
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));

        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key_der)
            .context("building leaf ServerConfig")?;

        Ok(Arc::new(server_config))
    }

    /// Return (or generate and cache) a rustls `ServerConfig` presenting a
    /// leaf certificate for `domain`, signed by this CA.
    ///
    /// Keygen runs on a blocking thread; concurrent first-misses for the same
    /// host coalesce so the work is only done once per cache slot.
    pub async fn leaf_server_config(&self, domain: &str) -> Result<Arc<ServerConfig>> {
        let cell = {
            let mut cache = self
                .cert_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            cache
                .get_or_insert(domain.to_string(), || Arc::new(OnceCell::new()))
                .clone()
        };

        let config = cell
            .get_or_try_init(|| {
                let signing = Arc::clone(&self.signing);
                let domain = domain.to_string();
                async move {
                    tokio::task::spawn_blocking(move || Self::sign_leaf(&signing, &domain))
                        .await
                        .context("leaf keygen task panicked")?
                }
            })
            .await?;

        Ok(Arc::clone(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_or_create_is_idempotent() {
        let dir = tempdir().expect("create temp dir");
        let path = dir.path();

        // 1. Create for the first time
        let store1 = CaStore::load_or_create(path).expect("first create");
        let cert1 = store1.cert_pem.clone();
        assert!(path.join("ca.crt").exists());
        assert!(path.join("ca.key").exists());

        // 2. Load again from the same dir
        let store2 = CaStore::load_or_create(path).expect("second load");
        assert_eq!(
            store2.cert_pem, cert1,
            "CA certificate should be persistent"
        );
    }

    #[tokio::test]
    async fn leaf_server_config_caches_results() {
        let dir = tempdir().expect("create temp dir");
        let store = CaStore::load_or_create(dir.path()).expect("create store");

        let config1 = store
            .leaf_server_config("example.com")
            .await
            .expect("first leaf");
        let config2 = store
            .leaf_server_config("example.com")
            .await
            .expect("second leaf");

        assert!(
            Arc::ptr_eq(&config1, &config2),
            "server configs should be cached"
        );

        let config3 = store
            .leaf_server_config("other.com")
            .await
            .expect("different domain");
        assert!(
            !Arc::ptr_eq(&config1, &config3),
            "different domains should have different configs"
        );
    }
}
