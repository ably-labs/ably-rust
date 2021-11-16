use super::error::{ErrorInfo, WrappedError};
use super::Result;
use super::{auth, rest};
use regex::Regex;
pub use reqwest::header::{HeaderMap, HeaderValue};
pub use reqwest::Method;
use std::convert::TryFrom;
use std::marker::PhantomData;

use futures::future::FutureExt;
use futures::stream::{self, Stream, StreamExt};

use lazy_static::lazy_static;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// A low-level HTTP client for the [Ably REST API].
///
/// [Ably REST API]: https://ably.com/documentation/rest-api
#[derive(Clone, Debug)]
pub struct Client {
    inner:    reqwest::Client,
    rest_url: reqwest::Url,
}

impl Client {
    pub fn new(rest_url: reqwest::Url) -> Self {
        Self {
            inner: reqwest::Client::new(),
            rest_url,
        }
    }

    /// Start building a HTTP request to the Ably REST API.
    ///
    /// Returns a RequestBuilder which can be used to set query params, headers
    /// and the request body before sending the request.
    pub fn request(&self, method: Method, path: impl Into<String>) -> RequestBuilder {
        let mut url = self.rest_url.clone();
        url.set_path(&path.into());
        self.request_url(method, url)
    }

    pub fn paginated_request<T: PaginatedItem, U: PaginatedItemHandler<T>>(
        &self,
        method: Method,
        path: impl Into<String>,
        handler: Option<U>,
    ) -> PaginatedRequestBuilder<T, U> {
        PaginatedRequestBuilder::new(self.request(method, path), handler)
    }

    /// Start building a HTTP request to the given URL.
    ///
    /// Returns a RequestBuilder which can be used to set query params, headers
    /// and the request body before sending the request.
    pub fn request_url(&self, method: Method, url: impl reqwest::IntoUrl) -> RequestBuilder {
        RequestBuilder::new(self.inner.clone(), self.inner.request(method, url))
    }
}

/// A builder to construct a HTTP request to the [Ably REST API].
///
/// [Ably REST API]: https://ably.com/documentation/rest-api
pub struct RequestBuilder {
    client: reqwest::Client,
    inner:  Result<reqwest::RequestBuilder>,
    auth:   Option<auth::Auth>,
    format: rest::Format,
}

impl RequestBuilder {
    fn new(client: reqwest::Client, inner: reqwest::RequestBuilder) -> Self {
        Self {
            client,
            inner: Ok(inner),
            auth: None,
            format: rest::DEFAULT_FORMAT,
        }
    }

    /// Set the request format.
    pub fn format(mut self, format: rest::Format) -> Self {
        self.format = format;
        self
    }

    /// Modify the query params of the request, adding the parameters provided.
    pub fn params<T: Serialize + ?Sized>(mut self, params: &T) -> Self {
        if let Ok(req) = self.inner {
            self.inner = Ok(req.query(params));
        }
        self
    }

    /// Set the request body.
    pub fn body<T: Serialize + ?Sized>(self, body: &T) -> Self {
        match self.format {
            rest::Format::MessagePack => self.msgpack(body),
            rest::Format::JSON => self.json(body),
        }
    }

    /// Set the JSON request body.
    fn json<T: Serialize + ?Sized>(mut self, body: &T) -> Self {
        if let Ok(req) = self.inner {
            self.inner = Ok(req.json(body));
        }
        self
    }

    /// Set the MessagePack request body.
    fn msgpack<T: Serialize + ?Sized>(mut self, body: &T) -> Self {
        if let Ok(req) = self.inner {
            self.inner = rmp_serde::to_vec_named(body)
                .map(|data| {
                    req.header(
                        reqwest::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/x-msgpack"),
                    )
                    .body(data)
                })
                .map_err(Into::into)
        }
        self
    }

    /// Add a set of HTTP headers to the request.
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        if let Ok(req) = self.inner {
            self.inner = Ok(req.headers(headers));
        }
        self
    }

    pub fn auth(mut self, auth: auth::Auth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Send the request to the Ably REST API.
    pub async fn send(self) -> Result<Response> {
        self.build()?.send().await
    }

    fn build(self) -> Result<Request> {
        let mut req = self.inner?;

        req = req.header("X-Ably-Version", "1.2");

        // Set the Authorization header.
        if let Some(auth) = self.auth {
            match auth.credential {
                auth::Credential::Key(key) => {
                    req = req.basic_auth(&key.name, Some(&key.value));
                }
                auth::Credential::Token(token) => {
                    req = req.bearer_auth(&token);
                }
            }
        }

        // Build the request.
        let req = req.build()?;

        Ok(Request::new(self.client.clone(), req))
    }
}

/// Internal state used with [stream::unfold] to construct a pagination stream.
///
/// The state holds the request for the next page in the stream, and an
/// optional item handler which is passed to each PaginatedResult.
///
/// [stream::unfold]: https://docs.rs/futures/latest/futures/stream/fn.unfold.html
struct PaginatedState<T, U: PaginatedItemHandler<T>> {
    next_req: Option<Result<Request>>,
    handler:  Option<U>,
    phantom:  PhantomData<T>,
}

/// A builder to construct a paginated REST request.
pub struct PaginatedRequestBuilder<T: PaginatedItem, U: PaginatedItemHandler<T> = ()> {
    inner:   RequestBuilder,
    handler: Option<U>,
    phantom: PhantomData<T>,
}

impl<T: PaginatedItem, U: PaginatedItemHandler<T>> PaginatedRequestBuilder<T, U> {
    pub fn new(inner: RequestBuilder, handler: Option<U>) -> Self {
        Self {
            inner,
            handler,
            phantom: PhantomData,
        }
    }

    pub fn start(self, interval: &str) -> Self {
        self.params(&[("start", interval)])
    }

    pub fn end(self, interval: &str) -> Self {
        self.params(&[("end", interval)])
    }

    pub fn forwards(self) -> Self {
        self.params(&[("direction", "forwards")])
    }

    pub fn backwards(self) -> Self {
        self.params(&[("direction", "backwards")])
    }

    pub fn limit(self, limit: u32) -> Self {
        self.params(&[("limit", limit.to_string())])
    }

    /// Modify the query params of the request, adding the parameters provided.
    pub fn params<P: Serialize + ?Sized>(mut self, params: &P) -> Self {
        self.inner = self.inner.params(params);
        self
    }

    /// Request a stream of pages from the Ably REST API.
    pub fn pages(self) -> impl Stream<Item = Result<PaginatedResult<T, U>>> {
        // Use stream::unfold to create a stream of pages where the internal
        // state holds the request for the next page, and the closure sends the
        // request and returns both a PaginatedResult and the request for the
        // next page if the response has a 'Link: ...; rel="next"' header.
        let seed_state = PaginatedState {
            next_req: Some(self.inner.build()),
            handler:  self.handler,
            phantom:  PhantomData,
        };
        stream::unfold(seed_state, |mut state| {
            async {
                // If there is no request in the state, we're done, so unwrap
                // the request to a Result<Request>.
                let req = state.next_req?;

                // If there was an error constructing the next request, yield
                // that error and set the next request to None to end the
                // stream on the next iteration.
                let req = match req {
                    Err(err) => {
                        state.next_req = None;
                        return Some((Err(err), state));
                    }
                    Ok(req) => req,
                };

                // Clone the request first so we can maintain the same headers
                // for the next request before we consume the current request
                // by sending it.
                //
                // If the request is not cloneable, for example because it has
                // a streamed body, map it to an error which will be yielded on
                // the next iteration of the stream.
                let mut next_req = req
                    .try_clone()
                    .ok_or(error!(40000, "not a pageable request"));

                // Send the request and wrap the response in a PaginatedResult.
                //
                // If there's an error, yield the error and set the next
                // request to None to end the stream on the next iteration.
                let res = match req.send().await {
                    Err(err) => {
                        state.next_req = None;
                        return Some((Err(err), state));
                    }
                    Ok(res) => PaginatedResult::new(res, state.handler.clone()),
                };

                // If there's a next link in the response, merge its params
                // into the next request if we have one, otherwise set the next
                // request to None to end the stream on the next iteration.
                state.next_req = None;
                if let Some(link) = res.next_link() {
                    if let Ok(req) = &mut next_req {
                        req.url_mut().set_query(Some(&link.params));
                    }
                    state.next_req = Some(next_req)
                };

                // Yield the PaginatedResult and the next state.
                Some((Ok(res), state))
            }
            .boxed()
        })
    }

    /// Retrieve the first page of the paginated response.
    pub async fn send(self) -> Result<PaginatedResult<T, U>> {
        // The pages stream always returns at least one non-None value, even if
        // the first request returns an error which would be Some(Err(err)), so
        // we unwrap the Option with a generic error which we don't expect to
        // be encountered by the caller.
        self.pages()
            .next()
            .await
            .unwrap_or(Err(error!(40000, "Unexpected error retrieving first page")))
    }
}

pub struct Request {
    client: reqwest::Client,
    inner:  reqwest::Request,
}

impl Request {
    fn new(client: reqwest::Client, req: reqwest::Request) -> Self {
        Self { client, inner: req }
    }

    fn url_mut(&mut self) -> &mut reqwest::Url {
        self.inner.url_mut()
    }

    async fn send(self) -> Result<Response> {
        let res = self.client.execute(self.inner).await?;

        // Return the response if it was successful, otherwise try to decode a
        // JSON error from the response body, falling back to a generic error
        // if decoding fails.
        if res.status().is_success() {
            return Ok(Response::new(res));
        }

        let status_code: u32 = res.status().as_u16().into();
        Err(res
            .json::<WrappedError>()
            .await
            .map(|e| e.error)
            .unwrap_or_else(|err| {
                error!(
                    50000,
                    format!("Unexpected error: {}", err),
                    Some(status_code)
                )
            }))
    }

    fn try_clone(&self) -> Option<Self> {
        self.inner
            .try_clone()
            .map(|req| Self::new(self.client.clone(), req))
    }
}

/// A Link HTTP header.
struct Link {
    rel:    String,
    params: String,
}

lazy_static! {
    /// A static regular expression to extract the rel and params fields
    /// from a Link header, which looks something like:
    ///
    /// Link: <./messages?limit=10&direction=forwards&cont=true&format=json&firstStart=0&end=1635552598723>; rel="next"
    static ref LINK_RE: Regex = Regex::new(r#"^\s*<[^?]+\?(?P<params>.+)>;\s*rel="(?P<rel>\w+)"$"#).unwrap();
}

impl TryFrom<&reqwest::header::HeaderValue> for Link {
    type Error = ErrorInfo;

    /// Try and extract a Link object from a Link HTTP header.
    fn try_from(v: &reqwest::header::HeaderValue) -> Result<Link> {
        // Check we have a valid utf-8 string.
        let link = v
            .to_str()
            .map_err(|_| error!(40004, "Invalid Link header"))?;

        // Extract the rel and params from the header using the LINK_RE regular
        // expression.
        let caps = LINK_RE
            .captures(link)
            .ok_or(error!(40004, "Invalid Link header"))?;
        let rel = caps
            .name("rel")
            .ok_or(error!(40004, "Invalid Link header; missing rel"))?;
        let params = caps
            .name("params")
            .ok_or(error!(40004, "Invalid Link header; missing params"))?;

        Ok(Self {
            rel:    rel.as_str().to_string(),
            params: params.as_str().to_string(),
        })
    }
}

/// A successful Response from the [Ably REST API].
///
/// [Ably REST API]: https://ably.com/documentation/rest-api
#[derive(Debug)]
pub struct Response {
    inner: reqwest::Response,
}

impl Response {
    fn new(response: reqwest::Response) -> Self {
        Self { inner: response }
    }

    /// Returns the response Content-Type.
    pub fn content_type(&self) -> Option<mime::Mime> {
        self.inner
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|v| v.to_str().ok())
            .flatten()
            .map(|v| v.parse().ok())
            .flatten()
    }

    /// Deserialize the response body.
    pub async fn body<T: DeserializeOwned>(self) -> Result<T> {
        let content_type = self
            .content_type()
            .ok_or(error!(40001, "missing content-type"))?;

        match content_type.essence_str() {
            "application/json" => self.json().await,
            "application/x-msgpack" => self.msgpack().await,
            _ => Err(error!(
                40001,
                format!("invalid response content-type: {}", content_type)
            )),
        }
    }

    /// Deserialize the response body as JSON.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
        self.inner.json().await.map_err(Into::into)
    }

    /// Deserialize the response body as MessagePack.
    pub async fn msgpack<T: DeserializeOwned>(self) -> Result<T> {
        let data = self.inner.bytes().await?;

        rmp_serde::from_read(&*data).map_err(Into::into)
    }

    /// Return the response body as a String.
    pub async fn text(self) -> Result<String> {
        self.inner.text().await.map_err(Into::into)
    }

    /// Returns the HTTP status code.
    pub fn status_code(&self) -> reqwest::StatusCode {
        self.inner.status()
    }
}

/// A handler for items in a paginated response, typically used to decode
/// history messages before returning them to the caller.
pub trait PaginatedItemHandler<T>: Send + Clone + 'static {
    fn handle(&self, item: &mut T) -> ();
}

/// Provide a no-op implementation of PaginatedItemHandler for the unit type
/// which is used as the default type for paginated responses which don't
/// require a handler (e.g. paginated stats responses).
impl<T> PaginatedItemHandler<T> for () {
    fn handle(&self, _: &mut T) -> () {}
}

/// An item in a paginated response.
///
/// An item can be any type which can be deserialized and sent between threads,
/// and this trait just provides a convenient alias for those traits.
pub trait PaginatedItem: DeserializeOwned + Send + 'static {}

/// Indicate to the compiler that any type which implements DeserializeOwned
/// and Send can be used as a PaginatedItem.
impl<T> PaginatedItem for T where T: DeserializeOwned + Send + 'static {}

/// A page of items from a paginated response.
pub struct PaginatedResult<T: PaginatedItem, U: PaginatedItemHandler<T> = ()> {
    res:     Response,
    handler: Option<U>,
    phantom: PhantomData<T>,
}

impl<T: PaginatedItem, U: PaginatedItemHandler<T>> PaginatedResult<T, U> {
    pub fn new(res: Response, handler: Option<U>) -> Self {
        Self {
            res,
            handler,
            phantom: PhantomData,
        }
    }

    /// Returns the page's list of items, running them through the item handler
    /// if set.
    pub async fn items(self) -> Result<Vec<T>> {
        let mut items: Vec<T> = self.res.body().await?;

        if let Some(handler) = self.handler {
            items.iter_mut().for_each(|item| handler.handle(item));
        }

        Ok(items)
    }

    fn next_link(&self) -> Option<Link> {
        self.res
            .inner
            .headers()
            .get_all(reqwest::header::LINK)
            .iter()
            .map(Link::try_from)
            .flatten()
            .find(|l| l.rel == "next")
    }
}
