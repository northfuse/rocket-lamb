use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aws_lambda_events::encodings::Body;
use lamedh_http::{Handler, Request, RequestExt, Response};
use lamedh_runtime::Context;
use rocket::http::Header;
use rocket::http::RawStr;
use rocket::local::asynchronous::{Client, LocalRequest, LocalResponse};
use rocket::{Build, Rocket, Route};
use tokio::sync::Mutex;

use crate::config::*;
use crate::error::RocketLambError;
use crate::request_ext::RequestExt as _;

/// A Lambda handler for API Gateway events that processes requests using a [Rocket](rocket::Rocket) instance.
pub struct RocketHandler {
    pub(super) lazy_client: Arc<Mutex<LazyClient>>,
    pub(super) config: Arc<Config>,
}

pub(super) enum LazyClient {
    Uninitialized(Option<Rocket<Build>>),
    Ready(Arc<Client>),
}

type HandlerError = failure::Error;
type HandlerResponse = Response<Body>;
type HandlerResult = Result<HandlerResponse, HandlerError>;

impl Handler for RocketHandler {
    type Error = HandlerError;
    type Response = HandlerResponse;

    type Fut = Pin<Box<dyn Future<Output = HandlerResult> + 'static>>;

    fn call(&mut self, req: Request, _ctx: Context) -> Self::Fut {
        let config = Arc::clone(&self.config);
        let lazy_client = Arc::clone(&self.lazy_client);
        let fut = async {
            process_request(lazy_client, config, req)
                .await
                .map_err(failure::Error::from)
                .map_err(failure::Error::into)
        };
        Box::pin(fut)
    }
}

fn get_path_and_query(config: &Config, req: &Request) -> String {
    let mut uri = match &config.base_path_behaviour {
        BasePathBehaviour::Include | BasePathBehaviour::RemountAndInclude => dbg!(req.full_path()),
        BasePathBehaviour::Exclude => dbg!(req.api_path().to_owned()),
    };
    let query = req.query_string_parameters();

    let mut separator = '?';
    for (key, _) in query.iter() {
        for value in query.get_all(key).unwrap() {
            uri.push_str(&format!(
                "{}{}={}",
                separator,
                RawStr::new(key).percent_encode(),
                RawStr::new(value).percent_encode()
            ));
            separator = '&';
        }
    }
    uri
}

async fn process_request(
    lazy_client: Arc<Mutex<LazyClient>>,
    config: Arc<Config>,
    req: Request,
) -> Result<Response<Body>, RocketLambError> {
    let client = get_client_from_lazy(&lazy_client, &config, &req).await;
    let local_req = create_rocket_request(&client, Arc::clone(&config), req)?;
    let local_res = local_req.dispatch().await;
    create_lambda_response(config, local_res).await
}

async fn get_client_from_lazy(
    lazy_client_lock: &Mutex<LazyClient>,
    config: &Config,
    req: &Request,
) -> Arc<Client> {
    let mut lazy_client = lazy_client_lock.lock().await;
    match &mut *lazy_client {
        LazyClient::Ready(c) => Arc::clone(&c),
        LazyClient::Uninitialized(r) => {
            let r = r
                .take()
                .expect("It should not be possible for this to be None");
            let base_path = req.base_path();
            let client = if config.base_path_behaviour == BasePathBehaviour::RemountAndInclude
                && !base_path.is_empty()
            {
                let routes: Vec<Route> = r.routes().cloned().collect();
                let rocket = r.mount(&base_path, routes);
                Client::untracked(rocket).await.unwrap()
            } else {
                Client::untracked(r).await.unwrap()
            };
            let client = Arc::new(client);
            let client_clone = Arc::clone(&client);
            *lazy_client = LazyClient::Ready(client);
            client_clone
        }
    }
}

fn create_rocket_request(
    client: &Client,
    config: Arc<Config>,
    req: Request,
) -> Result<LocalRequest, RocketLambError> {
    let method = to_rocket_method(req.method())?;
    let uri = get_path_and_query(&config, &req);
    let mut local_req = client.req(method, uri);
    for (name, value) in req.headers() {
        match value.to_str() {
            Ok(v) => local_req.add_header(Header::new(name.to_string(), v.to_string())),
            Err(_) => return Err(invalid_request!("invalid value for header '{}'", name)),
        }
    }
    local_req.set_body(req.into_body());
    Ok(local_req)
}

async fn create_lambda_response(
    config: Arc<Config>,
    local_res: LocalResponse<'_>,
) -> Result<Response<Body>, RocketLambError> {
    let mut builder = Response::builder();
    builder = builder.status(local_res.status().code);
    for h in local_res.headers().iter() {
        builder = builder.header(&h.name.to_string(), &h.value.to_string());
    }

    let response_type = local_res
        .headers()
        .get_one("content-type")
        .unwrap_or_default()
        .split(';')
        .next()
        .and_then(|ct| config.response_types.get(&ct.to_lowercase()))
        .copied()
        .unwrap_or(config.default_response_type);
    let body = match local_res.into_bytes().await {
        Some(b) => match response_type {
            ResponseType::Auto => match String::from_utf8(b) {
                Ok(s) => Body::Text(s),
                Err(e) => Body::Binary(e.into_bytes()),
            },
            ResponseType::Text => Body::Text(
                String::from_utf8(b)
                    .map_err(|_| invalid_response!("failed to read response body as UTF-8"))?,
            ),
            ResponseType::Binary => Body::Binary(b),
        },
        None => Body::Empty,
    };

    builder.body(body).map_err(|e| invalid_response!("{}", e))
}

fn to_rocket_method(method: &http::Method) -> Result<rocket::http::Method, RocketLambError> {
    use http::Method as H;
    use rocket::http::Method::*;
    Ok(match *method {
        H::GET => Get,
        H::PUT => Put,
        H::POST => Post,
        H::DELETE => Delete,
        H::OPTIONS => Options,
        H::HEAD => Head,
        H::TRACE => Trace,
        H::CONNECT => Connect,
        H::PATCH => Patch,
        _ => return Err(invalid_request!("unknown method '{}'", method)),
    })
}
