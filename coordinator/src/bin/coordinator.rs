use std::env::var;
use std::net::ToSocketAddrs;
use std::time::Duration;

use coordinator::faucet::Faucet;
use coordinator::shared_state::SharedState;
use coordinator::structs::*;
use coordinator::utils::*;

use env_logger::Env;

use ethers_core::types::{Address, U64};

use tokio::task::spawn;
use tokio::time::sleep;

use hyper::body::HttpBody;
use hyper::client::HttpConnector;
use hyper::header::HeaderValue;
use hyper::service::{make_service_fn, service_fn};
use hyper::HeaderMap;
use hyper::{Body, Method, Request, Response, Server, StatusCode, Uri};

const EVENT_LOOP_COOLDOWN: Duration = Duration::from_millis(3000);
/// allowed jsonrpc methods
const PROXY_ALLOWED_METHODS: [&str; 40] = [
    "eth_chainId",
    "eth_gasPrice",
    "eth_blockNumber",
    "eth_estimateGas",
    "eth_call",
    "eth_getCode",
    "eth_createAccessList",
    "eth_feeHistory",
    "eth_getLogs",
    "eth_getBalance",
    "eth_getStorageAt",
    "eth_getTransactionCount",
    "eth_sendRawTransaction",
    "eth_getTransactionReceipt",
    "eth_getTransactionByHash",
    "net_version",
    "web3_clientVersion",
    "eth_getHeaderByNumber",
    "eth_getHeaderByHash",
    "eth_getBlockByNumber",
    "eth_getBlockByHash",
    "eth_getTransactionByBlockHashAndIndex",
    "eth_getTransactionByBlockNumberAndIndex",
    "eth_getBlockTransactionCountByHash",
    "eth_getBlockTransactionCountByNumber",
    "eth_getRawTransactionByHash",
    "eth_getProof",
    "debug_accountRange",
    "debug_getHeaderRlp",
    "debug_getBlockRlp",
    "debug_dumpBlock",
    "debug_traceBlock",
    "debug_intermediateRoots",
    "debug_traceBlockByNumber",
    "debug_traceBlockByHash",
    "debug_traceTransaction",
    "debug_traceCall",
    "debug_storageRangeAt",
    "debug_getModifiedAccountsByNumber",
    "debug_getModifiedAccountsByHash",
];

fn set_headers(headers: &mut HeaderMap, extended: bool) {
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));

    if extended {
        headers.insert(
            "access-control-allow-methods",
            HeaderValue::from_static("post, get, options"),
        );
        headers.insert(
            "access-control-allow-headers",
            HeaderValue::from_static("origin, content-type, accept, x-requested-with"),
        );
        headers.insert("access-control-max-age", HeaderValue::from_static("300"));
    }
}

async fn handle_request(
    shared_state: SharedState,
    faucet: Option<Faucet>,
    client: hyper::Client<HttpConnector>,
    req: Request<Body>,
) -> Result<Response<Body>, hyper::Error> {
    // TODO: support deflate content encoding

    #[derive(serde::Deserialize, serde::Serialize)]
    struct ProxyRequest {
        id: serde_json::Value,
        method: String,
    }

    {
        // limits the request size
        const MAX_BODY_SIZE: u64 = 4 << 20;
        let response_content_length = match req.body().size_hint().upper() {
            Some(v) => v,
            None => MAX_BODY_SIZE + 1,
        };

        if response_content_length > MAX_BODY_SIZE {
            let mut resp = Response::new(Body::from("request too large"));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    }

    match (req.method(), req.uri().path()) {
        // serve some information about the chain
        (&Method::GET, "/") => {
            let mut resp = Response::new(Body::from(
                serde_json::to_vec(&shared_state.rw.lock().await.chain_state).unwrap(),
            ));
            set_headers(resp.headers_mut(), false);
            Ok(resp)
        }

        // json-rpc
        (&Method::POST, "/") => {
            let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let obj: ProxyRequest =
                serde_json::from_slice(body_bytes.as_ref()).expect("ProxyRequest");

            // only allow allow the following methods and nothing else
            if !PROXY_ALLOWED_METHODS.iter().any(|e| **e == obj.method) {
                let err = JsonRpcResponseError {
                    jsonrpc: "2.0".to_string(),
                    id: obj.id,
                    error: JsonRpcError {
                        code: -32601,
                        message: "this method is not available".to_string(),
                    },
                };
                let resp = Response::new(Body::from(serde_json::to_vec(&err).unwrap()));
                return Ok(resp);
            }

            let mut resp;
            {
                // choose a serving node or none
                let r = rand::random::<usize>();
                let ctx = shared_state.rw.lock().await;
                let len = ctx.nodes.len();
                if len == 0 {
                    drop(ctx);
                    resp = Response::default();
                    *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE
                } else {
                    let node_req = Request::post(&ctx.nodes[r % len]);
                    drop(ctx);
                    // reusing the same request doesn't work correctly.
                    // Feeding the body via a reader() which was already consumed doesn't work either :/
                    let node_req = node_req
                        .header(hyper::header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body_bytes))
                        .unwrap();
                    resp = client.request(node_req).await.unwrap();
                }
            }

            set_headers(resp.headers_mut(), false);
            Ok(resp)
        }

        // serve CORS headers
        (&Method::OPTIONS, "/") => {
            let mut resp = Response::default();
            set_headers(resp.headers_mut(), true);
            Ok(resp)
        }

        // returns 503 if faucet is disabled else 200 and enqueues a faucet requests
        // that is processed asyncly.
        // The faucet transfer can still fail if the `l1_wallet` has not enough ETH.
        (&Method::GET, "/faucet") => {
            let receiver = req
                .uri()
                .query()
                .expect("uri query")
                .parse::<Address>()
                .expect("receiver address");
            let mut resp = Response::default();

            match faucet {
                None => {
                    *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
                }
                Some(faucet) => {
                    faucet.queue.lock().await.push_back(receiver);
                    *resp.status_mut() = StatusCode::OK;
                }
            }

            set_headers(resp.headers_mut(), false);
            Ok(resp)
        }

        // everything else
        _ => {
            let mut not_found = Response::default();
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }
    }
}

async fn check_nodes(ctx: SharedState, client: hyper::Client<HttpConnector>) {
    let head_hash = ctx.rw.lock().await.chain_state.head_block_hash;
    // discover & update nodes
    let addrs_iter = var("RPC_SERVER_NODES")
        .expect("RPC_SERVER_NODES env var")
        .to_socket_addrs()
        .unwrap();
    let mut nodes = Vec::new();

    let mut fallback_node_uri = None;
    let mut fallback_node_num = U64::zero();
    for addr in addrs_iter {
        let uri = Uri::try_from(format!("http://{}", addr)).unwrap();
        let header = get_chain_head(&client, &uri).await;

        // use the most advanced node as fallback
        if header.number > fallback_node_num {
            fallback_node_num = header.number;
            fallback_node_uri = Some(uri.clone());
        }

        if header.hash != head_hash {
            log::warn!("skipping inconsistent node: {}", uri);
            continue;
        }

        nodes.push(uri);
    }

    // update nodes
    let mut rw = ctx.rw.lock().await;
    if rw.nodes.len() != nodes.len() {
        log::info!("found {} ready rpc nodes", nodes.len());
    }
    if nodes.is_empty() && fallback_node_uri.is_some() {
        let uri = fallback_node_uri.unwrap();
        log::info!("using {} as fallback node", uri);
        rw.nodes = vec![uri];
    } else {
        rw.nodes = nodes;
    }
}

async fn event_loop(ctx: SharedState, _client: hyper::Client<HttpConnector>) {
    // TODO: split sync,mine into own task

    ctx.sync().await;
    ctx.mine().await;
    ctx.submit_blocks().await;
    ctx.finalize_blocks().await;
    ctx.relay_to_l1().await;
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let shared_state = SharedState::from_env().await;
    shared_state.init().await;

    let faucet: Option<Faucet> = coordinator::option_enabled!("ENABLE_FAUCET", Faucet::default());
    log::info!("faucet enabled: {}", faucet.is_some());

    {
        let addr = var("LISTEN")
            .expect("LISTEN env var")
            .parse::<std::net::SocketAddr>()
            .expect("valid socket address");
        let client = hyper::Client::new();
        let shared_state = shared_state.clone();
        let faucet = faucet.clone();
        // start the http server
        spawn(async move {
            let service = make_service_fn(move |_| {
                let shared_state = shared_state.clone();
                let faucet = faucet.clone();
                let client = client.clone();
                let service = service_fn(move |req| {
                    handle_request(shared_state.clone(), faucet.clone(), client.to_owned(), req)
                });

                async move { Ok::<_, hyper::Error>(service) }
            });
            let server = Server::bind(&addr).serve(service);
            log::info!("Listening on http://{}", addr);
            server.await.expect("server should be serving");
            // terminate process?
        });
    }

    {
        let ctx = shared_state.clone();
        let h1 = spawn(async move {
            let client = hyper::Client::new();
            loop {
                log::debug!("spawning event_loop task");

                {
                    // the event_loop handles l1/l2 chain events and chain progression
                    let res = spawn(event_loop(ctx.clone(), client.to_owned())).await;

                    if let Err(err) = res {
                        log::error!("event_loop: {}", err);
                    }
                }

                {
                    // The faucet shares the same l1 wallet with the event_loop
                    // above, therefore it should be invoked in serial.
                    let ctx = ctx.clone();
                    let faucet = faucet.clone();
                    let res = spawn(async move {
                        if let Some(_faucet) = &faucet {
                            // only consume up to 3 items each time
                            _faucet.drain(ctx, 3).await;
                        }
                    })
                    .await;

                    if let Err(err) = res {
                        log::error!("faucet: {}", err);
                    }
                }

                sleep(EVENT_LOOP_COOLDOWN).await;
            }
        });

        let ctx = shared_state.clone();
        let h2 = spawn(async move {
            let client = hyper::Client::new();
            loop {
                log::debug!("spawning check_nodes task");
                let res = spawn(check_nodes(ctx.clone(), client.to_owned())).await;

                if let Err(err) = res {
                    log::error!("task: {}", err);
                }

                sleep(Duration::from_millis(100)).await;
            }
        });

        // wait for all tasks
        if tokio::try_join!(h1, h2).is_err() {
            panic!("unexpected task error");
        }
    }
}
