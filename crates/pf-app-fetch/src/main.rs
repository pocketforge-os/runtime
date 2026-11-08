use std::sync::Arc;
fn main() {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(provider)
        .build();
    let _agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .max_redirects(0)
        .build()
        .into();
    let _ = tar::Archive::new(std::io::empty());
}
