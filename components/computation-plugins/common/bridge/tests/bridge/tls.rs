// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{BridgeError, BridgeSink, BridgeSource, ErrorCode, TlsResource};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    RootCertStore,
};
use sha2::{Digest, Sha256};
use std::process::{Command, Stdio};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn openssl(directory: &Directory, arguments: &[&str]) -> Result<()> {
    let output = Command::new("openssl")
        .current_dir(directory.path())
        .env("RANDFILE", "random.seed")
        .args(arguments)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "test certificate generation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn certificates(directory: &Directory) -> Result<()> {
    std::fs::write(directory.path().join("ca.cnf"), "[req]\nprompt=no\ndistinguished_name=dn\nx509_extensions=ca\n[dn]\nCN=Drasi Bridge Test CA\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n")?;
    openssl(
        directory,
        &[
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem",
            "-days", "1", "-sha256", "-config", "ca.cnf",
        ],
    )?;
    openssl(
        directory,
        &["x509", "-in", "ca.pem", "-outform", "DER", "-out", "ca.der"],
    )?;
    for name in ["server", "client"] {
        std::fs::write(directory.path().join(format!("{name}.ext")), "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:localhost\n")?;
        openssl(
            directory,
            &[
                "req",
                "-new",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                &format!("{name}.key"),
                "-out",
                &format!("{name}.csr"),
                "-subj",
                "/CN=localhost",
                "-config",
                "ca.cnf",
            ],
        )?;
        openssl(
            directory,
            &[
                "x509",
                "-req",
                "-in",
                &format!("{name}.csr"),
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-out",
                &format!("{name}.pem"),
                "-days",
                "1",
                "-sha256",
                "-extfile",
                &format!("{name}.ext"),
            ],
        )?;
        openssl(
            directory,
            &[
                "x509",
                "-in",
                &format!("{name}.pem"),
                "-outform",
                "DER",
                "-out",
                &format!("{name}.der"),
            ],
        )?;
        openssl(
            directory,
            &[
                "pkcs8",
                "-topk8",
                "-nocrypt",
                "-in",
                &format!("{name}.key"),
                "-outform",
                "DER",
                "-out",
                &format!("{name}-key.der"),
            ],
        )?;
    }
    Ok(())
}

fn material(
    directory: &Directory,
    name: &str,
) -> Result<(
    RootCertStore,
    Vec<CertificateDer<'static>>,
    PrivateKeyDer<'static>,
    [u8; 32],
)> {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(std::fs::read(
        directory.path().join("ca.der"),
    )?))?;
    let certificate = std::fs::read(directory.path().join(format!("{name}.der")))?;
    let pin = Sha256::digest(&certificate).into();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(std::fs::read(
        directory.path().join(format!("{name}-key.der")),
    )?));
    Ok((roots, vec![CertificateDer::from(certificate)], key, pin))
}

#[tokio::test(flavor = "current_thread")]
async fn mutual_tls_uses_the_same_protocol_and_rejects_wrong_peer_pins_and_plaintext() -> Result<()>
{
    let directory = Directory::new()?;
    certificates(&directory)?;
    let (_, _, _, client_pin) = material(&directory, "client")?;
    let (_, _, _, server_pin) = material(&directory, "server")?;
    let recipe = |name: &str, role, pin: [u8; 32]| crate::TlsResourceRecipe {
        kind: crate::TlsRecipeKind::BridgeTls,
        version: 1,
        role,
        trust_roots: vec!["ca.der".into()],
        certificate_chain: vec![format!("{name}.der").into()],
        private_key: format!("{name}-key.der").into(),
        peer_certificate_sha256: pin.iter().map(|byte| format!("{byte:02x}")).collect(),
    };
    let client_recipe = recipe("client", crate::TlsRole::Client, server_pin);
    let server_recipe = recipe("server", crate::TlsRole::Server, client_pin);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        directory.path().join("client-key.der"),
        std::fs::Permissions::from_mode(0o644),
    )?;
    assert!(client_recipe.resolve(directory.path()).is_err());
    for key in ["client-key.der", "server-key.der"] {
        std::fs::set_permissions(
            directory.path().join(key),
            std::fs::Permissions::from_mode(0o600),
        )?;
    }
    let mut invalid = client_recipe.clone();
    invalid.peer_certificate_sha256 = "bad-pin".into();
    assert!(invalid.validate().is_err());
    std::fs::write(directory.path().join("oversized.der"), vec![0; 65_537])?;
    invalid = client_recipe.clone();
    invalid.trust_roots = vec!["oversized.der".into()];
    assert!(invalid.resolve(directory.path()).is_err());
    std::os::unix::fs::symlink("client-key.der", directory.path().join("key-link.der"))?;
    invalid = client_recipe.clone();
    invalid.private_key = "key-link.der".into();
    assert!(invalid.resolve(directory.path()).is_err());
    let client = client_recipe.resolve(directory.path())?;
    let server = server_recipe.resolve(directory.path())?;
    let data = std::net::TcpListener::bind("127.0.0.1:0")?;
    let control = std::net::TcpListener::bind("127.0.0.1:0")?;
    let data_address = data.local_addr()?;
    let control_address = control.local_addr()?;
    let transport = Transport::Tls {
        data_address: data_address.to_string(),
        control_address: control_address.to_string(),
        server_name: "localhost".into(),
    };
    let mut source_config = configuration(
        &directory,
        true,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    let mut sink_config = configuration(
        &directory,
        false,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    source_config.transport = transport.clone();
    sink_config.transport = transport;
    drop(data);
    drop(control);
    let mut source = BridgeSource::new(source_config, GRAPH.into(), None, Some(server))?;
    let mut sink = BridgeSink::new(sink_config.clone(), GRAPH.into(), None, Some(client))?;
    source.start().await?;
    sink.start().await?;
    let input = graph_input(77, 18)?;
    sink.handle(super::input(input.clone())).await?;
    assert_eq!(sink.control().status().await?.accepted, 1);
    let output = source.next().await?.unwrap().envelope;
    assert_eq!(output.changes().operations(), input.changes().operations());
    assert_eq!(output.system().timestamp(), input.system().timestamp());
    let cut = sink.egress_control().finish(DEADLINE).await?;
    assert!(
        source
            .ingress_control()
            .wait_drained(&cut, DEADLINE)
            .await?
            .drained
    );
    sink.stop().await?;

    let mut plaintext = tokio::net::TcpStream::connect(data_address).await?;
    plaintext
        .write_all(b"DRBG\0\x01\x02\0\0\0\0\x01\xc0")
        .await?;
    plaintext.shutdown().await?;
    let mut reply = [0; 64];
    let result = tokio::time::timeout(DEADLINE, plaintext.read(&mut reply)).await?;
    if let Ok(length) = result {
        assert!(
            length == 0 || !reply[..length].starts_with(b"DRBG"),
            "plaintext must never negotiate a bridge"
        );
    }

    let (roots, chain, key, _) = material(&directory, "client")?;
    let wrong = Arc::new(TlsResource::client(roots, chain, key, [0; 32])?);
    let mut rejected = BridgeSink::new(sink_config, GRAPH.into(), None, Some(wrong))?;
    rejected.start().await?;
    let error = rejected
        .handle(super::input(graph_input(78, 19)?))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<BridgeError>().unwrap().code,
        ErrorCode::Incompatible
    );
    assert_eq!(source.status().accepted, 1);
    rejected.stop().await?;
    source.stop().await?;
    Ok(())
}
