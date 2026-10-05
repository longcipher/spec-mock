//! End-to-end tests for scenario-driven gRPC mocking.
//!
//! A method is addressed as `unary /package.Service/Method` and scenario
//! directives travel as lowercase gRPC metadata, so one sidecar format and one
//! set of directive names cover all three protocols.
//!
//! tonic 0.14 removed the built-in `ProstCodec`, hence the hand-rolled pair
//! below, which mirrors the one in `grpc_protobuf.rs`.

use std::{marker::PhantomData, net::SocketAddr, path::PathBuf};

use prost::Message;
use specmock_core::MockMode;
use specmock_runtime::{RuntimeError, ServerConfig, start};
use tonic::{
    client::Grpc,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
};

struct WidgetsCodec<T, U>(PhantomData<(T, U)>);

impl<T, U> Default for WidgetsCodec<T, U> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

struct WidgetsEncoder<T>(PhantomData<T>);
struct WidgetsDecoder<U>(PhantomData<U>);

impl<T: Message + 'static, U: Message + Default + 'static> Codec for WidgetsCodec<T, U> {
    type Encode = T;
    type Decode = U;
    type Encoder = WidgetsEncoder<T>;
    type Decoder = WidgetsDecoder<U>;

    fn encoder(&mut self) -> Self::Encoder {
        WidgetsEncoder(PhantomData)
    }
    fn decoder(&mut self) -> Self::Decoder {
        WidgetsDecoder(PhantomData)
    }
}

impl<T: Message> Encoder for WidgetsEncoder<T> {
    type Item = T;
    type Error = tonic::Status;

    fn encode(&mut self, item: Self::Item, buf: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(buf).map_err(|error| tonic::Status::internal(error.to_string()))
    }
}

impl<U: Message + Default> Decoder for WidgetsDecoder<U> {
    type Item = U;
    type Error = tonic::Status;

    fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        U::decode(buf).map(Some).map_err(|error| tonic::Status::internal(error.to_string()))
    }
}

#[derive(Clone, PartialEq, Message)]
struct CreateRequest {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
}

#[derive(Clone, PartialEq, Message)]
struct GetRequest {
    #[prost(string, tag = "1")]
    id: String,
}

#[derive(Clone, PartialEq, Message)]
struct UnstableRequest {
    #[prost(string, tag = "1")]
    id: String,
}

#[derive(Clone, PartialEq, Message)]
struct Widget {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
}

fn spec(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("specs").join(name)
}

async fn boot() -> Result<Option<specmock_runtime::RunningServer>, Box<dyn std::error::Error>> {
    let config = ServerConfig {
        proto_spec: Some(spec("widgets.proto")),
        scenarios_spec: Some(spec("widgets.scenarios.yaml")),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        grpc_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };

    match start(config).await {
        Ok(server) => Ok(Some(server)),
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Ok(None)
        }
        Err(error) => Err(error.to_string().into()),
    }
}

const CREATE: &str = "/widgets.Widgets/Create";
const GET: &str = "/widgets.Widgets/Get";
const UNSTABLE: &str = "/widgets.Widgets/Unstable";

/// tonic takes a `PathAndQuery`, and the mock's router keys on the path.
fn path(value: &'static str) -> http::uri::PathAndQuery {
    http::uri::PathAndQuery::from_static(value)
}

/// Make one unary call over a fresh connection.
///
/// A fresh channel per call, deliberately: reusing a single `Grpc` across
/// sequential requests trips tower's `send_item called without first calling
/// poll_reserve`. The existing gRPC tests never notice because each makes exactly
/// one call.
async fn connect(
    addr: SocketAddr,
) -> Result<Grpc<tonic::transport::Channel>, Box<dyn std::error::Error>> {
    let channel =
        tonic::transport::Channel::from_shared(format!("http://{addr}"))?.connect().await?;
    let mut client = Grpc::new(channel);
    client.ready().await?;
    Ok(client)
}

#[tokio::test]
async fn a_created_widget_comes_back_on_a_later_get() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let Some(addr) = server.grpc_addr else {
        return Ok(());
    };
    let mut client = connect(addr).await?;

    let created: Widget = client
        .unary(
            tonic::Request::new(CreateRequest { id: "w1".to_owned(), name: "First".to_owned() }),
            path(CREATE),
            WidgetsCodec::<CreateRequest, Widget>::default(),
        )
        .await?
        .into_inner();
    assert!(!created.id.is_empty(), "the mock must return an identifier: {created:?}");

    let fetched: Widget = connect(addr)
        .await?
        .unary(
            tonic::Request::new(GetRequest { id: created.id.clone() }),
            path(GET),
            WidgetsCodec::<GetRequest, Widget>::default(),
        )
        .await?
        .into_inner();

    assert_eq!(
        fetched, created,
        "the stored widget must be exactly what create returned, not a fresh generation"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn getting_an_unknown_widget_reports_not_found() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let Some(addr) = server.grpc_addr else {
        return Ok(());
    };
    let mut client = connect(addr).await?;

    let status = client
        .unary(
            tonic::Request::new(GetRequest { id: "ghost".to_owned() }),
            path(GET),
            WidgetsCodec::<GetRequest, Widget>::default(),
        )
        .await
        .expect_err("an unknown widget must fail");

    assert_eq!(status.code(), tonic::Code::NotFound, "mapping from the HTTP-shaped 404");
    assert!(status.message().contains("ghost"), "the message must name the resource: {status:?}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_state_specific_response_surfaces_as_a_grpc_status()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let Some(addr) = server.grpc_addr else {
        return Ok(());
    };
    // `steady` answers normally, then the flow lands in `broken`.
    let unstable = || UnstableRequest { id: "u1".to_owned() };
    let first: Widget = connect(addr)
        .await?
        .unary(
            tonic::Request::new(unstable()),
            path(UNSTABLE),
            WidgetsCodec::<UnstableRequest, Widget>::default(),
        )
        .await?
        .into_inner();
    assert!(!first.id.is_empty(), "the first call is served: {first:?}");

    let status = connect(addr)
        .await?
        .unary(
            tonic::Request::new(unstable()),
            path(UNSTABLE),
            WidgetsCodec::<UnstableRequest, Widget>::default(),
        )
        .await
        .expect_err("the broken state declares a 13");
    assert_eq!(
        status.code(),
        tonic::Code::Internal,
        "the `broken` state's declared 13 is INTERNAL over gRPC: {status:?}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn sessions_isolate_stores_across_calls() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let Some(addr) = server.grpc_addr else {
        return Ok(());
    };
    let create = |session: &'static str| -> tonic::Request<CreateRequest> {
        let mut request = tonic::Request::new(CreateRequest {
            id: "shared".to_owned(),
            name: "Shared".to_owned(),
        });
        if let Ok(value) = session.parse() {
            request.metadata_mut().insert("session", value);
        }
        request
    };

    connect(addr)
        .await?
        .unary(create("alpha"), path(CREATE), WidgetsCodec::<CreateRequest, Widget>::default())
        .await?;

    connect(addr)
        .await?
        .unary(create("beta"), path(CREATE), WidgetsCodec::<CreateRequest, Widget>::default())
        .await
        .map_err(|_| "a different session has its own store, so this must succeed")?;

    let status = connect(addr)
        .await?
        .unary(create("alpha"), path(CREATE), WidgetsCodec::<CreateRequest, Widget>::default())
        .await
        .expect_err("repeating a create in one session must collide");
    assert_eq!(
        status.code(),
        tonic::Code::FailedPrecondition,
        "mapping from the 409 collision: {status:?}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_unknown_scenario_name_is_reported() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let Some(addr) = server.grpc_addr else {
        return Ok(());
    };
    let mut client = connect(addr).await?;

    let mut request = tonic::Request::new(GetRequest { id: "x".to_owned() });
    request.metadata_mut().insert("scenario", "ghost".parse()?);
    let status = client
        .unary(request, path(GET), WidgetsCodec::<GetRequest, Widget>::default())
        .await
        .expect_err("an unknown scenario must fail");

    assert_eq!(status.code(), tonic::Code::NotFound);
    assert!(status.message().contains("ghost"), "got {status:?}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_method_no_scenario_mentions_is_served_normally() -> Result<(), Box<dyn std::error::Error>>
{
    // A sidecar is an additive overlay: `Unstable` is governed, so use `Get`
    // against a store the scenario does not govern to prove nothing is refused
    // merely for being unlisted. A plain read of a seeded-style resource still
    // reaches the store, so assert the error is the *store's*, not a routing one.
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let Some(addr) = server.grpc_addr else {
        return Ok(());
    };
    let mut client = connect(addr).await?;

    let status = client
        .unary(
            tonic::Request::new(GetRequest { id: "absent".to_owned() }),
            path(GET),
            WidgetsCodec::<GetRequest, Widget>::default(),
        )
        .await
        .expect_err("nothing has been created yet");

    assert_eq!(
        status.code(),
        tonic::Code::NotFound,
        "the store's own answer, not an unlisted-operation refusal: {status:?}"
    );

    server.shutdown().await;
    Ok(())
}
