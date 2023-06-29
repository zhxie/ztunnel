// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt::Debug;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hyper::client::ResponseFuture;
use hyper::server::conn::AddrStream;
use hyper::{Request, Uri};
use openssl::asn1::Asn1Time;
use openssl::bn::BigNum;
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{self, Ssl, SslContextBuilder, SslMode};
use openssl::stack::Stack;
use openssl::x509::extension::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
};
use openssl::x509::verify::X509CheckFlags;
use openssl::x509::{self, X509Ref, X509StoreContextRef, X509VerifyResult};
use rand::RngCore;
use tokio::net::TcpStream;
use tonic::body::BoxBody;
use tower::Service;
use tracing::{error, info};

use crate::config::RootCert;
use crate::identity::{self, Identity};

use super::Error;

pub use openssl::asn1::Asn1TimeRef;
pub use openssl::error::ErrorStack;
pub use openssl::ssl::{ConnectConfiguration, SslAcceptor};
pub use openssl::x509::X509;
pub use tokio_openssl::SslStream;

pub fn version() -> &'static str {
    openssl::version::version()
}

pub fn asn1_time_to_system_time(time: &Asn1TimeRef) -> SystemTime {
    let unix_time = Asn1Time::from_unix(0).unwrap().diff(time).unwrap();
    SystemTime::UNIX_EPOCH
        + Duration::from_secs(unix_time.days as u64 * 86400 + unix_time.secs as u64)
}

fn system_time_to_asn1_time(time: SystemTime) -> Option<Asn1Time> {
    let ts = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Asn1Time::from_unix(ts.try_into().ok()?).ok()
}

pub fn cert_from(key: &[u8], cert: &[u8], chain: Vec<&[u8]>) -> Certs {
    let key = pkey::PKey::private_key_from_pem(key).unwrap();
    let cert = X509::from_pem(cert).unwrap();
    let ztunnel_cert = ZtunnelCert::new(cert);
    let chain = chain
        .into_iter()
        .map(|pem| ZtunnelCert::new(X509::from_pem(pem).unwrap()))
        .collect();
    Certs {
        cert: ztunnel_cert,
        chain,
        key,
    }
}

pub struct CertSign {
    pub csr: Vec<u8>,
    pub pkey: Vec<u8>,
}

pub struct CsrOptions {
    pub san: String,
}

impl CsrOptions {
    pub fn generate(&self) -> Result<CertSign, Error> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
        let ec_key = EcKey::generate(&group)?;
        let pkey = PKey::from_ec_key(ec_key)?;

        let mut csr = x509::X509ReqBuilder::new()?;
        csr.set_pubkey(&pkey)?;
        let mut extensions = Stack::new()?;
        let subject_alternative_name = SubjectAlternativeName::new()
            .uri(&self.san)
            .critical()
            .build(&csr.x509v3_context(None))
            .unwrap();
        extensions.push(subject_alternative_name)?;
        csr.add_extensions(&extensions)?;
        csr.sign(&pkey, MessageDigest::sha256())?;

        let csr = csr.build();
        let pkey_pem = pkey.private_key_to_pem_pkcs8()?;
        let csr_pem = csr.to_pem()?;
        Ok(CertSign {
            csr: csr_pem,
            pkey: pkey_pem,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ZtunnelCert {
    x509: X509,
    not_before: SystemTime,
    not_after: SystemTime,
}

// Wrapper around X509 that uses SystemTime for not_before/not_after.
// Asn1Time does not support sub-second granularity.
impl ZtunnelCert {
    pub fn new(cert: X509) -> ZtunnelCert {
        ZtunnelCert {
            not_before: asn1_time_to_system_time(cert.not_before()),
            not_after: asn1_time_to_system_time(cert.not_after()),
            x509: cert, // cert is already owned, the asn1_ functions borrow cert so as long as we move cert to ZtunnelCert after the borrows this doesn't need cloning
        }
    }
}

#[derive(Clone, Debug)]
pub struct Certs {
    // the leaf cert
    cert: ZtunnelCert,
    // the remainder of the chain, not including the leaf cert
    chain: Vec<ZtunnelCert>,
    key: pkey::PKey<pkey::Private>,
}

impl PartialEq for Certs {
    fn eq(&self, other: &Self) -> bool {
        self.cert
            .x509
            .to_der()
            .iter()
            .eq(other.cert.x509.to_der().iter())
            && self
                .key
                .private_key_to_der()
                .iter()
                .eq(other.key.private_key_to_der().iter())
            && self.cert.not_after == other.cert.not_after
            && self.cert.not_before == other.cert.not_before
    }
}

impl Certs {
    pub fn chain(&self) -> Result<bytes::Bytes, Error> {
        Ok(self.chain[0].x509.to_pem()?.into())
    }

    // TODO: This works very differently from the chain method. Figure out what's the intention
    // behind the chain method and make things more consistent.
    pub fn iter_chain(&self) -> impl Iterator<Item = &X509> {
        self.chain.iter().map(|zcert| &zcert.x509)
    }

    pub fn is_expired(&self) -> bool {
        SystemTime::now() > self.cert.not_after
    }

    pub fn refresh_at(&self) -> SystemTime {
        match self.cert.not_after.duration_since(self.cert.not_before) {
            Ok(valid_for) => self.cert.not_before + valid_for / 2,
            Err(_) => self.cert.not_after,
        }
    }

    pub fn get_duration_until_refresh(&self) -> Duration {
        let halflife = self
            .cert
            .not_after
            .duration_since(self.cert.not_before)
            .unwrap_or_else(|_| std::time::Duration::from_secs(0))
            / 2;
        // If now() is earlier than not_before, we need to refresh ASAP, so return 0.
        let elapsed = SystemTime::now()
            .duration_since(self.cert.not_before)
            .unwrap_or(halflife);
        halflife
            .checked_sub(elapsed)
            .unwrap_or_else(|| Duration::from_secs(0))
    }

    pub fn x509(&self) -> &X509 {
        &self.cert.x509
    }
}

#[derive(Clone, Debug)]
pub struct TlsGrpcChannel {
    uri: Uri,
    client: hyper::Client<hyper_openssl::HttpsConnector<hyper::client::HttpConnector>, BoxBody>,
}

/// grpc_connector provides a client TLS channel for gRPC requests.
pub fn grpc_connector(uri: String, root_cert: RootCert) -> Result<TlsGrpcChannel, Error> {
    let mut conn = ssl::SslConnector::builder(ssl::SslMethod::tls_client())?;

    let uri = Uri::try_from(uri)?;
    let is_localhost_call = uri.host() == Some("localhost");
    conn.set_verify(ssl::SslVerifyMode::PEER);
    conn.set_alpn_protos(Alpn::H2.encode())?;
    conn.set_min_proto_version(Some(ssl::SslVersion::TLS1_2))?;
    conn.set_max_proto_version(Some(ssl::SslVersion::TLS1_3))?;
    match root_cert {
        RootCert::File(f) => {
            conn.set_ca_file(f).map_err(Error::InvalidRootCert)?;
        }
        RootCert::Static(b) => {
            conn.cert_store_mut()
                .add_cert(X509::from_pem(&b).map_err(Error::InvalidRootCert)?)
                .map_err(Error::InvalidRootCert)?;
        }
        RootCert::Default => {} // Already configured to use system root certs
    }
    let mut http = hyper::client::HttpConnector::new();
    http.enforce_http(false);
    let mut https = hyper_openssl::HttpsConnector::with_connector(http, conn)?;
    https.set_callback(move |cc, _| {
        if is_localhost_call {
            // Follow Istio logic to allow localhost calls: https://github.com/istio/istio/blob/373fc89518c986c9f48ed3cd891930da6fdc8628/pkg/istio-agent/xds_proxy.go#L735
            cc.set_verify_hostname(false);
            let param = cc.param_mut();
            param.set_hostflags(X509CheckFlags::NO_PARTIAL_WILDCARDS);
            param.set_host("istiod.istio-system.svc").unwrap();
        }
        Ok(())
    });

    // Configure hyper's client to be h2 only and build with the
    // correct https connector.
    let hyper = hyper::Client::builder()
        .http2_only(true)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .build(https);

    Ok(TlsGrpcChannel { uri, client: hyper })
}

impl Certs {
    fn verify_mode() -> ssl::SslVerifyMode {
        ssl::SslVerifyMode::PEER | ssl::SslVerifyMode::FAIL_IF_NO_PEER_CERT
    }

    pub fn mtls_acceptor(&self) -> Result<SslAcceptor, Error> {
        let _ctx = ssl::SslContext::builder(ssl::SslMethod::tls_server())?;
        // mozilla_intermediate_v5 is the only variant that enables TLSv1.3, so we use that.
        let mut conn = SslAcceptor::mozilla_intermediate_v5(ssl::SslMethod::tls_server())?;
        self.setup_ctx(&mut conn)?;

        Ok(conn.build())
    }

    pub fn acceptor(&self) -> Result<SslAcceptor, Error> {
        let _ctx = ssl::SslContext::builder(ssl::SslMethod::tls_server())?;
        // mozilla_intermediate_v5 is the only variant that enables TLSv1.3, so we use that.
        let mut conn = SslAcceptor::mozilla_intermediate_v5(ssl::SslMethod::tls_server())?;
        self.setup_ctx(&mut conn)?;

        conn.set_verify_callback(ssl::SslVerifyMode::NONE, Verifier::None.callback());
        Ok(conn.build())
    }

    pub fn connector(&self, dest_id: Option<&Identity>) -> Result<ssl::SslConnector, Error> {
        let mut conn = ssl::SslConnector::builder(ssl::SslMethod::tls_client())?;
        self.setup_ctx(&mut conn)?;

        // client verifies SAN
        if let Some(dest_id) = dest_id {
            conn.set_verify_callback(
                Self::verify_mode(),
                Verifier::San(dest_id.clone()).callback(),
            );
        }

        Ok(conn.build())
    }

    fn setup_ctx(&self, conn: &mut SslContextBuilder) -> Result<(), Error> {
        // Enable async mode if there are async-enabled engines.
        conn.set_mode(SslMode::ASYNC);

        // general TLS options
        conn.set_alpn_protos(Alpn::H2.encode())?;
        conn.set_min_proto_version(Some(ssl::SslVersion::TLS1_3))?;
        conn.set_max_proto_version(Some(ssl::SslVersion::TLS1_3))?;

        // key and certs
        conn.set_private_key(&self.key)?;
        conn.set_certificate(&self.cert.x509)?;
        for (i, chain_cert) in self.chain.iter().enumerate() {
            // Only include intermediate certs in the chain.
            // The last cert is the root cert which should already exist on the peer.
            if i < (self.chain.len() - 1) {
                // This is an intermediate cert that should be added to the cert chain
                conn.add_extra_chain_cert(chain_cert.x509.clone())?;
            }
            conn.cert_store_mut().add_cert(chain_cert.x509.clone())?;
        }
        conn.check_private_key()?;

        // by default, allow OpenSSL to do standard validation
        conn.set_verify_callback(Self::verify_mode(), Verifier::None.callback());

        Ok(())
    }
}

enum Verifier {
    // Does not verify an individual identity.
    None,

    // Allows exactly one identity, making sure at least one of the presented certs
    San(Identity),
}

impl Verifier {
    fn base_verifier(verified: bool, ctx: &mut X509StoreContextRef) -> Result<(), TlsError> {
        if !verified {
            return Err(TlsError::Verification(ctx.error()));
        };
        Ok(())
    }

    fn verifiy_san(&self, ctx: &mut X509StoreContextRef) -> Result<(), TlsError> {
        let Self::San(identity) = self else {
            // not verifying san
            return Ok(());
        };

        let cert = ctx
            .chain()
            .ok_or(TlsError::ExDataError)?
            .get(0)
            .ok_or(TlsError::PeerCertError)?;

        cert.verify_san(identity)
    }

    fn verify(&self, verified: bool, ctx: &mut X509StoreContextRef) -> Result<(), TlsError> {
        Self::base_verifier(verified, ctx)?;
        self.verifiy_san(ctx)?;
        Ok(())
    }

    fn callback(self) -> impl Fn(bool, &mut X509StoreContextRef) -> bool {
        move |verified, ctx| match self.verify(verified, ctx) {
            Ok(_) => true,
            Err(e) => {
                // TODO metrics/counters; info would be too noisy
                info!("failed verifying TLS: {e}");
                false
            }
        }
    }
}

pub trait SanChecker {
    fn verify_san(&self, identity: &Identity) -> Result<(), TlsError>;
}

impl SanChecker for Certs {
    fn verify_san(&self, identity: &Identity) -> Result<(), TlsError> {
        self.cert.x509.verify_san(identity)
    }
}

pub fn extract_sans(cert: &X509Ref) -> Vec<Identity> {
    cert.subject_alt_names()
        .iter()
        .flat_map(|sans| sans.iter())
        .filter_map(|s| s.uri())
        .map(Identity::from_str)
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default()
}

impl SanChecker for X509Ref {
    fn verify_san(&self, identity: &Identity) -> Result<(), TlsError> {
        let sans = extract_sans(self);
        sans.iter()
            .find(|id| id == &identity)
            .ok_or_else(|| TlsError::SanError(identity.to_owned(), sans.clone()))
            .map(|_| ())
    }
}

impl Service<Request<BoxBody>> for TlsGrpcChannel {
    type Response = hyper::Response<hyper::Body>;
    type Error = hyper::Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Ok(()).into()
    }

    fn call(&mut self, mut req: Request<BoxBody>) -> Self::Future {
        let uri = Uri::builder()
            .scheme(self.uri.scheme().unwrap().to_owned())
            .authority(self.uri.authority().unwrap().to_owned())
            .path_and_query(req.uri().path_and_query().unwrap().to_owned())
            .build()
            .unwrap();
        *req.uri_mut() = uri;
        self.client.request(req)
    }
}

enum Alpn {
    H2,
}

impl Alpn {
    fn encode(&self) -> &[u8] {
        match self {
            Alpn::H2 => b"\x02h2",
        }
    }
}

#[async_trait::async_trait]
pub trait CertProvider: Send + Sync {
    async fn fetch_cert(&mut self, fd: &TcpStream) -> Result<SslAcceptor, TlsError>;
}

#[derive(Clone, Debug)]
pub struct ControlPlaneCertProvider(pub Certs);

#[async_trait::async_trait]
impl CertProvider for ControlPlaneCertProvider {
    async fn fetch_cert(&mut self, _: &TcpStream) -> Result<SslAcceptor, TlsError> {
        let acc = self.0.acceptor()?;
        Ok(acc)
    }
}

#[derive(Clone)]
pub struct TlsAcceptor<F: CertProvider> {
    /// Acceptor is a function that determines the TLS context to use. As input, the FD of the client
    /// connection is provided.
    pub acceptor: F,
}

#[derive(thiserror::Error, Debug)]
pub enum HandshakeError<S> {
    #[error("invalid operation: {1:?}")]
    Ssl(S, ErrorStack),
    #[error("create stream error: {0:?}")]
    Create(ErrorStack),
    #[error("accept error: {1}")]
    Accept(SslStream<S>, openssl::ssl::Error),
    #[error("connect error: {1}")]
    Connect(SslStream<S>, openssl::ssl::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum TlsError {
    #[error("tls handshake error: {0:?}")]
    Handshake(#[from] HandshakeError<TcpStream>),
    #[error("tls verification error: {0}")]
    Verification(X509VerifyResult),
    #[error("certificate lookup error: {0} is not a known destination")]
    CertificateLookup(IpAddr),
    #[error("signing error: {0}")]
    SigningError(#[from] identity::Error),
    #[error("san verification error: remote did not present the expected SAN ({0}), got {1:?}")]
    SanError(Identity, Vec<Identity>),
    #[error("failed getting ex data")]
    ExDataError,
    #[error("failed getting peer cert")]
    PeerCertError,
    #[error("ssl error: {0}")]
    SslError(#[from] Error),
}

impl<F> tls_listener::AsyncTls<AddrStream> for TlsAcceptor<F>
where
    F: CertProvider + Clone + 'static,
{
    type Stream = SslStream<TcpStream>;
    type Error = TlsError;
    type AcceptFuture = Pin<Box<dyn Future<Output = Result<Self::Stream, Self::Error>> + Send>>;

    fn accept(&self, conn: AddrStream) -> Self::AcceptFuture {
        let inner = conn.into_inner();
        let mut acceptor = self.acceptor.clone();
        Box::pin(async move {
            let tls = acceptor.fetch_cert(&inner).await?;
            let ssl = match Ssl::new(tls.context()) {
                Ok(ssl) => ssl,
                Err(e) => return Err(TlsError::Handshake(HandshakeError::Ssl(inner, e))),
            };
            let mut stream = SslStream::new(ssl, inner)
                .map_err(|e| TlsError::Handshake(HandshakeError::Create(e)))?;
            match Pin::new(&mut stream).accept().await {
                Ok(()) => Ok(stream),
                Err(e) => Err(TlsError::Handshake(HandshakeError::Accept(stream, e))),
            }
        })
    }
}

pub async fn connect(
    config: ConnectConfiguration,
    domain: &str,
    stream: TcpStream,
) -> Result<SslStream<TcpStream>, HandshakeError<TcpStream>> {
    let ssl = match config.into_ssl(domain) {
        Ok(ssl) => ssl,
        Err(e) => return Err(HandshakeError::Ssl(stream, e)),
    };
    let mut stream = SslStream::new(ssl, stream).map_err(HandshakeError::Create)?;
    match Pin::new(&mut stream).connect().await {
        Ok(()) => Ok(stream),
        Err(e) => Err(HandshakeError::Connect(stream, e)),
    }
}

const TEST_CERT: &[u8] = include_bytes!("cert-chain.pem");
const TEST_PKEY: &[u8] = include_bytes!("key.pem");
const TEST_ROOT: &[u8] = include_bytes!("root-cert.pem");
const TEST_ROOT_KEY: &[u8] = include_bytes!("ca-key.pem");

/// TestIdentity is an identity used for testing. This extends the Identity with test-only types
#[derive(Debug)]
pub enum TestIdentity {
    Identity(Identity),
    Ip(IpAddr),
}

impl From<Identity> for TestIdentity {
    fn from(i: Identity) -> Self {
        Self::Identity(i)
    }
}

impl From<IpAddr> for TestIdentity {
    fn from(i: IpAddr) -> Self {
        Self::Ip(i)
    }
}

//
// impl Display for TestIdentity {
//     fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
//         match self {
//             TestIdentity::Identity(i) => std::fmt::Display::fmt(&i, f),
//             TestIdentity::Ip(i) => std::fmt::Display::fmt(&i, f),
//         }
//     }
// }

// TODO: Move to the mock submodule.

// TODO: Move towards code that doesn't rely on SystemTime::now() for easier time control with
// tokio. Ideally we'll be able to also get rid of the sub-second timestamps on certificates
// (since right now they are there only for testing).
fn generate_test_certs_at(
    id: &TestIdentity,
    not_before: SystemTime,
    not_after: SystemTime,
    rng: Option<&mut dyn rand::RngCore>,
) -> Certs {
    let key = pkey::PKey::private_key_from_pem(TEST_PKEY).unwrap();
    let (ca_cert, ca_key) = test_ca().unwrap();
    let mut builder = X509::builder().unwrap();
    let not_before_asn = system_time_to_asn1_time(not_before).unwrap();
    builder.set_not_before(&not_before_asn).unwrap();
    builder
        .set_not_after(&system_time_to_asn1_time(not_after).unwrap())
        .unwrap();

    builder.set_pubkey(&key).unwrap();
    builder.set_version(2).unwrap();
    let serial_number = {
        let mut data = [0u8; 20];
        match rng {
            None => rand::thread_rng().fill_bytes(&mut data),
            Some(rng) => rng.fill_bytes(&mut data),
        }
        // Clear the most significant bit to make the resulting bignum effectively 159 bit long.
        data[0] &= 0x7f;
        let serial = BigNum::from_slice(&data).unwrap();
        serial.to_asn1_integer().unwrap()
    };
    builder.set_serial_number(&serial_number).unwrap();

    let mut names = openssl::x509::X509NameBuilder::new().unwrap();
    names.append_entry_by_text("O", "cluster.local").unwrap();
    let names = names.build();
    builder.set_issuer_name(&names).unwrap();

    let basic_constraints = BasicConstraints::new().critical().build().unwrap();
    let key_usage = KeyUsage::new()
        .critical()
        .digital_signature()
        .key_encipherment()
        .build()
        .unwrap();
    let ext_key_usage = ExtendedKeyUsage::new()
        .client_auth()
        .server_auth()
        .build()
        .unwrap();
    let authority_key_identifier = AuthorityKeyIdentifier::new()
        .keyid(false)
        .issuer(false)
        .build(&builder.x509v3_context(Some(&ca_cert), None))
        .unwrap();
    let mut san = SubjectAlternativeName::new();
    let subject_alternative_name = match id {
        TestIdentity::Identity(id) => san.uri(&id.to_string()),
        TestIdentity::Ip(ip) => san.ip(&ip.to_string()),
    };
    let subject_alternative_name = subject_alternative_name
        .critical()
        .build(&builder.x509v3_context(Some(&ca_cert), None))
        .unwrap();
    builder.append_extension(key_usage).unwrap();
    builder.append_extension(ext_key_usage).unwrap();
    builder.append_extension(basic_constraints).unwrap();
    builder.append_extension(authority_key_identifier).unwrap();
    builder.append_extension(subject_alternative_name).unwrap();

    builder.sign(&ca_key, MessageDigest::sha256()).unwrap();

    let mut cert = ZtunnelCert::new(builder.build());
    // For sub-second granularity
    cert.not_before = not_before;
    cert.not_after = not_after;
    Certs {
        cert,
        key,
        chain: vec![ZtunnelCert::new(ca_cert)],
    }
}

pub fn generate_test_certs(
    id: &TestIdentity,
    duration_until_valid: Duration,
    duration_until_expiry: Duration,
) -> Certs {
    let not_before = SystemTime::now() + duration_until_valid;
    generate_test_certs_at(id, not_before, not_before + duration_until_expiry, None)
}

fn test_ca() -> Result<(X509, PKey<Private>), Error> {
    let cert = X509::from_pem(TEST_ROOT)?;
    let key = pkey::PKey::private_key_from_pem(TEST_ROOT_KEY)?;
    Ok((cert, key))
}

pub fn test_certs() -> Certs {
    let cert = ZtunnelCert::new(X509::from_pem(TEST_CERT).unwrap());
    let key = pkey::PKey::private_key_from_pem(TEST_PKEY).unwrap();
    let chain = vec![cert.clone()];
    Certs { cert, key, chain }
}

pub mod mock {
    use rand::{rngs::SmallRng, SeedableRng};
    use std::time::SystemTime;

    use super::{generate_test_certs_at, Certs, TestIdentity};

    /// Allows generating test certificates in a deterministic manner.
    pub struct CertGenerator {
        rng: SmallRng,
    }

    impl CertGenerator {
        /// Returns a new test certificate generator. The seed parameter sets the seed for any
        /// randomized operations. Multiple CertGenerator instances created with the same seed will
        /// return the same successive certificates, if same arguments to new_certs are given.
        pub fn new(seed: u64) -> Self {
            Self {
                rng: SmallRng::seed_from_u64(seed),
            }
        }

        pub fn new_certs(
            &mut self,
            id: &TestIdentity,
            not_before: SystemTime,
            not_after: SystemTime,
        ) -> Certs {
            generate_test_certs_at(id, not_before, not_after, Some(&mut self.rng))
        }
    }

    impl Default for CertGenerator {
        fn default() -> Self {
            // Use arbitrary seed.
            Self::new(427)
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::time::Duration;

    use crate::identity::Identity;
    use crate::tls::TestIdentity;

    use super::generate_test_certs;

    #[test]
    #[cfg(feature = "boring-fips")]
    fn is_fips_enabled() {
        assert!(boring::fips::enabled());
    }

    #[test]
    #[cfg(all(feature = "boring", not(feature = "boring-fips")))]
    fn is_fips_disabled() {
        assert!(!boring::fips::enabled());
    }

    #[test]
    fn cert_expiration() {
        let expiry_seconds = 1000;
        let id: TestIdentity = Identity::default().into();
        let zero_dur = Duration::from_secs(0);
        let certs_not_expired = generate_test_certs(
            &id,
            Duration::from_secs(0),
            Duration::from_secs(expiry_seconds),
        );
        assert!(!certs_not_expired.is_expired());
        let seconds_until_refresh = certs_not_expired.get_duration_until_refresh().as_secs();
        // Give a couple second window to avoid flakiness in the test.
        assert!(
            seconds_until_refresh <= expiry_seconds / 2
                && seconds_until_refresh >= expiry_seconds / 2 - 1
        );

        let certs_expired = generate_test_certs(&id, zero_dur, zero_dur);
        assert!(certs_expired.is_expired());
        assert_eq!(certs_expired.get_duration_until_refresh(), zero_dur);

        let future_certs = generate_test_certs(
            &id,
            Duration::from_secs(1000),
            Duration::from_secs(expiry_seconds),
        );
        assert!(!future_certs.is_expired());
        assert_eq!(future_certs.get_duration_until_refresh(), zero_dur);
    }
}