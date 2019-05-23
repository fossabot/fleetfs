use std::error::Error;
use std::{fs, io};

use flatbuffers::{FlatBufferBuilder, UnionWIPOffset, WIPOffset};
use futures::Stream;
use tokio::codec::length_delimited;
use tokio::net::TcpListener;
use tokio::prelude::*;

use crate::file_handler::FileRequestHandler;
use crate::generated::*;
use crate::peer_client::PeerClient;
use crate::raft_manager::RaftManager;
use crate::utils::{
    empty_response, into_error_code, is_raft_request, is_write_request, ResultResponse,
    WritableFlatBuffer,
};
use futures::future::{ok, result};
use protobuf::Message as ProtobufMessage;
use raft::eraftpb::Message;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::timer::Interval;
use walkdir::WalkDir;

fn checksum(data_dir: &str) -> io::Result<Vec<u8>> {
    let mut hasher = Sha256::new();
    for entry in WalkDir::new(data_dir).sort_by(|a, b| a.file_name().cmp(b.file_name())) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let mut file = fs::File::open(entry.path())?;

            // TODO hash the path and file attributes too
            io::copy(&mut file, &mut hasher)?;
        }
        // TODO handle other file types
    }
    return Ok(hasher.result().to_vec());
}

fn fsck<'a>(
    context: &LocalContext,
    mut builder: FlatBufferBuilder<'a>,
) -> impl Future<
    Item = (
        FlatBufferBuilder<'a>,
        ResponseType,
        WIPOffset<UnionWIPOffset>,
    ),
    Error = ErrorCode,
> {
    let future_checksum = result(checksum(&context.data_dir).map_err(into_error_code));
    let mut peer_futures = vec![];
    for peer in context.peers.iter() {
        let client = PeerClient::new(*peer);
        peer_futures.push(client.filesystem_checksum().map_err(into_error_code));
    }

    futures::future::join_all(peer_futures)
        .join(future_checksum)
        .map(move |(peer_checksums, checksum)| {
            for peer_checksum in peer_checksums {
                if checksum != peer_checksum {
                    let args = ErrorResponseArgs {
                        error_code: ErrorCode::Corrupted,
                    };
                    let response_offset =
                        ErrorResponse::create(&mut builder, &args).as_union_value();
                    return (builder, ResponseType::ErrorResponse, response_offset);
                }
            }

            return empty_response(builder).unwrap();
        })
}

fn checksum_request<'a>(
    local_context: &LocalContext,
    mut builder: FlatBufferBuilder<'a>,
) -> ResultResponse<'a> {
    let checksum = checksum(&local_context.data_dir).map_err(|_| ErrorCode::Uncategorized)?;
    let data_offset = builder.create_vector_direct(&checksum);
    let mut response_builder = ReadResponseBuilder::new(&mut builder);
    response_builder.add_data(data_offset);
    let response_offset = response_builder.finish().as_union_value();

    return Ok((builder, ResponseType::ReadResponse, response_offset));
}
pub fn raft_message_handler<'a, 'b>(
    request: GenericRequest<'a>,
    raft_manager: &RaftManager,
    mut builder: FlatBufferBuilder<'b>,
) -> impl Future<Item = FlatBufferBuilder<'b>, Error = ErrorCode> {
    let response: Box<
        Future<
                Item = (
                    FlatBufferBuilder<'b>,
                    ResponseType,
                    WIPOffset<UnionWIPOffset>,
                ),
                Error = ErrorCode,
            > + Send,
    >;

    match request.request_type() {
        RequestType::FilesystemCheckRequest => unreachable!(),
        RequestType::FilesystemChecksumRequest => unreachable!(),
        RequestType::ReadRequest => unreachable!(),
        RequestType::HardlinkRequest => unreachable!(),
        RequestType::RenameRequest => unreachable!(),
        RequestType::ChmodRequest => unreachable!(),
        RequestType::TruncateRequest => unreachable!(),
        RequestType::UnlinkRequest => unreachable!(),
        RequestType::WriteRequest => unreachable!(),
        RequestType::UtimensRequest => unreachable!(),
        RequestType::ReaddirRequest => unreachable!(),
        RequestType::GetattrRequest => unreachable!(),
        RequestType::MkdirRequest => unreachable!(),
        RequestType::RaftRequest => {
            let raft_request = request.request_as_raft_request().unwrap();
            let mut deserialized_message = Message::new();
            deserialized_message
                .merge_from_bytes(raft_request.message())
                .unwrap();
            raft_manager
                .apply_messages(&[deserialized_message])
                .unwrap();
            response = Box::new(result(empty_response(builder)));
        }
        RequestType::LatestCommitRequest => {
            let index = raft_manager.get_latest_local_commit();
            let mut response_builder = LatestCommitResponseBuilder::new(&mut builder);
            response_builder.add_index(index);
            let response_offset = response_builder.finish().as_union_value();
            response = Box::new(ok((
                builder,
                ResponseType::LatestCommitResponse,
                response_offset,
            )));
        }
        RequestType::GetLeaderRequest => {
            let leader_future = raft_manager
                .get_leader()
                .map(move |leader_id| {
                    let mut response_builder = NodeIdResponseBuilder::new(&mut builder);
                    response_builder.add_node_id(leader_id);
                    let response_offset = response_builder.finish().as_union_value();
                    (builder, ResponseType::NodeIdResponse, response_offset)
                })
                .map_err(|_| ErrorCode::Uncategorized);

            response = Box::new(leader_future);
        }
        RequestType::NONE => unreachable!(),
    }

    response.map(|(mut builder, response_type, response_offset)| {
        let mut generic_response_builder = GenericResponseBuilder::new(&mut builder);
        generic_response_builder.add_response_type(response_type);
        generic_response_builder.add_response(response_offset);

        let final_response_offset = generic_response_builder.finish();
        builder.finish_size_prefixed(final_response_offset, None);

        builder
    })
}

pub fn handler<'a, 'b>(
    request: GenericRequest<'a>,
    context: &LocalContext,
    builder: FlatBufferBuilder<'b>,
) -> impl Future<Item = FlatBufferBuilder<'b>, Error = ErrorCode> {
    let response: Box<
        Future<
                Item = (
                    FlatBufferBuilder<'b>,
                    ResponseType,
                    WIPOffset<UnionWIPOffset>,
                ),
                Error = ErrorCode,
            > + Send,
    >;

    match request.request_type() {
        RequestType::FilesystemCheckRequest => {
            response = Box::new(fsck(context, builder));
        }
        RequestType::FilesystemChecksumRequest => {
            response = Box::new(result(checksum_request(context, builder)));
        }
        RequestType::ReadRequest => {
            let read_request = request.request_as_read_request().unwrap();
            let file = FileRequestHandler::new(
                read_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(
                file.read(read_request.offset(), read_request.read_size()),
            ));
        }
        RequestType::HardlinkRequest => {
            let hardlink_request = request.request_as_hardlink_request().unwrap();
            let file = FileRequestHandler::new(
                hardlink_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(file.hardlink(&hardlink_request.new_path())));
        }
        RequestType::RenameRequest => {
            let rename_request = request.request_as_rename_request().unwrap();
            let file = FileRequestHandler::new(
                rename_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(file.rename(&rename_request.new_path())));
        }
        RequestType::ChmodRequest => {
            let chmod_request = request.request_as_chmod_request().unwrap();
            let file = FileRequestHandler::new(
                chmod_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(file.chmod(chmod_request.mode())));
        }
        RequestType::TruncateRequest => {
            let truncate_request = request.request_as_truncate_request().unwrap();
            let file = FileRequestHandler::new(
                truncate_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(file.truncate(truncate_request.new_length())));
        }
        RequestType::UnlinkRequest => {
            let unlink_request = request.request_as_unlink_request().unwrap();
            let file = FileRequestHandler::new(
                unlink_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(file.unlink()));
        }
        RequestType::WriteRequest => {
            let write_request = request.request_as_write_request().unwrap();
            let file = FileRequestHandler::new(
                write_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(
                file.write(write_request.offset(), write_request.data()),
            ));
        }
        RequestType::UtimensRequest => {
            let utimens_request = request.request_as_utimens_request().unwrap();
            let file = FileRequestHandler::new(
                utimens_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(file.utimens(
                utimens_request.atime().map(Timestamp::seconds).unwrap_or(0),
                utimens_request.atime().map(Timestamp::nanos).unwrap_or(0),
                utimens_request.mtime().map(Timestamp::seconds).unwrap_or(0),
                utimens_request.mtime().map(Timestamp::nanos).unwrap_or(0),
            )));
        }
        RequestType::ReaddirRequest => {
            let readdir_request = request.request_as_readdir_request().unwrap();
            let file = FileRequestHandler::new(
                readdir_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(file.readdir()));
        }
        RequestType::GetattrRequest => {
            let getattr_request = request.request_as_getattr_request().unwrap();
            let file = FileRequestHandler::new(
                getattr_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(file.getattr()));
        }
        RequestType::MkdirRequest => {
            let mkdir_request = request.request_as_mkdir_request().unwrap();
            let file = FileRequestHandler::new(
                mkdir_request.path().to_string(),
                context.data_dir.clone(),
                builder,
            );
            response = Box::new(result(file.mkdir(mkdir_request.mode()).map(|builder| {
                let file = FileRequestHandler::new(
                    mkdir_request.path().to_string(),
                    context.data_dir.clone(),
                    builder,
                );
                // TODO: probably possible to hit a distributed race here
                file.getattr()
                    .expect("getattr failed on newly created file")
            })));
        }
        RequestType::RaftRequest => unreachable!(),
        RequestType::LatestCommitRequest => unreachable!(),
        RequestType::GetLeaderRequest => unreachable!(),
        RequestType::NONE => unreachable!(),
    }

    response.map(|(mut builder, response_type, response_offset)| {
        let mut generic_response_builder = GenericResponseBuilder::new(&mut builder);
        generic_response_builder.add_response_type(response_type);
        generic_response_builder.add_response(response_offset);

        let final_response_offset = generic_response_builder.finish();
        builder.finish_size_prefixed(final_response_offset, None);

        builder
    })
}

#[derive(Clone)]
pub struct LocalContext {
    pub data_dir: String,
    pub peers: Vec<SocketAddr>,
    pub node_id: u64,
}

impl LocalContext {
    pub fn new(data_dir: &str, peers: Vec<SocketAddr>, node_id: u64) -> LocalContext {
        LocalContext {
            data_dir: data_dir.to_string(),
            peers,
            node_id,
        }
    }
}

pub struct Node {
    context: LocalContext,
    raft_manager: RaftManager<'static>,
    port: u16,
}

impl Node {
    pub fn new(node_dir: &str, port: u16, peers: Vec<SocketAddr>) -> Node {
        let data_dir = Path::new(node_dir).join("data");
        // TODO huge hack. Should be generated randomly and then dynamically discovered
        // Unique ID of node within the cluster. Never 0.
        let node_id = u64::from(port);
        let context = LocalContext::new(data_dir.to_str().unwrap(), peers, node_id);
        Node {
            context: context.clone(),
            raft_manager: RaftManager::new(context.clone()),
            port,
        }
    }

    pub fn run(self) {
        if let Err(why) = fs::create_dir_all(&self.context.data_dir) {
            panic!("Couldn't create storage dir: {}", why.description());
        };

        let address = ([127, 0, 0, 1], self.port).into();
        let listener = TcpListener::bind(&address).expect("unable to bind API listener");

        let context = self.context.clone();
        let raft_manager = Arc::new(self.raft_manager);
        let raft_manager_cloned = raft_manager.clone();
        let server = listener
            .incoming()
            .map_err(|e| eprintln!("accept connection failed = {:?}", e))
            .for_each(move |socket| {
                let (reader, writer) = socket.split();
                let reader = length_delimited::Builder::new()
                    .little_endian()
                    .new_read(reader);

                let cloned = context.clone();
                let cloned_raft = raft_manager.clone();
                let builder = FlatBufferBuilder::new();
                let conn = reader.fold((writer, builder), move |(writer, mut builder), frame| {
                    let request = get_root_as_generic_request(&frame);
                    builder.reset();
                    let builder_future: Box<
                        Future<Item = FlatBufferBuilder, Error = ErrorCode> + Send,
                    >;
                    if is_raft_request(request.request_type()) {
                        builder_future =
                            Box::new(raft_message_handler(request, &cloned_raft, builder));
                    } else if is_write_request(request.request_type()) {
                        builder_future = Box::new(
                            cloned_raft
                                .propose(request, builder)
                                .map_err(|_| ErrorCode::Uncategorized),
                        );
                    } else {
                        // Sync to ensure replicas serve latest data
                        let cloned_raft2 = cloned_raft.clone();
                        let cloned2 = cloned.clone();
                        let after_sync = cloned_raft
                            .get_latest_commit_from_leader()
                            .map(move |latest_commit| cloned_raft2.sync(latest_commit))
                            .flatten()
                            .map_err(|_| ErrorCode::Uncategorized);
                        let read_after_sync = after_sync
                            .map(move |_| {
                                let request = get_root_as_generic_request(&frame);
                                handler(request, &cloned2, builder)
                            })
                            .flatten();
                        builder_future = Box::new(read_after_sync);
                    }
                    let builder_future2: Box<
                        Future<Item = FlatBufferBuilder, Error = std::io::Error> + Send,
                    >;
                    builder_future2 = Box::new(builder_future.or_else(|error_code| {
                        let mut builder = FlatBufferBuilder::new();
                        let args = ErrorResponseArgs { error_code };
                        let response_offset =
                            ErrorResponse::create(&mut builder, &args).as_union_value();
                        let mut generic_response_builder =
                            GenericResponseBuilder::new(&mut builder);
                        generic_response_builder.add_response_type(ResponseType::ErrorResponse);
                        generic_response_builder.add_response(response_offset);

                        let final_response_offset = generic_response_builder.finish();
                        builder.finish_size_prefixed(final_response_offset, None);

                        Ok(builder)
                    }));
                    builder_future2
                        .map(|builder| {
                            let writable = WritableFlatBuffer::new(builder);
                            tokio::io::write_all(writer, writable)
                        })
                        .flatten()
                        .map(|(writer, written)| (writer, written.into_buffer()))
                });

                tokio::spawn(conn.map(|_| ()).map_err(|_| ()))
            });

        let background_raft = Interval::new(Instant::now(), Duration::from_millis(100))
            .for_each(move |_| {
                raft_manager_cloned.background_tick();
                Ok(())
            })
            .map_err(|e| panic!("Background Raft thread failed error: {:?}", e));

        // TODO: currently we run single threaded to uncover deadlocks more easily
        let mut runtime = tokio::runtime::Builder::new()
            .core_threads(1)
            .build()
            .unwrap();
        runtime.spawn(server);
        runtime.block_on_all(background_raft.map(|_| ())).unwrap();
    }
}
