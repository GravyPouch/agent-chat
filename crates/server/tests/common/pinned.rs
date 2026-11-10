//! A STARTTLS connector for tokio-xmpp that trusts one SPKI pin instead of
//! the system roots, and returns the `tls-exporter` channel binding so
//! SCRAM-PLUS is used. This is the shape of the daemon's planned
//! `PinnedStartTlsConnector` (§7.3, M6).

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;

use agent_chat_server::net::tls::spki_pin;
use futures::{SinkExt, StreamExt};
use sasl::common::ChannelBinding;
use tokio::io::BufStream;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, ProtocolVersion, SignatureScheme};
use tokio_xmpp::connect::ServerConnector;
use tokio_xmpp::xmlstream::{
    FallibleStreamElement, PendingFeaturesRecv, ReadError, StreamHeader, Timeouts, XmppStreamElement, initiate_stream,
};
use xmpp_parsers::jid::Jid;
use xmpp_parsers::starttls;

#[derive(Debug, Clone)]
pub struct PinnedStartTls {
    pub addr: SocketAddr,
    pub pin: String,
}

#[derive(Debug)]
struct PinVerifier {
    pin: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match spki_pin(end_entity.as_ref()) {
            Ok(p) if p == self.pin => Ok(ServerCertVerified::assertion()),
            _ => Err(rustls::Error::General("certificate does not match the pin".into())),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn io_err(e: impl std::fmt::Display) -> tokio_xmpp::Error {
    tokio_xmpp::Error::Io(std::io::Error::other(e.to_string()))
}

impl ServerConnector for PinnedStartTls {
    type Stream = BufStream<TlsStream<TcpStream>>;

    async fn connect(
        &self,
        jid: &Jid,
        ns: &'static str,
        timeouts: Timeouts,
    ) -> Result<(PendingFeaturesRecv<Self::Stream>, ChannelBinding), tokio_xmpp::Error> {
        let domain = jid.domain().as_str().to_owned();
        let tcp = TcpStream::connect(self.addr).await?;
        let header = || StreamHeader {
            to: Some(Cow::Owned(domain.clone())),
            from: None,
            id: None,
        };
        let pending = initiate_stream(BufStream::new(tcp), ns, header(), timeouts).await?;
        let (features, mut stream) = pending.recv_features::<FallibleStreamElement>().await.map_err(io_err)?;
        if !features.can_starttls() {
            return Err(io_err("the server does not offer STARTTLS"));
        }
        stream
            .send(&XmppStreamElement::Starttls(starttls::Nonza::Request(starttls::Request)))
            .await?;
        loop {
            match stream.next().await {
                Some(Ok(FallibleStreamElement::Ok(XmppStreamElement::Starttls(starttls::Nonza::Proceed(_))))) => break,
                Some(Ok(_)) | Some(Err(ReadError::SoftTimeout)) => continue,
                other => return Err(io_err(format!("STARTTLS failed: {other:?}"))),
            }
        }
        let tcp = stream.into_inner().into_inner();

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(io_err)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinVerifier {
                pin: self.pin.clone(),
                provider,
            }))
            .with_no_client_auth();
        let name = ServerName::try_from(domain.clone()).map_err(io_err)?;
        let tls = TlsConnector::from(Arc::new(config)).connect(name, tcp).await?;
        let (_, conn) = tls.get_ref();
        let cb = match conn.protocol_version() {
            Some(ProtocolVersion::TLSv1_3) => ChannelBinding::TlsExporter(
                conn.export_keying_material(vec![0u8; 32], b"EXPORTER-Channel-Binding", None)
                    .map_err(io_err)?,
            ),
            _ => ChannelBinding::None,
        };
        let pending = initiate_stream(BufStream::new(tls), ns, header(), timeouts).await?;
        Ok((pending, cb))
    }
}
