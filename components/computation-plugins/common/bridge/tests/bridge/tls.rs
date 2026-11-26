// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{
    client::Client,
    protocol::{read_message, write_message, DataHello, DataResponse, Lane, CONTROL_BYTES},
    transport::connect,
    BridgeSink, BridgeSource, ErrorCode, TlsResourceRecipe,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSink, NativeSource};
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

fn pin_of(pin: [u8; 32]) -> String {
    pin.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Recipes whose paths are relative to the working directory, as endpoint
/// configuration resolves them.
fn recipe(
    directory: &Directory,
    name: &str,
    role: crate::TlsRole,
    pin: [u8; 32],
) -> TlsResourceRecipe {
    TlsResourceRecipe {
        kind: crate::TlsRecipeKind::BridgeTls,
        version: 1,
        role,
        trust_roots: vec![directory.path().join("ca.der")],
        certificate_chain: vec![directory.path().join(format!("{name}.der"))],
        private_key: directory.path().join(format!("{name}-key.der")),
        peer_certificate_sha256: pin_of(pin),
    }
}

fn restrict_keys(directory: &Directory) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for key in ["client-key.der", "server-key.der"] {
        std::fs::set_permissions(
            directory.path().join(key),
            std::fs::Permissions::from_mode(0o600),
        )?;
    }
    Ok(())
}

/// Both endpoint configurations over TLS on two free loopback ports.
fn tls_configurations(
    directory: &Directory,
    client: &TlsResourceRecipe,
    server: &TlsResourceRecipe,
) -> Result<(BridgeConfig, BridgeConfig, std::net::SocketAddr)> {
    let data = std::net::TcpListener::bind("127.0.0.1:0")?;
    let control = std::net::TcpListener::bind("127.0.0.1:0")?;
    let data_address = data.local_addr()?;
    let transport = Transport::Tls {
        data_address: data_address.to_string(),
        control_address: control.local_addr()?.to_string(),
        server_name: "localhost".into(),
    };
    let mut source_config =
        configuration(directory, true, Profile::Volatile, PortableDelivery::Events);
    let mut sink_config = configuration(
        directory,
        false,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    source_config.transport = transport.clone();
    source_config.tls = Some(server.clone());
    sink_config.transport = transport;
    sink_config.tls = Some(client.clone());
    Ok((source_config, sink_config, data_address))
}

#[tokio::test(flavor = "current_thread")]
async fn mutual_tls_uses_the_same_protocol_and_rejects_wrong_peer_pins_and_plaintext() -> Result<()>
{
    let directory = Directory::new()?;
    certificates(&directory)?;
    let (_, _, _, client_pin) = material(&directory, "client")?;
    let (_, _, _, server_pin) = material(&directory, "server")?;
    let client_recipe = recipe(&directory, "client", crate::TlsRole::Client, server_pin);
    let server_recipe = recipe(&directory, "server", crate::TlsRole::Server, client_pin);
    let here = Path::new(".");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        directory.path().join("client-key.der"),
        std::fs::Permissions::from_mode(0o644),
    )?;
    assert!(client_recipe.resolve(here).is_err());
    restrict_keys(&directory)?;
    let mut invalid = client_recipe.clone();
    invalid.peer_certificate_sha256 = "bad-pin".into();
    assert!(invalid.validate().is_err());
    std::fs::write(directory.path().join("oversized.der"), vec![0; 65_537])?;
    invalid = client_recipe.clone();
    invalid.trust_roots = vec![directory.path().join("oversized.der")];
    assert!(invalid.resolve(here).is_err());
    std::os::unix::fs::symlink("client-key.der", directory.path().join("key-link.der"))?;
    invalid = client_recipe.clone();
    invalid.private_key = directory.path().join("key-link.der");
    assert!(invalid.resolve(here).is_err());
    let (source_config, sink_config, data_address) =
        tls_configurations(&directory, &client_recipe, &server_recipe)?;
    let mut mismatched = source_config.clone();
    mismatched.tls = None;
    assert!(
        BridgeSource::new(mismatched, GRAPH.into(), None).is_err(),
        "the TLS transport requires its identity"
    );
    let mut mismatched = sink_config.clone();
    mismatched.tls = Some(server_recipe.clone());
    assert!(
        BridgeSink::new(mismatched, GRAPH.into()).is_err(),
        "a sender presents a client identity"
    );
    let mut source = BridgeSource::new(source_config.clone(), GRAPH.into(), None)?;
    let mut sink = BridgeSink::new(sink_config.clone(), GRAPH.into())?;
    source.start(&mut lifecycle_ctx::start()).await?;
    sink.start(&mut lifecycle_ctx::start()).await?;
    let input = graph_input(77, 18)?;
    sink.handle(super::input(input.clone())).await?;
    let control = sink.control().expect("started sink control");
    assert_eq!(control.status().await?.accepted, 1);
    let output = source.next().await?.unwrap().envelope;
    assert_eq!(output.changes().operations(), input.changes().operations());
    assert_eq!(output.system().timestamp(), input.system().timestamp());
    let cut = finish(&sink, &sink_config, Some(client_recipe.resolve(here)?)).await?;
    let mut observer = observe(
        &source_config,
        Some(server_recipe.resolve_local_observer(here)?),
    )
    .await?;
    assert!(observer.wait_drained(&cut, DEADLINE).await?.drained);
    drop(observer);
    sink.stop(&lifecycle_ctx::stop()).await?;

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

    let mut wrong = sink_config;
    if let Some(tls) = &mut wrong.tls {
        tls.peer_certificate_sha256 = pin_of([0; 32]);
    }
    let mut rejected = BridgeSink::new(wrong, GRAPH.into())?;
    rejected.start(&mut lifecycle_ctx::start()).await?;
    let error = rejected
        .handle(super::input(graph_input(78, 19)?))
        .await
        .unwrap_err();
    assert_eq!(error.bridge_cause().unwrap().code, ErrorCode::Incompatible);
    assert_eq!(source.status().accepted, 1);
    rejected.stop(&lifecycle_ctx::stop()).await?;
    source.stop(&lifecycle_ctx::stop()).await?;
    Ok(())
}

/// The receiver's own certificate observes and seals it over the control
/// lane, but is never a sender: its Hello and data connections are refused.
#[tokio::test(flavor = "current_thread")]
async fn receiver_local_observer_observes_and_seals_but_never_sends() -> Result<()> {
    let directory = Directory::new()?;
    certificates(&directory)?;
    restrict_keys(&directory)?;
    let (_, _, _, client_pin) = material(&directory, "client")?;
    let (_, _, _, server_pin) = material(&directory, "server")?;
    let client_recipe = recipe(&directory, "client", crate::TlsRole::Client, server_pin);
    let server_recipe = recipe(&directory, "server", crate::TlsRole::Server, client_pin);
    let here = Path::new(".");
    assert!(
        client_recipe.resolve_local_observer(here).is_err(),
        "only a server identity observes its own receiver"
    );
    let local = server_recipe.resolve_local_observer(here)?;
    let (source_config, sink_config, _) =
        tls_configurations(&directory, &client_recipe, &server_recipe)?;
    let mut source = BridgeSource::new(source_config.clone(), GRAPH.into(), None)?;
    source.start(&mut lifecycle_ctx::start()).await?;

    let mut impostor = Client::new(sink_config.clone(), Some(local.clone()));
    let error = impostor
        .connect(None)
        .await
        .expect_err("the receiver's own identity cannot open a producer session");
    assert_eq!(error.code, crate::ErrorCode::Incompatible, "{error}");
    let refused = async {
        let mut io = connect(&sink_config.transport, false, Some(&local), DEADLINE).await?;
        let hello = DataHello {
            version: crate::PROTOCOL_VERSION,
            generation: 1,
            session: ProducerSession {
                incarnation: uuid::Uuid::new_v4(),
                producer: ComponentId::try_new("6098fde0-6751-5c81-8647-31b2b083b961")
                    .expect("fixture UUID"),
                epoch: 1,
            },
            contract: sink_config.contract(false).fingerprint(),
        };
        write_message(&mut io, Lane::DataHello, &hello, CONTROL_BYTES).await?;
        read_message::<_, DataResponse>(&mut io, Lane::Receipt, CONTROL_BYTES).await
    }
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(source.status().generation, 0, "no sender was welcomed");

    let mut observer = observe(&source_config, Some(local)).await?;
    assert!(observer.sealed_cut().await?.is_none());
    let cut = observer.seal(DEADLINE).await?;
    assert_eq!((cut.client_sequence, cut.position), (0, 0));
    assert_eq!(observer.sealed_cut().await?, Some(cut.clone()));
    assert!(observer.drain_status(&cut).await?.drained);

    let mut sink = BridgeSink::new(sink_config, GRAPH.into())?;
    sink.start(&mut lifecycle_ctx::start()).await?;
    let error = sink
        .handle(super::input(graph_input(1, 1)?))
        .await
        .unwrap_err();
    assert_eq!(error.bridge_cause().unwrap().code, ErrorCode::Sealed);
    sink.stop(&lifecycle_ctx::stop()).await?;
    source.stop(&lifecycle_ctx::stop()).await?;
    Ok(())
}
