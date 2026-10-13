// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{sync::Arc, time::Duration};

use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, ServerName},
    ClientConfig, RootCertStore, ServerConfig,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::{BridgeError, ErrorCode, Result, Transport};

const ALPN: &[u8] = b"drasi-bridge/1";

pub trait BridgeIo: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> BridgeIo for T {}
pub type BoxIo = Box<dyn BridgeIo>;

/// Host-owned identity resource. Constructors require normal WebPKI validation,
/// mutual authentication and an exact expected peer leaf-certificate pin.
/// Data credentials provide no process-management authority.
pub struct TlsResource {
    client: Option<TlsConnector>,
    server: Option<TlsAcceptor>,
    peer_certificate_sha256: [u8; 32],
}

impl TlsResource {
    pub fn client(
        roots: RootCertStore,
        certificate_chain: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
        peer_certificate_sha256: [u8; 32],
    ) -> anyhow::Result<Self> {
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_client_auth_cert(certificate_chain, private_key)?;
        config.alpn_protocols = vec![ALPN.to_vec()];
        Ok(Self {
            client: Some(TlsConnector::from(Arc::new(config))),
            server: None,
            peer_certificate_sha256,
        })
    }

    pub fn server(
        client_roots: RootCertStore,
        certificate_chain: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
        peer_certificate_sha256: [u8; 32],
    ) -> anyhow::Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(client_roots),
            provider.clone(),
        )
        .build()?;
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(verifier)
            .with_single_cert(certificate_chain, private_key)?;
        config.alpn_protocols = vec![ALPN.to_vec()];
        Ok(Self {
            client: None,
            server: Some(TlsAcceptor::from(Arc::new(config))),
            peer_certificate_sha256,
        })
    }

    pub(crate) fn validate_role(&self, source: bool) -> Result<()> {
        if (source && self.server.is_none()) || (!source && self.client.is_none()) {
            return Err(BridgeError::invalid(
                "TLS resource has the wrong endpoint role",
            ));
        }
        Ok(())
    }

    fn verify(
        &self,
        certificates: Option<&[CertificateDer<'_>]>,
        alpn: Option<&[u8]>,
    ) -> Result<()> {
        let certificate = certificates
            .and_then(|chain| chain.first())
            .ok_or_else(|| {
                BridgeError::new(
                    ErrorCode::Incompatible,
                    "mutual TLS peer certificate is required",
                )
            })?;
        let digest: [u8; 32] = Sha256::digest(certificate.as_ref()).into();
        if digest != self.peer_certificate_sha256 || alpn != Some(ALPN) {
            return Err(BridgeError::new(
                ErrorCode::Incompatible,
                "TLS peer certificate pin or bridge ALPN mismatch",
            ));
        }
        Ok(())
    }
}

pub(crate) async fn connect(
    transport: &Transport,
    control: bool,
    tls: Option<&TlsResource>,
    timeout: Duration,
) -> Result<BoxIo> {
    deadline(timeout, async {
        match transport {
            #[cfg(unix)]
            Transport::Unix {
                data_path,
                control_path,
            } => {
                use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
                let path = if control { control_path } else { data_path };
                let metadata = std::fs::symlink_metadata(path).map_err(BridgeError::unknown)?;
                if !metadata.file_type().is_socket() || metadata.permissions().mode() & 0o077 != 0 {
                    return Err(BridgeError::invalid(
                        "Unix bridge socket must be owner-only",
                    ));
                }
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(BridgeError::unknown)?;
                if stream.peer_cred().map_err(BridgeError::unknown)?.uid() != metadata.uid() {
                    return Err(BridgeError::invalid(
                        "Unix peer does not own the configured socket",
                    ));
                }
                Ok(Box::new(stream) as BoxIo)
            }
            #[cfg(windows)]
            Transport::NamedPipe {
                data_pipe,
                control_pipe,
            } => {
                let path = if control { control_pipe } else { data_pipe };
                let stream = tokio::net::windows::named_pipe::ClientOptions::new()
                    .open(path)
                    .map_err(BridgeError::unknown)?;
                Ok(Box::new(stream) as BoxIo)
            }
            Transport::Tls {
                data_address,
                control_address,
                server_name,
            } => {
                let tls =
                    tls.ok_or_else(|| BridgeError::invalid("TLS identity resource is required"))?;
                tls.validate_role(false)?;
                let address = if control {
                    control_address
                } else {
                    data_address
                };
                let stream = tokio::net::TcpStream::connect(address)
                    .await
                    .map_err(BridgeError::unknown)?;
                stream.set_nodelay(true).map_err(BridgeError::unknown)?;
                let name = ServerName::try_from(server_name.clone())
                    .map_err(|_| BridgeError::invalid("invalid TLS server name"))?;
                let stream = tls
                    .client
                    .as_ref()
                    .expect("validated client")
                    .connect(name, stream)
                    .await
                    .map_err(BridgeError::unknown)?;
                let (_, connection) = stream.get_ref();
                tls.verify(connection.peer_certificates(), connection.alpn_protocol())?;
                Ok(Box::new(stream) as BoxIo)
            }
            #[allow(unreachable_patterns)]
            _ => Err(BridgeError::invalid(
                "transport is not supported on this platform",
            )),
        }
    })
    .await
}

pub(crate) enum Listener {
    #[cfg(unix)]
    Unix {
        listener: tokio::net::UnixListener,
        owner: UnixPath,
    },
    #[cfg(windows)]
    NamedPipe {
        next: tokio::net::windows::named_pipe::NamedPipeServer,
        path: String,
    },
    Tls {
        listener: tokio::net::TcpListener,
        identity: Arc<TlsResource>,
        timeout: Duration,
    },
}

impl Listener {
    pub(crate) async fn bind(
        transport: &Transport,
        control: bool,
        tls: Option<Arc<TlsResource>>,
        timeout: Duration,
    ) -> Result<Self> {
        match transport {
            #[cfg(unix)]
            Transport::Unix {
                data_path,
                control_path,
            } => {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let path = if control { control_path } else { data_path };
                // Never unlink an existing address: it can belong to a live owner.
                let listener =
                    tokio::net::UnixListener::bind(path).map_err(BridgeError::unknown)?;
                let metadata = std::fs::symlink_metadata(path).map_err(BridgeError::unknown)?;
                let owner = UnixPath {
                    path: path.into(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    uid: metadata.uid(),
                };
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(BridgeError::unknown)?;
                Ok(Self::Unix { listener, owner })
            }
            #[cfg(windows)]
            Transport::NamedPipe {
                data_pipe,
                control_pipe,
            } => {
                let path = if control { control_pipe } else { data_pipe };
                let next = tokio::net::windows::named_pipe::ServerOptions::new()
                    .first_pipe_instance(true)
                    .reject_remote_clients(true)
                    .max_instances(3)
                    .create(path)
                    .map_err(BridgeError::unknown)?;
                Ok(Self::NamedPipe {
                    next,
                    path: path.clone(),
                })
            }
            Transport::Tls {
                data_address,
                control_address,
                ..
            } => {
                let identity =
                    tls.ok_or_else(|| BridgeError::invalid("TLS identity resource is required"))?;
                identity.validate_role(true)?;
                let address = if control {
                    control_address
                } else {
                    data_address
                };
                let listener = tokio::net::TcpListener::bind(address)
                    .await
                    .map_err(BridgeError::unknown)?;
                Ok(Self::Tls {
                    listener,
                    identity,
                    timeout,
                })
            }
            #[allow(unreachable_patterns)]
            _ => Err(BridgeError::invalid(
                "transport is not supported on this platform",
            )),
        }
    }

    pub(crate) async fn accept(&mut self) -> Result<BoxIo> {
        match self {
            #[cfg(unix)]
            Self::Unix { listener, owner } => {
                let (stream, _) = listener.accept().await.map_err(BridgeError::unknown)?;
                if stream.peer_cred().map_err(BridgeError::unknown)?.uid() != owner.uid {
                    return Err(BridgeError::invalid(
                        "Unix bridge peers must use the socket owner's UID",
                    ));
                }
                Ok(Box::new(stream))
            }
            #[cfg(windows)]
            Self::NamedPipe { next, path } => {
                next.connect().await.map_err(BridgeError::unknown)?;
                let replacement = tokio::net::windows::named_pipe::ServerOptions::new()
                    .reject_remote_clients(true)
                    .max_instances(3)
                    .create(path.as_str())
                    .map_err(BridgeError::unknown)?;
                Ok(Box::new(std::mem::replace(next, replacement)))
            }
            Self::Tls {
                listener,
                identity,
                timeout,
            } => {
                let (stream, _) = listener.accept().await.map_err(BridgeError::unknown)?;
                stream.set_nodelay(true).map_err(BridgeError::unknown)?;
                deadline(*timeout, async {
                    let stream = identity
                        .server
                        .as_ref()
                        .expect("validated server")
                        .accept(stream)
                        .await
                        .map_err(BridgeError::unknown)?;
                    let (_, connection) = stream.get_ref();
                    identity.verify(connection.peer_certificates(), connection.alpn_protocol())?;
                    Ok(Box::new(stream) as BoxIo)
                })
                .await
            }
        }
    }
}

#[cfg(unix)]
pub(crate) struct UnixPath {
    path: std::path::PathBuf,
    device: u64,
    inode: u64,
    uid: u32,
}

#[cfg(unix)]
impl Drop for UnixPath {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            if metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

pub(crate) async fn deadline<T>(
    timeout: Duration,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| BridgeError::unknown("bridge deadline elapsed; acceptance must be resolved"))?
}
