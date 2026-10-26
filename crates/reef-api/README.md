# reef-api

The API of **Reef**, version control built for a world where AI agents write most of the code: the `reef.api.v1` protobuf schema in [`proto/reef/api/v1`](proto/reef/api/v1), and its generated Rust messages, clients (`<service>_client`) and servers (`<service>_server`).

The schema is the API of record. Its conventions, error model, versioning rules and review checklist are [ADR 0029](../../docs/adr/0029-daemon-api-v1.md): v1 only grows, and every change bumps `API_REVISION`. [buf](https://buf.build) checks the protos' lint, formatting and breaking changes (`just api`, and CI's `API schema` job), but building this crate needs neither buf nor `protoc`: `build.rs` compiles the protos with `protox`.

The crate also exports the encoded descriptor set (`FILE_DESCRIPTOR_SET`, for SDK generators and reflection), each method's access level (`access`), and the `google.rpc.Status` envelope of every error's `ErrorDetail` (`error`). `proto/reef/daemon/v0` holds the two files of 0.1's API that a daemon still serves for version detection; they are frozen.

It depends on no other Reef crate. The daemon (`reef-daemon`) serves the API and the CLI (`reef-vcs`) calls it; the design document and roadmap are at <https://github.com/reef-vcs/reef>.
