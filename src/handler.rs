use crate::config::*;
use crate::error::RocketLambError;
use crate::request_ext::RequestExt as _;
use aws_lambda_events::encodings::Body;
use lamedh_http::{Handler, Request, RequestExt, Response};
use lamedh_runtime::Context;
use rocket::http::{uri::Uri, Header};
use rocket::local::blocking::{Client, LocalRequest, LocalResponse};
use rocket::{Rocket, Route};
use std::future::Future;
use std::mem;
use std::pin::Pin;

/// A Lambda handler for API Gateway events that processes requests using a [Rocket](rocket::Rocket) instance.
pub struct RocketHandler {
    pub(super) client: LazyClient,
    pub(super) config: Config,
}

pub(super) enum LazyClient {
    Placeholder,
    Uninitialized(Rocket),
    Ready(Client),
}

impl Handler for RocketHandler {
    type Error = failure::Error;
    type Response = Response<Body>;
    type Fut = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + 'static>>;

    fn call(&mut self, req: Request, _ctx: Context) -> Self::Fut {
        self.ensure_client_ready(&req);
        let fut = async {
            process_request(self, req)
                .await
                .map_err(failure::Error::from)
                .map_err(failure::Error::into)
        };
        Box::pin(fut)
    }
}

impl RocketHandler {
    fn ensure_client_ready(&mut self, req: &Request) {
        match self.client {
            ref mut lazy_client @ LazyClient::Uninitialized(_) => {
                let uninitialized_client = mem::replace(lazy_client, LazyClient::Placeholder);
                let mut rocket = match uninitialized_client {
                    LazyClient::Uninitialized(rocket) => rocket,
                    _ => unreachable!("LazyClient must be uninitialized at this point."),
                };
                if self.config.base_path_behaviour == BasePathBehaviour::RemountAndInclude {
                    let base_path = req.base_path();
                    if !base_path.is_empty() {
                        let routes: Vec<Route> = rocket.routes().cloned().collect();
                        rocket = rocket.mount(&base_path, routes);
                    }
                }
                let client = Client::untracked(rocket).unwrap();
                self.client = LazyClient::Ready(client);
            }
            LazyClient::Ready(_) => {}
            LazyClient::Placeholder => panic!("LazyClient has previously begun initialiation."),
        }
    }

    fn client(&self) -> &Client {
        match &self.client {
            LazyClient::Ready(client) => client,
            _ => panic!("Rocket client wasn't ready. ensure_client_ready should have been called!"),
        }
    }

    fn get_path_and_query(&self, req: &Request) -> String {
        let mut uri = match self.config.base_path_behaviour {
            BasePathBehaviour::Include | BasePathBehaviour::RemountAndInclude => req.full_path(),
            BasePathBehaviour::Exclude => req.api_path().to_owned(),
        };
        let query = req.query_string_parameters();

        let mut separator = '?';
        for (key, _) in query.iter() {
            for value in query.get_all(key).unwrap() {
                uri.push_str(&format!(
                    "{}{}={}",
                    separator,
                    Uri::percent_encode(key),
                    Uri::percent_encode(value)
                ));
                separator = '&';
            }
        }
        uri
    }
}

async fn process_request(
    handler: &RocketHandler,
    req: Request,
) -> Result<Response<Body>, RocketLambError> {
    let local_req = create_rocket_request(handler, req)?;
    let local_res = local_req.dispatch();
    create_lambda_response(handler, local_res).await
}

fn create_rocket_request(
    handler: &RocketHandler,
    req: Request,
) -> Result<LocalRequest, RocketLambError> {
    let method = to_rocket_method(req.method())?;
    let uri = handler.get_path_and_query(&req);
    let mut local_req = handler.client().req(method, uri);
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
    handler: &RocketHandler,
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
        .and_then(|ct| handler.config.response_types.get(&ct.to_lowercase()))
        .copied()
        .unwrap_or(handler.config.default_response_type);
    let body = match local_res.into_bytes() {
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
