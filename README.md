# FleetFS
[![Build Status](https://travis-ci.com/fleetfs/fleetfs.svg?branch=master)](https://travis-ci.com/fleetfs/fleetfs)
[![Crates](https://img.shields.io/crates/v/fleetfs.svg)](https://crates.io/crates/fleetfs)
[![Documentation](https://docs.rs/fleetfs/badge.svg)](https://docs.rs/fleetfs)
[![dependency status](https://deps.rs/repo/github/fleetfs/fleetfs/status.svg)](https://deps.rs/repo/github/fleetfs/fleetfs)
[![FOSSA Status](https://app.fossa.io/api/projects/git%2Bgithub.com%2Ffleetfs%2Ffleetfs.svg?type=shield)](https://app.fossa.io/projects/git%2Bgithub.com%2Ffleetfs%2Ffleetfs?ref=badge_shield)

FleetFS distributed filesystem

## Development
* `apt install libfuse-dev` (needed by `fuse` dependency)
* Install flatc (https://github.com/google/flatbuffers)
* rustup component add rustfmt
* rustup component add clippy

## Status
Very very alpha. Expect FleetFS to eat your data :)

**Features implemented:**
* basic operations: read/write/create/delete/rename
* file permissions (read/write/exec), but not ownership

## Design decisions
* Clients only need to talk to a single node
  * Context: There is significant overhead in opening TCP connections, so we want the client to keep its
  connections open. Therefore, the client shouldn't make on-demand connections to every storage node in
  the cluster.
  * Cons: doubles network traffic inside FleetFS cluster, as nodes have to proxy traffic reading/writing
  to other nodes
  * Pros: better scalability, as client connection is handled by a single node. Also simplifies client code
* Clients are trusted to make permission checks
  * Context: FleetFS has no access to a central user store, so has to trust the user ids sent by the client
  * Cons: security relies on the client
  * Pros: client doesn't have to send exhaustive list of groups that user is part of to make permission checks

## License
[![FOSSA Status](https://app.fossa.io/api/projects/git%2Bgithub.com%2Ffleetfs%2Ffleetfs.svg?type=large)](https://app.fossa.io/projects/git%2Bgithub.com%2Ffleetfs%2Ffleetfs?ref=badge_large)