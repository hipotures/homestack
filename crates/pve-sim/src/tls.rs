//! Throwaway certificates for the simulator: a CA and a server certificate
//! for `localhost` and `127.0.0.1` signed by it.

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};

pub struct Certs {
    pub ca_pem: String,
    pub chain_pem: String,
    pub key_pem: String,
}

pub fn generate() -> Result<Certs, rcgen::Error> {
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.distinguished_name.push(DnType::CommonName, "pve-sim test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let issuer = Issuer::new(ca_params, ca_key);

    let mut server_params = CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    server_params.distinguished_name.push(DnType::CommonName, "localhost");
    let server_key = KeyPair::generate()?;
    let server_cert = server_params.signed_by(&server_key, &issuer)?;

    Ok(Certs {
        ca_pem: ca_cert.pem(),
        chain_pem: server_cert.pem(),
        key_pem: server_key.serialize_pem(),
    })
}
