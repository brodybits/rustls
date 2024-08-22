use crate::alias::Arc;
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use pki_types::ServerName;
pub(super) use server_hello::CompleteServerHelloHandling;
use subtle::ConstantTimeEq;

use super::client_conn::ClientConnectionData;
use super::hs::ClientContext;
use crate::check::{inappropriate_handshake_message, inappropriate_message};
use crate::client::common::{ClientAuthDetails, ServerCertDetails};
use crate::client::{hs, ClientConfig};
use crate::common_state::{CommonState, HandshakeKind, KxState, Side, State};
use crate::conn::ConnectionRandoms;
use crate::crypto::KeyExchangeAlgorithm;
use crate::enums::{AlertDescription, ContentType, HandshakeType, ProtocolVersion};
use crate::error::{Error, InvalidMessage, PeerIncompatible, PeerMisbehaved};
use crate::hash_hs::HandshakeHash;
#[cfg(feature = "logging")]
use crate::log::{debug, trace, warn};
use crate::msgs::base::{Payload, PayloadU16, PayloadU8};
use crate::msgs::ccs::ChangeCipherSpecPayload;
use crate::msgs::handshake::{
    CertificateChain, ClientDhParams, ClientEcdhParams, ClientKeyExchangeParams,
    HandshakeMessagePayload, HandshakePayload, NewSessionTicketPayload, ServerKeyExchangeParams,
    SessionId,
};
use crate::msgs::message::{Message, MessagePayload};
use crate::msgs::persist;
use crate::sign::Signer;
use crate::suites::{PartiallyExtractedSecrets, SupportedCipherSuite};
use crate::tls12::{self, ConnectionSecrets, Tls12CipherSuite};
use crate::verify::{self, DigitallySignedStruct};

mod server_hello {
    use super::*;
    use crate::msgs::enums::ExtensionType;
    use crate::msgs::handshake::{HasServerExtensions, ServerHelloPayload};

    pub(in crate::client) struct CompleteServerHelloHandling {
        pub(in crate::client) config: Arc<ClientConfig>,
        pub(in crate::client) resuming_session: Option<persist::Tls12ClientSessionValue>,
        pub(in crate::client) server_name: ServerName<'static>,
        pub(in crate::client) randoms: ConnectionRandoms,
        pub(in crate::client) using_ems: bool,
        pub(in crate::client) transcript: HandshakeHash,
    }

    impl CompleteServerHelloHandling {
        pub(in crate::client) fn handle_server_hello(
            mut self,
            cx: &mut ClientContext<'_>,
            suite: &'static Tls12CipherSuite,
            server_hello: &ServerHelloPayload,
            tls13_supported: bool,
        ) -> hs::NextStateOrError<'static> {
            self.randoms
                .server
                .clone_from_slice(&server_hello.random.0[..]);

            // Look for TLS1.3 downgrade signal in server random
            // both the server random and TLS12_DOWNGRADE_SENTINEL are
            // public values and don't require constant time comparison
            let has_downgrade_marker = self.randoms.server[24..] == tls12::DOWNGRADE_SENTINEL;
            if tls13_supported && has_downgrade_marker {
                return Err({
                    cx.common.send_fatal_alert(
                        AlertDescription::IllegalParameter,
                        PeerMisbehaved::AttemptedDowngradeToTls12WhenTls13IsSupported,
                    )
                });
            }

            // Doing EMS?
            self.using_ems = server_hello.ems_support_acked();
            if self.config.require_ems && !self.using_ems {
                return Err({
                    cx.common.send_fatal_alert(
                        AlertDescription::HandshakeFailure,
                        PeerIncompatible::ExtendedMasterSecretExtensionRequired,
                    )
                });
            }

            // Might the server send a ticket?
            let must_issue_new_ticket = if server_hello
                .find_extension(ExtensionType::SessionTicket)
                .is_some()
            {
                debug!("Server supports tickets");
                true
            } else {
                false
            };

            // Might the server send a CertificateStatus between Certificate and
            // ServerKeyExchange?
            let may_send_cert_status = server_hello
                .find_extension(ExtensionType::StatusRequest)
                .is_some();
            if may_send_cert_status {
                debug!("Server may staple OCSP response");
            }

            // See if we're successfully resuming.
            if let Some(resuming) = self.resuming_session {
                if resuming.session_id == server_hello.session_id {
                    debug!("Server agreed to resume");

                    // Is the server telling lies about the ciphersuite?
                    if resuming.suite() != suite {
                        return Err(PeerMisbehaved::ResumptionOfferedWithVariedCipherSuite.into());
                    }

                    // And about EMS support?
                    if resuming.extended_ms() != self.using_ems {
                        return Err(PeerMisbehaved::ResumptionOfferedWithVariedEms.into());
                    }

                    let secrets =
                        ConnectionSecrets::new_resume(self.randoms, suite, resuming.secret());
                    self.config.key_log.log(
                        "CLIENT_RANDOM",
                        &secrets.randoms.client,
                        &secrets.master_secret,
                    );
                    cx.common
                        .start_encryption_tls12(&secrets, Side::Client);

                    // Since we're resuming, we verified the certificate and
                    // proof of possession in the prior session.
                    cx.common.peer_certificates = Some(
                        resuming
                            .server_cert_chain()
                            .clone()
                            .into_owned(),
                    );
                    cx.common.handshake_kind = Some(HandshakeKind::Resumed);
                    let cert_verified = verify::ServerCertVerified::assertion();
                    let sig_verified = verify::HandshakeSignatureValid::assertion();

                    return if must_issue_new_ticket {
                        Ok(Box::new(ExpectNewTicket {
                            config: self.config,
                            secrets,
                            resuming_session: Some(resuming),
                            session_id: server_hello.session_id,
                            server_name: self.server_name,
                            using_ems: self.using_ems,
                            transcript: self.transcript,
                            resuming: true,
                            cert_verified,
                            sig_verified,
                        }))
                    } else {
                        Ok(Box::new(ExpectCcs {
                            config: self.config,
                            secrets,
                            resuming_session: Some(resuming),
                            session_id: server_hello.session_id,
                            server_name: self.server_name,
                            using_ems: self.using_ems,
                            transcript: self.transcript,
                            ticket: None,
                            resuming: true,
                            cert_verified,
                            sig_verified,
                        }))
                    };
                }
            }

            cx.common.handshake_kind = Some(HandshakeKind::Full);
            Ok(Box::new(ExpectCertificate {
                config: self.config,
                resuming_session: None,
                session_id: server_hello.session_id,
                server_name: self.server_name,
                randoms: self.randoms,
                using_ems: self.using_ems,
                transcript: self.transcript,
                suite,
                may_send_cert_status,
                must_issue_new_ticket,
            }))
        }
    }
}

struct ExpectCertificate {
    config: Arc<ClientConfig>,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    randoms: ConnectionRandoms,
    using_ems: bool,
    transcript: HandshakeHash,
    pub(super) suite: &'static Tls12CipherSuite,
    may_send_cert_status: bool,
    must_issue_new_ticket: bool,
}

impl State<ClientConnectionData> for ExpectCertificate {
    fn handle<'m>(
        mut self: Box<Self>,
        _cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        self.transcript.add_message(&m);
        let server_cert_chain = require_handshake_msg_move!(
            m,
            HandshakeType::Certificate,
            HandshakePayload::Certificate
        )?;

        if self.may_send_cert_status {
            Ok(Box::new(ExpectCertificateStatusOrServerKx {
                config: self.config,
                resuming_session: self.resuming_session,
                session_id: self.session_id,
                server_name: self.server_name,
                randoms: self.randoms,
                using_ems: self.using_ems,
                transcript: self.transcript,
                suite: self.suite,
                server_cert_chain,
                must_issue_new_ticket: self.must_issue_new_ticket,
            }))
        } else {
            let server_cert = ServerCertDetails::new(server_cert_chain, vec![]);

            Ok(Box::new(ExpectServerKx {
                config: self.config,
                resuming_session: self.resuming_session,
                session_id: self.session_id,
                server_name: self.server_name,
                randoms: self.randoms,
                using_ems: self.using_ems,
                transcript: self.transcript,
                suite: self.suite,
                server_cert,
                must_issue_new_ticket: self.must_issue_new_ticket,
            }))
        }
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        self
    }
}

struct ExpectCertificateStatusOrServerKx<'m> {
    config: Arc<ClientConfig>,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    randoms: ConnectionRandoms,
    using_ems: bool,
    transcript: HandshakeHash,
    suite: &'static Tls12CipherSuite,
    server_cert_chain: CertificateChain<'m>,
    must_issue_new_ticket: bool,
}

impl State<ClientConnectionData> for ExpectCertificateStatusOrServerKx<'_> {
    fn handle<'m>(
        self: Box<Self>,
        cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        match m.payload {
            MessagePayload::Handshake {
                parsed:
                    HandshakeMessagePayload {
                        payload: HandshakePayload::ServerKeyExchange(..),
                        ..
                    },
                ..
            } => Box::new(ExpectServerKx {
                config: self.config,
                resuming_session: self.resuming_session,
                session_id: self.session_id,
                server_name: self.server_name,
                randoms: self.randoms,
                using_ems: self.using_ems,
                transcript: self.transcript,
                suite: self.suite,
                server_cert: ServerCertDetails::new(self.server_cert_chain, vec![]),
                must_issue_new_ticket: self.must_issue_new_ticket,
            })
            .handle(cx, m),
            MessagePayload::Handshake {
                parsed:
                    HandshakeMessagePayload {
                        payload: HandshakePayload::CertificateStatus(..),
                        ..
                    },
                ..
            } => Box::new(ExpectCertificateStatus {
                config: self.config,
                resuming_session: self.resuming_session,
                session_id: self.session_id,
                server_name: self.server_name,
                randoms: self.randoms,
                using_ems: self.using_ems,
                transcript: self.transcript,
                suite: self.suite,
                server_cert_chain: self.server_cert_chain,
                must_issue_new_ticket: self.must_issue_new_ticket,
            })
            .handle(cx, m),
            payload => Err(inappropriate_handshake_message(
                &payload,
                &[ContentType::Handshake],
                &[
                    HandshakeType::ServerKeyExchange,
                    HandshakeType::CertificateStatus,
                ],
            )),
        }
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        Box::new(ExpectCertificateStatusOrServerKx {
            config: self.config,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            randoms: self.randoms,
            using_ems: self.using_ems,
            transcript: self.transcript,
            suite: self.suite,
            server_cert_chain: self.server_cert_chain.into_owned(),
            must_issue_new_ticket: self.must_issue_new_ticket,
        })
    }
}

struct ExpectCertificateStatus<'a> {
    config: Arc<ClientConfig>,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    randoms: ConnectionRandoms,
    using_ems: bool,
    transcript: HandshakeHash,
    suite: &'static Tls12CipherSuite,
    server_cert_chain: CertificateChain<'a>,
    must_issue_new_ticket: bool,
}

impl State<ClientConnectionData> for ExpectCertificateStatus<'_> {
    fn handle<'m>(
        mut self: Box<Self>,
        _cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        self.transcript.add_message(&m);
        let server_cert_ocsp_response = require_handshake_msg_move!(
            m,
            HandshakeType::CertificateStatus,
            HandshakePayload::CertificateStatus
        )?
        .into_inner();

        trace!(
            "Server stapled OCSP response is {:?}",
            &server_cert_ocsp_response
        );

        let server_cert = ServerCertDetails::new(self.server_cert_chain, server_cert_ocsp_response);

        Ok(Box::new(ExpectServerKx {
            config: self.config,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            randoms: self.randoms,
            using_ems: self.using_ems,
            transcript: self.transcript,
            suite: self.suite,
            server_cert,
            must_issue_new_ticket: self.must_issue_new_ticket,
        }))
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        Box::new(ExpectCertificateStatus {
            config: self.config,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            randoms: self.randoms,
            using_ems: self.using_ems,
            transcript: self.transcript,
            suite: self.suite,
            server_cert_chain: self.server_cert_chain.into_owned(),
            must_issue_new_ticket: self.must_issue_new_ticket,
        })
    }
}

struct ExpectServerKx<'a> {
    config: Arc<ClientConfig>,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    randoms: ConnectionRandoms,
    using_ems: bool,
    transcript: HandshakeHash,
    suite: &'static Tls12CipherSuite,
    server_cert: ServerCertDetails<'a>,
    must_issue_new_ticket: bool,
}

impl State<ClientConnectionData> for ExpectServerKx<'_> {
    fn handle<'m>(
        mut self: Box<Self>,
        cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        let opaque_kx = require_handshake_msg!(
            m,
            HandshakeType::ServerKeyExchange,
            HandshakePayload::ServerKeyExchange
        )?;
        self.transcript.add_message(&m);

        let kx = opaque_kx
            .unwrap_given_kxa(self.suite.kx)
            .ok_or_else(|| {
                cx.common.send_fatal_alert(
                    AlertDescription::DecodeError,
                    InvalidMessage::MissingKeyExchange,
                )
            })?;

        // Save the signature and signed parameters for later verification.
        let mut kx_params = Vec::new();
        kx.params.encode(&mut kx_params);
        let server_kx = ServerKxDetails::new(kx_params, kx.dss);

        #[cfg_attr(not(feature = "logging"), allow(unused_variables))]
        {
            match &kx.params {
                ServerKeyExchangeParams::Ecdh(ecdhe) => {
                    debug!("ECDHE curve is {:?}", ecdhe.curve_params)
                }
                ServerKeyExchangeParams::Dh(dhe) => {
                    debug!("DHE params are p = {:?}, g = {:?}", dhe.dh_p, dhe.dh_g)
                }
            }
        }

        Ok(Box::new(ExpectServerDoneOrCertReq {
            config: self.config,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            randoms: self.randoms,
            using_ems: self.using_ems,
            transcript: self.transcript,
            suite: self.suite,
            server_cert: self.server_cert,
            server_kx,
            must_issue_new_ticket: self.must_issue_new_ticket,
        }))
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        Box::new(ExpectServerKx {
            config: self.config,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            randoms: self.randoms,
            using_ems: self.using_ems,
            transcript: self.transcript,
            suite: self.suite,
            server_cert: self.server_cert.into_owned(),
            must_issue_new_ticket: self.must_issue_new_ticket,
        })
    }
}

fn emit_certificate(
    transcript: &mut HandshakeHash,
    cert_chain: CertificateChain<'static>,
    common: &mut CommonState,
) {
    let cert = Message {
        version: ProtocolVersion::TLSv1_2,
        payload: MessagePayload::handshake(HandshakeMessagePayload {
            typ: HandshakeType::Certificate,
            payload: HandshakePayload::Certificate(cert_chain),
        }),
    };

    transcript.add_message(&cert);
    common.send_msg(cert, false);
}

fn emit_client_kx(
    transcript: &mut HandshakeHash,
    kxa: KeyExchangeAlgorithm,
    common: &mut CommonState,
    pub_key: &[u8],
) {
    let mut buf = Vec::new();
    match kxa {
        KeyExchangeAlgorithm::ECDHE => ClientKeyExchangeParams::Ecdh(ClientEcdhParams {
            public: PayloadU8::new(pub_key.to_vec()),
        }),
        KeyExchangeAlgorithm::DHE => ClientKeyExchangeParams::Dh(ClientDhParams {
            public: PayloadU16::new(pub_key.to_vec()),
        }),
    }
    .encode(&mut buf);
    let pubkey = Payload::new(buf);

    let ckx = Message {
        version: ProtocolVersion::TLSv1_2,
        payload: MessagePayload::handshake(HandshakeMessagePayload {
            typ: HandshakeType::ClientKeyExchange,
            payload: HandshakePayload::ClientKeyExchange(pubkey),
        }),
    };

    transcript.add_message(&ckx);
    common.send_msg(ckx, false);
}

fn emit_certverify(
    transcript: &mut HandshakeHash,
    signer: &dyn Signer,
    common: &mut CommonState,
) -> Result<(), Error> {
    let message = transcript
        .take_handshake_buf()
        .ok_or_else(|| Error::General("Expected transcript".to_owned()))?;

    let scheme = signer.scheme();
    let sig = signer.sign(&message)?;
    let body = DigitallySignedStruct::new(scheme, sig);

    let m = Message {
        version: ProtocolVersion::TLSv1_2,
        payload: MessagePayload::handshake(HandshakeMessagePayload {
            typ: HandshakeType::CertificateVerify,
            payload: HandshakePayload::CertificateVerify(body),
        }),
    };

    transcript.add_message(&m);
    common.send_msg(m, false);
    Ok(())
}

fn emit_ccs(common: &mut CommonState) {
    let ccs = Message {
        version: ProtocolVersion::TLSv1_2,
        payload: MessagePayload::ChangeCipherSpec(ChangeCipherSpecPayload {}),
    };

    common.send_msg(ccs, false);
}

fn emit_finished(
    secrets: &ConnectionSecrets,
    transcript: &mut HandshakeHash,
    common: &mut CommonState,
) {
    let vh = transcript.current_hash();
    let verify_data = secrets.client_verify_data(&vh);
    let verify_data_payload = Payload::new(verify_data);

    let f = Message {
        version: ProtocolVersion::TLSv1_2,
        payload: MessagePayload::handshake(HandshakeMessagePayload {
            typ: HandshakeType::Finished,
            payload: HandshakePayload::Finished(verify_data_payload),
        }),
    };

    transcript.add_message(&f);
    common.send_msg(f, true);
}

struct ServerKxDetails {
    kx_params: Vec<u8>,
    kx_sig: DigitallySignedStruct,
}

impl ServerKxDetails {
    fn new(params: Vec<u8>, sig: DigitallySignedStruct) -> Self {
        Self {
            kx_params: params,
            kx_sig: sig,
        }
    }
}

// --- Either a CertificateRequest, or a ServerHelloDone. ---
// Existence of the CertificateRequest tells us the server is asking for
// client auth.  Otherwise we go straight to ServerHelloDone.
struct ExpectServerDoneOrCertReq<'a> {
    config: Arc<ClientConfig>,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    randoms: ConnectionRandoms,
    using_ems: bool,
    transcript: HandshakeHash,
    suite: &'static Tls12CipherSuite,
    server_cert: ServerCertDetails<'a>,
    server_kx: ServerKxDetails,
    must_issue_new_ticket: bool,
}

impl State<ClientConnectionData> for ExpectServerDoneOrCertReq<'_> {
    fn handle<'m>(
        mut self: Box<Self>,
        cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        if matches!(
            m.payload,
            MessagePayload::Handshake {
                parsed: HandshakeMessagePayload {
                    payload: HandshakePayload::CertificateRequest(_),
                    ..
                },
                ..
            }
        ) {
            Box::new(ExpectCertificateRequest {
                config: self.config,
                resuming_session: self.resuming_session,
                session_id: self.session_id,
                server_name: self.server_name,
                randoms: self.randoms,
                using_ems: self.using_ems,
                transcript: self.transcript,
                suite: self.suite,
                server_cert: self.server_cert,
                server_kx: self.server_kx,
                must_issue_new_ticket: self.must_issue_new_ticket,
            })
            .handle(cx, m)
        } else {
            self.transcript.abandon_client_auth();

            Box::new(ExpectServerDone {
                config: self.config,
                resuming_session: self.resuming_session,
                session_id: self.session_id,
                server_name: self.server_name,
                randoms: self.randoms,
                using_ems: self.using_ems,
                transcript: self.transcript,
                suite: self.suite,
                server_cert: self.server_cert,
                server_kx: self.server_kx,
                client_auth: None,
                must_issue_new_ticket: self.must_issue_new_ticket,
            })
            .handle(cx, m)
        }
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        Box::new(ExpectServerDoneOrCertReq {
            config: self.config,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            randoms: self.randoms,
            using_ems: self.using_ems,
            transcript: self.transcript,
            suite: self.suite,
            server_cert: self.server_cert.into_owned(),
            server_kx: self.server_kx,
            must_issue_new_ticket: self.must_issue_new_ticket,
        })
    }
}

struct ExpectCertificateRequest<'a> {
    config: Arc<ClientConfig>,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    randoms: ConnectionRandoms,
    using_ems: bool,
    transcript: HandshakeHash,
    suite: &'static Tls12CipherSuite,
    server_cert: ServerCertDetails<'a>,
    server_kx: ServerKxDetails,
    must_issue_new_ticket: bool,
}

impl State<ClientConnectionData> for ExpectCertificateRequest<'_> {
    fn handle<'m>(
        mut self: Box<Self>,
        _cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        let certreq = require_handshake_msg!(
            m,
            HandshakeType::CertificateRequest,
            HandshakePayload::CertificateRequest
        )?;
        self.transcript.add_message(&m);
        debug!("Got CertificateRequest {:?}", certreq);

        // The RFC jovially describes the design here as 'somewhat complicated'
        // and 'somewhat underspecified'.  So thanks for that.
        //
        // We ignore certreq.certtypes as a result, since the information it contains
        // is entirely duplicated in certreq.sigschemes.

        const NO_CONTEXT: Option<Vec<u8>> = None; // TLS 1.2 doesn't use a context.
        let no_compression = None; // or compression
        let client_auth = ClientAuthDetails::resolve(
            self.config
                .client_auth_cert_resolver
                .as_ref(),
            Some(&certreq.canames),
            &certreq.sigschemes,
            NO_CONTEXT,
            no_compression,
        );

        Ok(Box::new(ExpectServerDone {
            config: self.config,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            randoms: self.randoms,
            using_ems: self.using_ems,
            transcript: self.transcript,
            suite: self.suite,
            server_cert: self.server_cert,
            server_kx: self.server_kx,
            client_auth: Some(client_auth),
            must_issue_new_ticket: self.must_issue_new_ticket,
        }))
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        Box::new(ExpectCertificateRequest {
            config: self.config,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            randoms: self.randoms,
            using_ems: self.using_ems,
            transcript: self.transcript,
            suite: self.suite,
            server_cert: self.server_cert.into_owned(),
            server_kx: self.server_kx,
            must_issue_new_ticket: self.must_issue_new_ticket,
        })
    }
}

struct ExpectServerDone<'a> {
    config: Arc<ClientConfig>,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    randoms: ConnectionRandoms,
    using_ems: bool,
    transcript: HandshakeHash,
    suite: &'static Tls12CipherSuite,
    server_cert: ServerCertDetails<'a>,
    server_kx: ServerKxDetails,
    client_auth: Option<ClientAuthDetails>,
    must_issue_new_ticket: bool,
}

impl State<ClientConnectionData> for ExpectServerDone<'_> {
    fn handle<'m>(
        self: Box<Self>,
        cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        match m.payload {
            MessagePayload::Handshake {
                parsed:
                    HandshakeMessagePayload {
                        payload: HandshakePayload::ServerHelloDone,
                        ..
                    },
                ..
            } => {}
            payload => {
                return Err(inappropriate_handshake_message(
                    &payload,
                    &[ContentType::Handshake],
                    &[HandshakeType::ServerHelloDone],
                ));
            }
        }

        let mut st = *self;
        st.transcript.add_message(&m);

        cx.common.check_aligned_handshake()?;

        trace!("Server cert is {:?}", st.server_cert.cert_chain);
        debug!("Server DNS name is {:?}", st.server_name);

        let suite = st.suite;

        // 1. Verify the cert chain.
        // 2. Verify any SCTs provided with the certificate.
        // 3. Verify that the top certificate signed their kx.
        // 4. If doing client auth, send our Certificate.
        // 5. Complete the key exchange:
        //    a) generate our kx pair
        //    b) emit a ClientKeyExchange containing it
        //    c) if doing client auth, emit a CertificateVerify
        //    d) emit a CCS
        //    e) derive the shared keys, and start encryption
        // 6. emit a Finished, our first encrypted message under the new keys.

        // 1.
        let (end_entity, intermediates) = st
            .server_cert
            .cert_chain
            .split_first()
            .ok_or(Error::NoCertificatesPresented)?;

        let now = st.config.current_time()?;

        let cert_verified = st
            .config
            .verifier
            .verify_server_cert(
                end_entity,
                intermediates,
                &st.server_name,
                &st.server_cert.ocsp_response,
                now,
            )
            .map_err(|err| {
                cx.common
                    .send_cert_verify_error_alert(err)
            })?;

        // 3.
        // Build up the contents of the signed message.
        // It's ClientHello.random || ServerHello.random || ServerKeyExchange.params
        let sig_verified = {
            let mut message = Vec::new();
            message.extend_from_slice(&st.randoms.client);
            message.extend_from_slice(&st.randoms.server);
            message.extend_from_slice(&st.server_kx.kx_params);

            // Check the signature is compatible with the ciphersuite.
            let sig = &st.server_kx.kx_sig;
            if !SupportedCipherSuite::from(suite)
                .usable_for_signature_algorithm(sig.scheme.algorithm())
            {
                warn!(
                    "peer signed kx with wrong algorithm (got {:?} expect {:?})",
                    sig.scheme.algorithm(),
                    suite.sign
                );
                return Err(PeerMisbehaved::SignedKxWithWrongAlgorithm.into());
            }

            st.config
                .verifier
                .verify_tls12_signature(&message, end_entity, sig)
                .map_err(|err| {
                    cx.common
                        .send_cert_verify_error_alert(err)
                })?
        };
        cx.common.peer_certificates = Some(st.server_cert.cert_chain.into_owned());

        // 4.
        if let Some(client_auth) = &st.client_auth {
            let certs = match client_auth {
                ClientAuthDetails::Empty { .. } => CertificateChain::default(),
                ClientAuthDetails::Verify { certkey, .. } => CertificateChain(certkey.cert.clone()),
            };
            emit_certificate(&mut st.transcript, certs, cx.common);
        }

        // 5a.
        let kx_params = tls12::decode_kx_params::<ServerKeyExchangeParams>(
            st.suite.kx,
            cx.common,
            &st.server_kx.kx_params,
        )?;
        let named_group = kx_params
            .named_group()
            .ok_or(PeerMisbehaved::SelectedUnofferedKxGroup)?;
        let skxg = match st.config.find_kx_group(named_group) {
            Some(skxg) => skxg,
            None => {
                return Err(PeerMisbehaved::SelectedUnofferedKxGroup.into());
            }
        };
        cx.common.kx_state = KxState::Start(skxg);
        let kx = skxg.start()?;

        // 5b.
        let mut transcript = st.transcript;
        emit_client_kx(&mut transcript, st.suite.kx, cx.common, kx.pub_key());
        // Note: EMS handshake hash only runs up to ClientKeyExchange.
        let ems_seed = st
            .using_ems
            .then(|| transcript.current_hash());

        // 5c.
        if let Some(ClientAuthDetails::Verify { signer, .. }) = &st.client_auth {
            emit_certverify(&mut transcript, signer.as_ref(), cx.common)?;
        }

        // 5d.
        emit_ccs(cx.common);

        // 5e. Now commit secrets.
        let secrets = ConnectionSecrets::from_key_exchange(
            kx,
            kx_params.pub_key(),
            ems_seed,
            st.randoms,
            suite,
        )?;
        cx.common.kx_state.complete();

        st.config.key_log.log(
            "CLIENT_RANDOM",
            &secrets.randoms.client,
            &secrets.master_secret,
        );
        cx.common
            .start_encryption_tls12(&secrets, Side::Client);
        cx.common
            .record_layer
            .start_encrypting();

        // 6.
        emit_finished(&secrets, &mut transcript, cx.common);

        if st.must_issue_new_ticket {
            Ok(Box::new(ExpectNewTicket {
                config: st.config,
                secrets,
                resuming_session: st.resuming_session,
                session_id: st.session_id,
                server_name: st.server_name,
                using_ems: st.using_ems,
                transcript,
                resuming: false,
                cert_verified,
                sig_verified,
            }))
        } else {
            Ok(Box::new(ExpectCcs {
                config: st.config,
                secrets,
                resuming_session: st.resuming_session,
                session_id: st.session_id,
                server_name: st.server_name,
                using_ems: st.using_ems,
                transcript,
                ticket: None,
                resuming: false,
                cert_verified,
                sig_verified,
            }))
        }
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        Box::new(ExpectServerDone {
            config: self.config,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            randoms: self.randoms,
            using_ems: self.using_ems,
            transcript: self.transcript,
            suite: self.suite,
            server_cert: self.server_cert.into_owned(),
            server_kx: self.server_kx,
            client_auth: self.client_auth,
            must_issue_new_ticket: self.must_issue_new_ticket,
        })
    }
}

struct ExpectNewTicket {
    config: Arc<ClientConfig>,
    secrets: ConnectionSecrets,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    using_ems: bool,
    transcript: HandshakeHash,
    resuming: bool,
    cert_verified: verify::ServerCertVerified,
    sig_verified: verify::HandshakeSignatureValid,
}

impl State<ClientConnectionData> for ExpectNewTicket {
    fn handle<'m>(
        mut self: Box<Self>,
        _cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        self.transcript.add_message(&m);

        let nst = require_handshake_msg_move!(
            m,
            HandshakeType::NewSessionTicket,
            HandshakePayload::NewSessionTicket
        )?;

        Ok(Box::new(ExpectCcs {
            config: self.config,
            secrets: self.secrets,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            using_ems: self.using_ems,
            transcript: self.transcript,
            ticket: Some(nst),
            resuming: self.resuming,
            cert_verified: self.cert_verified,
            sig_verified: self.sig_verified,
        }))
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        self
    }
}

// -- Waiting for their CCS --
struct ExpectCcs {
    config: Arc<ClientConfig>,
    secrets: ConnectionSecrets,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    using_ems: bool,
    transcript: HandshakeHash,
    ticket: Option<NewSessionTicketPayload>,
    resuming: bool,
    cert_verified: verify::ServerCertVerified,
    sig_verified: verify::HandshakeSignatureValid,
}

impl State<ClientConnectionData> for ExpectCcs {
    fn handle<'m>(
        self: Box<Self>,
        cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        match m.payload {
            MessagePayload::ChangeCipherSpec(..) => {}
            payload => {
                return Err(inappropriate_message(
                    &payload,
                    &[ContentType::ChangeCipherSpec],
                ));
            }
        }
        // CCS should not be received interleaved with fragmented handshake-level
        // message.
        cx.common.check_aligned_handshake()?;

        // Note: msgs layer validates trivial contents of CCS.
        cx.common
            .record_layer
            .start_decrypting();

        Ok(Box::new(ExpectFinished {
            config: self.config,
            secrets: self.secrets,
            resuming_session: self.resuming_session,
            session_id: self.session_id,
            server_name: self.server_name,
            using_ems: self.using_ems,
            transcript: self.transcript,
            ticket: self.ticket,
            resuming: self.resuming,
            cert_verified: self.cert_verified,
            sig_verified: self.sig_verified,
        }))
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        self
    }
}

struct ExpectFinished {
    config: Arc<ClientConfig>,
    resuming_session: Option<persist::Tls12ClientSessionValue>,
    session_id: SessionId,
    server_name: ServerName<'static>,
    using_ems: bool,
    transcript: HandshakeHash,
    ticket: Option<NewSessionTicketPayload>,
    secrets: ConnectionSecrets,
    resuming: bool,
    cert_verified: verify::ServerCertVerified,
    sig_verified: verify::HandshakeSignatureValid,
}

impl ExpectFinished {
    // -- Waiting for their finished --
    fn save_session(&mut self, cx: &ClientContext<'_>) {
        // Save a ticket.  If we got a new ticket, save that.  Otherwise, save the
        // original ticket again.
        let (mut ticket, lifetime) = match self.ticket.take() {
            Some(nst) => (nst.ticket.0, nst.lifetime_hint),
            None => (Vec::new(), 0),
        };

        if ticket.is_empty() {
            if let Some(resuming_session) = &mut self.resuming_session {
                ticket = resuming_session.take_ticket();
            }
        }

        if self.session_id.is_empty() && ticket.is_empty() {
            debug!("Session not saved: server didn't allocate id or ticket");
            return;
        }

        let now = match self.config.current_time() {
            Ok(now) => now,
            Err(_) => {
                debug!("Could not get current time");
                return;
            }
        };

        let session_value = persist::Tls12ClientSessionValue::new(
            self.secrets.suite(),
            self.session_id,
            ticket,
            self.secrets.master_secret(),
            cx.common
                .peer_certificates
                .clone()
                .unwrap_or_default(),
            now,
            lifetime,
            self.using_ems,
        );

        self.config
            .resumption
            .store
            .set_tls12_session(self.server_name.clone(), session_value);
    }
}

impl State<ClientConnectionData> for ExpectFinished {
    fn handle<'m>(
        self: Box<Self>,
        cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        let mut st = *self;
        let finished =
            require_handshake_msg!(m, HandshakeType::Finished, HandshakePayload::Finished)?;

        cx.common.check_aligned_handshake()?;

        // Work out what verify_data we expect.
        let vh = st.transcript.current_hash();
        let expect_verify_data = st.secrets.server_verify_data(&vh);

        // Constant-time verification of this is relatively unimportant: they only
        // get one chance.  But it can't hurt.
        let _fin_verified =
            match ConstantTimeEq::ct_eq(&expect_verify_data[..], finished.bytes()).into() {
                true => verify::FinishedMessageVerified::assertion(),
                false => {
                    return Err(cx
                        .common
                        .send_fatal_alert(AlertDescription::DecryptError, Error::DecryptError));
                }
            };

        // Hash this message too.
        st.transcript.add_message(&m);

        st.save_session(cx);

        if st.resuming {
            emit_ccs(cx.common);
            cx.common
                .record_layer
                .start_encrypting();
            emit_finished(&st.secrets, &mut st.transcript, cx.common);
        }

        cx.common
            .start_traffic(&mut cx.sendable_plaintext);
        Ok(Box::new(ExpectTraffic {
            secrets: st.secrets,
            _cert_verified: st.cert_verified,
            _sig_verified: st.sig_verified,
            _fin_verified,
        }))
    }

    // we could not decrypt the encrypted handshake message with session resumption
    // this might mean that the ticket was invalid for some reason, so we remove it
    // from the store to restart a session from scratch
    fn handle_decrypt_error(&self) {
        if self.resuming {
            self.config
                .resumption
                .store
                .remove_tls12_session(&self.server_name);
        }
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        self
    }
}

// -- Traffic transit state --
struct ExpectTraffic {
    secrets: ConnectionSecrets,
    _cert_verified: verify::ServerCertVerified,
    _sig_verified: verify::HandshakeSignatureValid,
    _fin_verified: verify::FinishedMessageVerified,
}

impl State<ClientConnectionData> for ExpectTraffic {
    fn handle<'m>(
        self: Box<Self>,
        cx: &mut ClientContext<'_>,
        m: Message<'m>,
    ) -> hs::NextStateOrError<'m>
    where
        Self: 'm,
    {
        match m.payload {
            MessagePayload::ApplicationData(payload) => cx
                .common
                .take_received_plaintext(payload),
            payload => {
                return Err(inappropriate_message(
                    &payload,
                    &[ContentType::ApplicationData],
                ));
            }
        }
        Ok(self)
    }

    fn export_keying_material(
        &self,
        output: &mut [u8],
        label: &[u8],
        context: Option<&[u8]>,
    ) -> Result<(), Error> {
        self.secrets
            .export_keying_material(output, label, context);
        Ok(())
    }

    fn extract_secrets(&self) -> Result<PartiallyExtractedSecrets, Error> {
        self.secrets
            .extract_secrets(Side::Client)
    }

    fn into_owned(self: Box<Self>) -> hs::NextState<'static> {
        self
    }
}
