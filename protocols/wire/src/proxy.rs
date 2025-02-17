// Copyright (C) 2020  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

//! Implements  [PROXY protocol](http://www.haproxy.org/download/1.8/doc/proxy-protocol.txt) in tokio

use std::convert::TryInto;
use std::net::SocketAddr;

use bytes::Buf;
use bytes::BytesMut;
use futures::{Future, FutureExt, StreamExt};
use pin_project::pin_project;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::codec::{Decoder, Encoder, Framed, FramedParts};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::connection::Connection;
use crate::framing::Framing;
use codec::{v1::V1Codec, v2::V2Codec, MAX_HEADER_SIZE};
use error::{Error, Result};

pub mod codec;
pub mod error;
pub use codec::ProxyInfo;
use std::pin::Pin;

const V1_TAG: &[u8] = b"PROXY ";
const V2_TAG: &[u8] = codec::v2::SIGNATURE;

/// Information from proxy protocol are provided through `WithProxyInfo` trait,
/// which provides original addresses from PROXY protocol
///
/// For compatibility it's also implemented for `tokio::net::TcpSteam`, where it returns `None`
pub trait WithProxyInfo {
    // TODO: or original_source_addr - which one is better?
    /// Returns address of original source of the connection (client)
    fn original_peer_addr(&self) -> Option<SocketAddr> {
        None
    }

    /// Returns address of original destination of the connection (e.g. first proxy)
    fn original_destination_addr(&self) -> Option<SocketAddr> {
        None
    }

    fn proxy_info(&self) -> Result<ProxyInfo> {
        use std::convert::TryFrom;
        let original_source = self.original_peer_addr();
        let original_destination = self.original_destination_addr();
        ProxyInfo::try_from((original_source, original_destination))
    }
}

impl WithProxyInfo for TcpStream {}

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ProtocolVersion {
    V1,
    V2,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ProtocolConfig {
    /// When true, the PROXY protocol is enforced and the server will not accept any connection
    /// that doesn't initiate PROXY protocol session
    pub require_proxy_header: bool,
    /// Accepted versions of PROXY protocol on incoming connection
    pub versions: Vec<ProtocolVersion>,
}

impl ProtocolConfig {
    pub fn new(require_proxy_header: bool, versions: Vec<ProtocolVersion>) -> Self {
        Self {
            require_proxy_header,
            versions,
        }
    }
}

/// Struct to accept stream with PROXY header and extract information from it
pub struct Acceptor {
    require_proxy_header: bool,
}

impl Default for Acceptor {
    fn default() -> Self {
        Acceptor {
            require_proxy_header: false,
        }
    }
}

impl Acceptor {
    /// When auto-detecting the Proxy protocol header, this is the sufficient number of bytes that
    /// need to be initially received to decide whether any of the supported protocol variants
    const COMMON_HEADER_PREFIX_LEN: usize = 5;

    /// Process proxy protocol header, and autodetect PROXY protocol version and
    /// create [`ProxyStream`] with appropriate information in it.
    ///
    /// This method may block for ~2 secs until stream timeout is triggered when performing
    /// autodetection and waiting for `COMMON_HEADER_PREFIX_LEN` bytes to arrive.
    pub async fn accept_auto<T>(self, mut stream: T) -> Result<ProxyStream<T>>
    where
        T: AsyncRead + Send + Unpin,
    {
        trace!("wire: Accepting stream, autodetecting PROXY protocol version ");
        let mut buf = BytesMut::with_capacity(MAX_HEADER_SIZE);
        // This loop will block for ~2 seconds (read_buf() timeout) if less than
        // COMMON_HEADER_PREFIX_LEN have arrived
        while buf.len() < Self::COMMON_HEADER_PREFIX_LEN {
            let r = stream.read_buf(&mut buf).await?;
            trace!("wire: Read {} bytes from stream", r);
            if r == 0 {
                trace!("wire: no more bytes supplied in the stream, terminating read");
                break;
            }
        }

        if buf.remaining() < Self::COMMON_HEADER_PREFIX_LEN {
            return self.try_from_stream_to_proxy_stream(stream, buf);
        }
        debug!("wire: Buffered initial {} bytes", buf.remaining());

        if buf[0..Self::COMMON_HEADER_PREFIX_LEN] == V1_TAG[0..Self::COMMON_HEADER_PREFIX_LEN] {
            debug!("wire: Detected proxy protocol v1 tag");
            self.accept_with_codec(Some(buf), stream, V1Codec::new())
                .await
        } else if buf[0..Self::COMMON_HEADER_PREFIX_LEN]
            == V2_TAG[0..Self::COMMON_HEADER_PREFIX_LEN]
        {
            debug!("wire: Detected proxy protocol v2 tag");
            self.accept_with_codec(Some(buf), stream, V2Codec::new())
                .await
        } else {
            self.try_from_stream_to_proxy_stream(stream, buf)
        }
    }

    pub async fn accept_v1<T>(self, stream: T) -> Result<ProxyStream<T>>
    where
        T: AsyncRead + Send + Unpin,
    {
        debug!("wire: Accepting stream, decoding PROXY protocol V1");
        self.accept_with_codec(None, stream, V1Codec::new()).await
    }

    pub async fn accept_v2<T>(self, stream: T) -> Result<ProxyStream<T>>
    where
        T: AsyncRead + Send + Unpin,
    {
        debug!("wire: Accepting stream, decoding PROXY protocol V2");
        self.accept_with_codec(None, stream, V2Codec::new()).await
    }

    /// Conditionally convert the stream as long as the proxy header is not required or return an
    /// error
    fn try_from_stream_to_proxy_stream<T>(&self, stream: T, buf: BytesMut) -> Result<ProxyStream<T>>
    where
        T: AsyncRead + Unpin,
    {
        debug!(
            "wire: Trying to convert stream to dummy ProxyStream, bytes received: {:#x?}",
            buf
        );
        if self.require_proxy_header {
            debug!("wire: Proxy protocol is required");
            Err(Error::Proxy("Proxy protocol is required".into()))
        } else {
            debug!("wire: No proxy protocol detected, just passing the stream");
            Ok(ProxyStream {
                inner: stream,
                buf,
                orig_source: None,
                orig_destination: None,
            })
        }
    }

    /// Accept a PROXY protocol of version defined by `codec`. This helper method takes care of
    /// constructing Framed `read_buf`
    async fn accept_with_codec<C, T>(
        &self,
        read_buf: Option<BytesMut>,
        stream: T,
        codec: C,
    ) -> Result<ProxyStream<T>>
    where
        T: AsyncRead + Unpin,
        C: Encoder<ProxyInfo> + Decoder<Item = ProxyInfo, Error = Error>,
    {
        let mut framed_parts = FramedParts::new(stream, codec);
        if let Some(read_buf) = read_buf {
            framed_parts.read_buf = read_buf;
        }
        let mut framed = Framed::from_parts(framed_parts);

        let proxy_info_result = framed
            .next()
            .await
            .ok_or_else(|| Error::Proxy("Stream terminated".into()))?;

        let parts = framed.into_parts();

        match proxy_info_result {
            Ok(proxy_info) => Ok(ProxyStream {
                inner: parts.io,
                buf: parts.read_buf,
                orig_source: proxy_info.original_source,
                orig_destination: proxy_info.original_destination,
            }),
            Err(e) => {
                debug!("wire: PROXY protocol header not present: {}", e);
                self.try_from_stream_to_proxy_stream(parts.io, parts.read_buf)
            }
        }
    }

    /// Creates new default `Acceptor`
    pub fn new() -> Self {
        Acceptor::default()
    }

    /// If true (default) PROXY header is required in accepted stream, if not present error is raised.
    /// If set to false, then if PROXY header is not present, stream is created and all data passed on.
    /// Original addresses then are not known indeed.
    pub fn require_proxy_header(self, require_proxy_header: bool) -> Self {
        Acceptor {
            require_proxy_header,
        }
    }
}

/// Represent a prepared acceptor for processing incoming bytes
pub type AcceptorFuture<T> = Pin<Box<dyn Future<Output = Result<ProxyStream<T>>> + Send>>;

/// Internal builder method selected based on configuration used when constructing `AcceptorBuilder`
type BuildMethod<T> = fn(&AcceptorBuilder<T>, T) -> AcceptorFuture<T>;

/// Builder is carries configuration for a future acceptor and is preconfigured early
/// to build an Acceptor in suitable state
#[derive(Clone)]
pub struct AcceptorBuilder<T> {
    config: ProtocolConfig,
    /// Build method for a particular Acceptor variant is selected based on provided configuration
    build_method: BuildMethod<T>,
}

impl<T> AcceptorBuilder<T>
where
    T: AsyncRead + Send + Unpin + 'static,
{
    pub fn new(config: ProtocolConfig) -> Self {
        // TODO for now, we only provide hardcoded autodetect build method
        let build_method = match config.versions.len() {
            0 => {
                assert!(
                    !config.require_proxy_header,
                    "BUG: inconsistent config, proxy header is required and no supported \
                     version has been specified unsupported"
                );
                Self::build_skip
            }
            1 => {
                if config.require_proxy_header {
                    match config.versions[0] {
                        ProtocolVersion::V1 => Self::build_v1,
                        ProtocolVersion::V2 => Self::build_v2,
                    }
                } else {
                    info!(
                        "wire: Ignoring direct PROXY protocol version config ({:?}), using auto \
                         detection since proxy protocol is not enforced in the configuration",
                        config.versions[0]
                    );
                    Self::build_auto
                }
            }
            _ => Self::build_auto,
        };

        Self {
            config,
            build_method,
        }
    }

    pub fn build(&self, stream: T) -> AcceptorFuture<T> {
        (self.build_method)(self, stream)
    }

    /// Builds a special future that only passes back the `stream` wrapped in ProxyStream
    fn build_skip(&self, stream: T) -> AcceptorFuture<T> {
        async move {
            Ok(ProxyStream {
                inner: stream,
                buf: BytesMut::new(),
                orig_source: None,
                orig_destination: None,
            })
        }
        .boxed()
    }
    fn build_auto(&self, stream: T) -> AcceptorFuture<T> {
        let acceptor = Acceptor::new().require_proxy_header(self.config.require_proxy_header);

        acceptor.accept_auto(stream).boxed()
    }

    fn build_v1(&self, stream: T) -> AcceptorFuture<T> {
        let acceptor = Acceptor::new().require_proxy_header(self.config.require_proxy_header);

        acceptor.accept_v1(stream).boxed()
    }

    fn build_v2(&self, stream: T) -> AcceptorFuture<T> {
        let acceptor = Acceptor::new().require_proxy_header(self.config.require_proxy_header);

        acceptor.accept_v2(stream).boxed()
    }
}

/// `Connector` enables to add PROXY protocol header to outgoing stream
pub struct Connector {
    protocol_version: ProtocolVersion,
}

impl Connector {
    /// If `use_v2` is true, v2 header will be added
    pub fn new(protocol_version: ProtocolVersion) -> Self {
        Connector { protocol_version }
    }

    /// Creates outgoing TCP connection with appropriate PROXY protocol header
    pub async fn connect(
        &self,
        addr: crate::Address,
        original_source: Option<SocketAddr>,
        original_destination: Option<SocketAddr>,
    ) -> Result<TcpStream> {
        let mut stream = TcpStream::connect(addr.as_ref()).await?;
        self.write_proxy_header(&mut stream, original_source, original_destination)
            .await?;
        Ok(stream)
    }

    /// Adds appropriate PROXY protocol header to given stream
    pub async fn write_proxy_header<T: AsyncWrite + Unpin>(
        &self,
        dest: &mut T,
        original_source: Option<SocketAddr>,
        original_destination: Option<SocketAddr>,
    ) -> Result<()> {
        let proxy_info = (original_source, original_destination).try_into()?;
        let mut data = BytesMut::new();
        match self.protocol_version {
            ProtocolVersion::V1 => V1Codec::new().encode(proxy_info, &mut data)?,
            ProtocolVersion::V2 => V2Codec::new().encode(proxy_info, &mut data)?,
        }

        dest.write(&data).await?;
        Ok(())
    }
}

/// Stream containing information from PROXY protocol
///
#[pin_project]
#[derive(Debug)]
pub struct ProxyStream<T> {
    #[pin]
    inner: T,
    buf: BytesMut,
    orig_source: Option<SocketAddr>,
    orig_destination: Option<SocketAddr>,
}

impl<T> ProxyStream<T> {
    /// Returns inner stream, but
    /// only when it is save, e.g. no data in buffer
    pub fn try_into_inner(self) -> Result<T> {
        if self.buf.is_empty() {
            Ok(self.inner)
        } else {
            Err(Error::InvalidState(
                "Cannot return inner steam because buffer is not empty".into(),
            ))
        }
    }

    /// Direct conversion to FramedParts with arbitrary codec. It eliminates the problem with
    /// `From` implementation that also exists but doesn't simply allow using the 'I' parameter.
    /// See additional notes in `From`
    pub fn into_framed_parts<C, I>(self: ProxyStream<T>) -> FramedParts<T, C>
    where
        C: Encoder<I> + Decoder + Default,
    {
        let mut parts = FramedParts::new(self.inner, C::default());
        parts.read_buf = self.buf;
        parts
    }
}

impl<T> AsRef<T> for ProxyStream<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

// with Deref we can get automatic coercion for some TcpStream methods

impl std::ops::Deref for ProxyStream<TcpStream> {
    type Target = TcpStream;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

// TODO: do we want to allow this - because it can cause problem, if used unwisely
// actually DerefMut mut can be quite dangerous, because it'll enable to inner stream, while some data are already in buffer
// impl std::ops::DerefMut for ProxyStream<TcpStream> {
//     fn deref_mut(&mut self) -> &mut Self::Target {
//         &mut self.inner
//     }
// }

// same for AsMut - try to not to use it
// impl<T> AsMut<T> for ProxyStream<T> {
//     fn as_mut(&mut self) -> &mut T {
//         &mut self.inner
//     }
// }

impl<T> WithProxyInfo for ProxyStream<T> {
    fn original_peer_addr(&self) -> Option<SocketAddr> {
        self.orig_source
    }

    fn original_destination_addr(&self) -> Option<SocketAddr> {
        self.orig_destination
    }
}

impl<T: AsyncRead + Send + Unpin> ProxyStream<T> {
    pub async fn new(stream: T) -> Result<Self> {
        Acceptor::default().accept_auto(stream).await
    }
}

impl<F> From<ProxyStream<TcpStream>> for Connection<F>
where
    F: Framing,
    F::Codec: Default,
{
    fn from(stream: ProxyStream<TcpStream>) -> Self {
        let mut parts = FramedParts::new(stream.inner, F::Codec::default());
        parts.read_buf = stream.buf; // pass existing read buffer
        Connection {
            framed_stream: Framed::from_parts(parts),
        }
    }
}

impl<C> From<ProxyStream<TcpStream>> for Framed<TcpStream, C>
where
    C: Encoder<ProxyInfo> + Decoder + Default,
{
    fn from(stream: ProxyStream<TcpStream>) -> Self {
        let parts = FramedParts::from(stream);
        Framed::from_parts(parts)
    }
}

/// NOTE: if conversion to FramedParts with arbitrary codec is needed, use
/// ProxyStream::into_framed_parts. The problem here is that we cannot replace ProxyInfo with `I`
/// generic parameter as it would require adding a phantom generic parameter to ProxyStream
/// (see E207)
impl<C> From<ProxyStream<TcpStream>> for FramedParts<TcpStream, C>
where
    C: Encoder<ProxyInfo> + Decoder + Default,
{
    fn from(stream: ProxyStream<TcpStream>) -> Self {
        let mut parts = FramedParts::new(stream.inner, C::default());
        parts.read_buf = stream.buf;
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryFrom;
    use std::net::IpAddr;

    /// Test codec for verifying that a message flows correctly through ProxyStream
    struct TestCodec {
        /// Test message that
        test_message: Vec<u8>,
        buf: BytesMut,
    }

    impl TestCodec {
        pub fn new(test_message: Vec<u8>) -> Self {
            let test_message_len = test_message.len();
            Self {
                test_message,
                buf: BytesMut::with_capacity(test_message_len),
            }
        }
    }

    impl Decoder for TestCodec {
        type Item = Vec<u8>;
        type Error = Error;

        fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>> {
            let received = buf.split();
            self.buf.unsplit(received);
            if self.buf.len() == self.test_message.len() {
                let message_bytes = self.buf.split();
                let item: Vec<u8> = message_bytes[..].into();
                Ok(Some(item))
            } else {
                Ok(None)
            }
        }
    }

    impl Encoder<Vec<u8>> for TestCodec {
        type Error = Error;
        fn encode(&mut self, _item: Vec<u8>, _header: &mut BytesMut) -> Result<()> {
            Err(Error::Proxy("Encoding not to be tested".into()))
        }
    }

    /// Helper that
    async fn read_and_compare_message<T: AsyncRead + Unpin>(
        proxy_stream: ProxyStream<T>,
        test_message: Vec<u8>,
    ) {
        let mut framed_parts =
            FramedParts::new(proxy_stream.inner, TestCodec::new(test_message.clone()));
        framed_parts.read_buf = proxy_stream.buf;
        let mut framed = Framed::from_parts(framed_parts);

        let passed_message = framed
            .next()
            .await
            .expect("BUG: Unexpected end of stream")
            .expect("BUG: Failed to read message from the stream");
        assert_eq!(
            passed_message, test_message,
            "BUG: Message didn't flow successfully"
        );
    }

    #[tokio::test]
    async fn test_v1_tcp4() {
        const HELLO: &'static [u8] = b"HELLO";
        let message = "PROXY TCP4 192.168.0.1 192.168.0.11 56324 443\r\nHELLO".as_bytes();
        let ps = Acceptor::new()
            .accept_auto(message)
            .await
            .expect("BUG: Cannot accept message");
        assert_eq!(
            "192.168.0.1:56324"
                .parse::<SocketAddr>()
                .expect("BUG: Cannot parse IP"),
            ps.original_peer_addr()
                .expect("BUG: Cannot parse original peer IP")
        );
        assert_eq!(
            "192.168.0.11:443"
                .parse::<SocketAddr>()
                .expect("BUG: Cannot parse IP"),
            ps.original_destination_addr()
                .expect("BUG: Cannot parse original dest IP")
        );
        read_and_compare_message(ps, Vec::from(HELLO)).await;
    }

    #[tokio::test]
    async fn test_v2tcp4() {
        let mut message = Vec::new();
        message.extend_from_slice(V2_TAG);
        message.extend(&[
            0x21, 0x11, 0, 12, 192, 168, 0, 1, 192, 168, 0, 11, 0xdc, 0x04, 1, 187,
        ]);
        message.extend(b"Hello");

        let ps = Acceptor::new()
            .accept_auto(&message[..])
            .await
            .expect("BUG: V2 message not accepted");
        assert_eq!(
            "192.168.0.1:56324"
                .parse::<SocketAddr>()
                .expect("BUG: Cannot parse IP"),
            ps.original_peer_addr()
                .expect("BUG: Cannot parse original peer IP")
        );
        assert_eq!(
            "192.168.0.11:443"
                .parse::<SocketAddr>()
                .expect("BUG: Cannot parse IP"),
            ps.original_destination_addr()
                .expect("BUG: Cannot parse original dest IP")
        );
        assert_eq!(
            b"Hello",
            &ps.buf[..],
            "BUG: Expected message not stored in ProxyStream"
        );
    }

    #[tokio::test]
    async fn test_v1_unknown_long_message() {
        let mut message = "PROXY UNKNOWN\r\n".to_string();
        //const DATA_LENGTH: usize = 1_000_000;
        const DATA_LENGTH: usize = 3;
        let data: Vec<u8> = (b'A'..=b'Z').cycle().take(DATA_LENGTH).collect();

        let data_str = String::from_utf8(data.clone()).expect("BUG: cannot build test large data");
        message.push_str(data_str.as_str());

        let ps = ProxyStream::new(message.as_bytes())
            .await
            .expect("BUG: cannot create ProxyStream");
        read_and_compare_message(ps, Vec::from(data)).await;
    }

    #[tokio::test]
    async fn test_no_proxy_header_passed() {
        const MESSAGE: &'static [u8] = b"MEMAM PROXY HEADER, CHUDACEK JA";

        let ps = ProxyStream::new(&MESSAGE[..])
            .await
            .expect("BUG: cannot create ProxyStream");
        assert!(ps.original_peer_addr().is_none());
        assert!(ps.original_destination_addr().is_none());
        read_and_compare_message(ps, Vec::from(MESSAGE)).await;
    }

    #[tokio::test]
    async fn test_no_proxy_header_rejected() {
        let message = b"MEMAM PROXY HEADER, CHUDACEK JA";
        let ps = Acceptor::new()
            .require_proxy_header(true)
            .accept_auto(&message[..])
            .await;
        assert!(ps.is_err());
    }

    #[tokio::test]
    async fn test_too_short_message_fail() {
        let message = b"NIC\r\n";
        let ps = Acceptor::new()
            .require_proxy_header(true)
            .accept_auto(&message[..])
            .await;
        assert!(ps.is_err());
    }

    #[tokio::test]
    async fn test_too_short_message_pass() {
        const MESSAGE: &'static [u8] = b"NIC\r\n";
        let ps = Acceptor::new()
            .require_proxy_header(false)
            .accept_auto(&MESSAGE[..])
            .await
            .expect("BUG: Cannot accept message");
        read_and_compare_message(ps, Vec::from(MESSAGE)).await;
    }

    /// Verify that a test message is succesfully passed through the Acceptor and leaves it
    /// untouched in the form of ProxyStream with prepared buffer. We use framed with a test
    /// codec to actually collect the message again
    #[tokio::test]
    async fn test_short_message_retention_via_proxy_stream() {
        const MESSAGE: &'static [u8] = b"NIC\r\n";
        let ps = Acceptor::new()
            .require_proxy_header(false)
            .accept_auto(&MESSAGE[..])
            .await
            .expect("BUG: Cannot accept incoming message");

        read_and_compare_message(ps, Vec::from(MESSAGE)).await;
    }

    #[tokio::test]
    async fn test_connect() {
        let mut buf = Vec::new();
        let src = "127.0.0.1:1111"
            .parse::<SocketAddr>()
            .expect("BUG: Cannot parse IP");
        let dest = "127.0.0.1:2222"
            .parse::<SocketAddr>()
            .expect("BUG: Cannot parse IP");
        let _res = Connector::new(ProtocolVersion::V1)
            .write_proxy_header(&mut buf, Some(src), Some(dest))
            .await
            .expect("BUG: Cannot write proxy header");
        let expected = "PROXY TCP4 127.0.0.1 127.0.0.1 1111 2222\r\n";
        assert_eq!(expected.as_bytes(), &buf[..]);
    }

    /// Helper that allows testing `AcceptorBuilder` that it internally configures the correct
    /// build method that matches `expected_build_method` based on a specified protocol version
    fn test_acceptor_builder(
        require_proxy_header: bool,
        versions: Vec<ProtocolVersion>,
        expected_build_method: BuildMethod<&[u8]>,
        method_suffix: &str,
    ) {
        let acceptor_builder: AcceptorBuilder<&[u8]> =
            AcceptorBuilder::new(ProtocolConfig::new(require_proxy_header, versions));

        let actual = acceptor_builder.build_method as *const BuildMethod<&[u8]>;
        let expected = expected_build_method as *const BuildMethod<&[u8]>;

        assert_eq!(
            actual, expected,
            "BUG: Expected 'build_{}' method",
            method_suffix
        );
    }

    /// Verify that build_skip method has been selected = no proxy handling
    #[test]
    fn acceptor_builder_skip() {
        test_acceptor_builder(false, vec![], AcceptorBuilder::<&[u8]>::build_skip, "skip");
    }

    /// Verify that build_skip method is not selected and the builder panics with a bug due to
    /// selecting no supported versions and requiring proxy header
    #[test]
    #[should_panic]
    fn acceptor_builder_panic() {
        test_acceptor_builder(true, vec![], AcceptorBuilder::<&[u8]>::build_skip, "skip");
    }

    /// Verify that build_v1 method has been selected
    #[test]
    fn acceptor_builder_v1() {
        test_acceptor_builder(
            true,
            vec![ProtocolVersion::V1],
            AcceptorBuilder::<&[u8]>::build_v1,
            "v1",
        );
    }

    /// Verify that build_v2 method has been selected
    #[test]
    fn acceptor_builder_v2() {
        test_acceptor_builder(
            true,
            vec![ProtocolVersion::V2],
            AcceptorBuilder::<&[u8]>::build_v2,
            "v2",
        );
    }

    /// Verify that build_auto method has been selected regardless of the protocol version when
    /// proxy header is not required
    #[test]
    fn acceptor_builder_v1_auto() {
        test_acceptor_builder(
            false,
            vec![ProtocolVersion::V1],
            AcceptorBuilder::<&[u8]>::build_auto,
            "auto",
        );
    }

    /// Verify that build_auto method has been selected regardless of the protocol version when
    /// proxy header is not required
    #[test]
    fn acceptor_builder_v2_auto() {
        test_acceptor_builder(
            false,
            vec![ProtocolVersion::V1],
            AcceptorBuilder::<&[u8]>::build_auto,
            "auto",
        );
    }

    /// Verify that build_auto method has been detected
    #[test]
    fn acceptor_builder_auto() {
        test_acceptor_builder(
            false,
            vec![ProtocolVersion::V1, ProtocolVersion::V2],
            AcceptorBuilder::<&[u8]>::build_auto,
            "auto",
        );
    }

    #[test]
    fn correct_proxy_info_format() {
        let src = SocketAddr::new(IpAddr::from([5, 4, 3, 2]), 5432);
        let dst = SocketAddr::new(IpAddr::from([4, 5, 6, 7]), 4567);
        let proxy_info =
            ProxyInfo::try_from((Some(src), Some(dst))).expect("BUG: cannot produce proxy info");
        assert_eq!(
            format!("{}", proxy_info),
            String::from("ProxyInfo[SRC:5.4.3.2:5432, DST:4.5.6.7:4567]")
        );

        let empty_proxy_info =
            ProxyInfo::try_from((None, None)).expect("BUG: cannot produce proxy info");
        assert_eq!(
            format!("{}", empty_proxy_info),
            String::from("ProxyInfo[SRC:N/A, DST:N/A]")
        );
    }
}
