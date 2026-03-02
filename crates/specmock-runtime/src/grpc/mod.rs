//! gRPC runtime bridge for protobuf descriptors.

pub mod protobuf;

pub use protobuf::{GrpcRuntime, spawn_grpc_server};
