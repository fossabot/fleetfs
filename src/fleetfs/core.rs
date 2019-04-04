use std::error::Error;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;

use bytes::Buf;
use futures::future;
use futures::Stream;
use hyper::{Body, Response, Server, StatusCode};
use hyper::Method;
use hyper::Request;
use hyper::rt::Future;
use hyper::service::service_fn;
use crate::fleetfs::client::PeerClient;

pub const PATH_HEADER: &str = "X-FleetFS-Path";
pub const NO_FORWARD_HEADER: &str = "X-FleetFS-No-Forward";


pub type BoxFuture = Box<Future<Item=Response<Body>, Error=hyper::Error> + Send>;

struct DistributedFile {
    filename: String,
    local_data_dir: String,
    peers: Vec<PeerClient>
}

impl DistributedFile {
    fn new(filename: String, local_data_dir: String, peers: &[String]) -> DistributedFile {
        DistributedFile {
            filename,
            local_data_dir,
            peers: peers.into_iter().map(|peer| PeerClient::new(peer)).collect()
        }
    }

    fn truncate(self, req: Request<Body>) -> BoxFuture {
        let response = req.into_body()
            .concat2()
            .map(move |_| {
                let path = Path::new(&self.local_data_dir).join(self.filename);
                let display = path.display();

                match File::create(&path) {
                    Err(why) => panic!("couldn't create {}: {}",
                                       display,
                                       why.description()),
                    Ok(file) => file,
                };

                Response::new(Body::from("done"))
            });

        Box::new(response)
    }

    fn write(self, req: Request<Body>) -> BoxFuture {
        let offset: u64 = req.uri().path()[1..].parse().unwrap();
        let forward: bool = req.headers().get(NO_FORWARD_HEADER).is_none();
        let response = req.into_body()
            .concat2()
            .map(move |chunk| {
                let bytes = chunk.bytes();
                let path = Path::new(&self.local_data_dir).join(&self.filename);
                let display = path.display();

                let mut file = match OpenOptions::new()
                    .write(true)
                    .create(true)
                    .open(&path) {
                    Err(why) => panic!("couldn't create {}: {}",
                                       display,
                                       why.description()),
                    Ok(file) => file,
                };

                match file.seek(SeekFrom::Start(offset)) {
                    Err(why) => {
                        panic!("couldn't seek to {} in {} because {}", offset, display,
                               why.description())
                    },
                    Ok(_) => {}
                }

                match file.write_all(bytes) {
                    Err(why) => {
                        panic!("couldn't write to {}: {}", display,
                               why.description())
                    },
                    Ok(_) => println!("successfully wrote to {}", display),
                }

                if forward {
                    for peer in self.peers {
                        peer.write(format!("/{}", &self.filename), offset, bytes);
                    }
                }

                Response::new(Body::from("done"))
            });

        Box::new(response)
    }

    fn list_dir(self, req: Request<Body>) -> BoxFuture {
        println!("Listing directory");
        let response = req.into_body()
            .concat2()
            .map(move |_| {
                let mut entries = vec![];
                for entry in fs::read_dir(Path::new(&self.local_data_dir).join(self.filename)).unwrap() {
                    let filename = entry.unwrap().path().clone().to_str().unwrap().to_string();
                    // TODO: there must be a better way to strip the data_dir substring off the left side
                    entries.push(filename.split_at(self.local_data_dir.len() + 1).1.to_string());
                }

                Response::new(Body::from(serde_json::to_string(&entries).unwrap()))
            });

        Box::new(response)
    }

    fn read(self, req: Request<Body>) -> BoxFuture {
        if self.filename.len() == 0 {
            return self.list_dir(req);
        }

        println!("Reading file");
        let response = req.into_body()
            .concat2()
            .map(move |_| {
                let contents = fs::read_to_string(Path::new(&self.local_data_dir).join(self.filename))
                    .expect("Something went wrong reading the file");

                Response::new(Body::from(contents))
            });

        Box::new(response)
    }

    fn unlink(self, req: Request<Body>) -> BoxFuture {
        let forward: bool = req.headers().get(NO_FORWARD_HEADER).is_none();
        assert_ne!(self.filename.len(), 0);

        println!("Deleting file");
        let path = Path::new(&self.local_data_dir).join(&self.filename);
        let response = req.into_body()
            .concat2()
            .map(move |_| {
                fs::remove_file(path)
                    .expect("Something went wrong reading the file");

                Response::new(Body::from("success"))
            });

        let mut result: BoxFuture = Box::new(response);
        if forward {
            for peer in self.peers {
                let peer_request = peer.unlink(format!("{}", &self.filename));
                result = Box::new(result.join(peer_request).map(|(r, _)| r));
            }
        }

        return result;
    }

}

fn handler(req: Request<Body>, data_dir: String, peers: &[String]) -> BoxFuture {
    let mut response = Response::new(Body::empty());

    let filename: String = req.headers()[PATH_HEADER].to_str().unwrap().trim_left_matches('/').to_string();

    let file = DistributedFile::new(filename, data_dir, peers);

    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => {
            return file.read(req);
        },
        (&Method::DELETE, "/") => {
            return file.unlink(req);
        },
        (&Method::POST, "/truncate") => {
            return file.truncate(req);
        },
        (&Method::POST, _) => {
            return file.write(req);
        },
        _ => {
            *response.status_mut() = StatusCode::NOT_FOUND;
        },
    };

    Box::new(future::ok(response))
}

pub struct Node {
    data_dir: String,
    port: u16,
    peers: Vec<String>
}

impl Node {
    pub fn new(data_dir: String, port: u16, peers: &[String]) -> Node {
        Node {
            data_dir,
            port,
            peers: Vec::from(peers)
        }
    }

    pub fn run(self) {
        match fs::create_dir_all(&self.data_dir) {
            Err(why) => panic!("Couldn't create storage dir: {}", why.description()),
            Ok(_) => ()

        };

        let addr = ([127, 0, 0, 1], self.port).into();

        let new_service = move || {
            let data_dir2 = self.data_dir.clone();
            let peers_copy = self.peers.clone();
            service_fn(move |req| handler(req, data_dir2.clone(), &peers_copy))
        };

        let server = Server::bind(&addr)
            .serve(new_service)
            .map_err(|e| eprintln!("server error: {}", e));

        hyper::rt::run(server);
    }
}
