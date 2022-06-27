use std::convert::TryFrom;
use std::future::Future;
use std::pin::Pin;

use chrono::prelude::*;
use dyn_clone::DynClone;
use hmac::{Hmac, Mac};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::error::ErrorInfo;
use crate::{http, rest, Result};

/// The maximum length of a valid token. Tokens with a length longer than this
/// are rejected with a 40170 error code.
const MAX_TOKEN_LENGTH: usize = 128 * 1024;

/// An API Key used to authenticate with the REST API using HTTP Basic Auth.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Key {
    #[serde(rename(deserialize = "keyName"))]
    pub name: String,
    pub value: String,
}

impl TryFrom<&str> for Key {
    type Error = ErrorInfo;

    /// Parse an API Key from a string of the form '<keyName>:<keySecret>'.
    ///
    /// # Example
    ///
    /// ```
    /// use std::convert::TryFrom;
    /// use ably::auth;
    ///
    /// let res = auth::Key::try_from("ABC123.DEF456:XXXXXXXXXXXX");
    /// assert!(res.is_ok());
    ///
    /// let res = auth::Key::try_from("not-a-valid-key");
    /// assert!(res.is_err());
    /// ```
    fn try_from(s: &str) -> Result<Self> {
        if let [name, value] = s.splitn(2, ':').collect::<Vec<&str>>()[..] {
            Ok(Key {
                name: name.to_string(),
                value: value.to_string(),
            })
        } else {
            Err(error!(40000, "Invalid key"))
        }
    }
}

impl Key {
    /// Use the API key to sign the given TokenParams, returning a signed
    /// TokenRequest which can be exchanged for a token.
    ///
    /// # Example
    ///
    /// ```
    /// # async fn run() -> ably::Result<()> {
    /// use std::convert::TryFrom;
    /// use ably::auth;
    ///
    /// let key = auth::Key::try_from("ABC123.DEF456:XXXXXXXXXXXX").unwrap();
    ///
    /// let mut params = auth::TokenParams::default();
    /// params.client_id = Some("test@example.com".to_string());
    ///
    /// let req = key.sign(params).await.unwrap();
    ///
    /// assert!(matches!(req, auth::Token::Request(_)));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn sign(&self, params: TokenParams) -> Result<Token> {
        params.sign(self).map(Token::Request)
    }
}

impl AuthCallback for Key {
    /// Support using the API key as an AuthCallback which always returns a
    /// signed token request.
    fn token(&self, _rest: &rest::Rest, params: TokenParams) -> TokenFuture {
        Box::pin(self.sign(params))
    }
}

/// Provides functions relating to Ably API authentication.
#[derive(Clone, Debug)]
pub struct Auth<'a> {
    pub(crate) rest: &'a rest::Rest,
}

impl<'a> Auth<'a> {
    pub fn new(rest: &'a rest::Rest) -> Self {
        Self { rest }
    }

    /// Start building a TokenRequest to be signed by a local API key.
    pub fn create_token_request(&self) -> CreateTokenRequestBuilder {
        let mut builder = CreateTokenRequestBuilder::new();

        if let Some(key) = &self.rest.opts.key {
            builder = builder.key(key.clone());
        }

        if let Some(client_id) = &self.rest.opts.client_id {
            builder = builder.client_id(client_id);
        }

        builder
    }

    /// Start building a request for a token.
    pub fn request_token(&self) -> RequestTokenBuilder {
        let mut builder = RequestTokenBuilder::new(self.rest);

        if let Some(ref callback) = self.rest.opts.auth_callback {
            builder = builder.auth_callback(callback.clone());
        } else if let Some(ref url) = self.rest.opts.auth_url {
            builder = builder.auth_url(AuthUrl {
                url: url.clone(),
                method: self.rest.opts.auth_method.clone(),
                headers: self.rest.opts.auth_headers.clone(),
                params: self.rest.opts.auth_params.clone(),
            });
        } else if let Some(ref key) = self.rest.opts.key {
            builder = builder.key(key.clone());
        } else if let Some(ref token) = self.rest.opts.token {
            builder = builder.token(token.clone());
        }

        if let Some(params) = &self.rest.opts.default_token_params {
            builder = builder.params(params.clone());
        }

        if let Some(client_id) = &self.rest.opts.client_id {
            builder = builder.client_id(client_id);
        }

        builder
    }

    /// Set the Authorization header in the given request.
    pub async fn with_auth_headers(&self, req: &mut reqwest::Request) -> Result<()> {
        if let Some(ref key) = self.rest.opts.key {
            if !self.rest.opts.use_token_auth {
                return Self::set_basic_auth(req, key);
            }
        }

        let res = self.request_token().send().await?;
        Self::set_bearer_auth(req, &res.token)
    }

    fn set_bearer_auth(req: &mut reqwest::Request, token: &str) -> Result<()> {
        Self::set_header(
            req,
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token),
        )
    }

    fn set_basic_auth(req: &mut reqwest::Request, key: &Key) -> Result<()> {
        let encoded = base64::encode(format!("{}:{}", key.name, key.value));
        Self::set_header(
            req,
            reqwest::header::AUTHORIZATION,
            format!("Basic {}", encoded),
        )
    }

    fn set_header(req: &mut reqwest::Request, key: http::HeaderName, value: String) -> Result<()> {
        req.headers_mut().append(key, value.parse()?);
        Ok(())
    }

    /// Generate a random 16 character nonce to use in a TokenRequest.
    fn generate_nonce() -> String {
        thread_rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect()
    }

    /// Use the given API key to compute the HMAC of the canonicalised
    /// representation of the given TokenRequest.
    ///
    /// See the [REST API Token Request Spec] for further details.
    ///
    /// [REST API Token Request Spec]: https://docs.ably.io/rest-api/token-request-spec/
    fn compute_mac(key: &Key, req: &TokenRequest) -> Result<String> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key.value.as_bytes())?;

        mac.update(key.name.as_bytes());
        mac.update(b"\n");

        mac.update(
            req.ttl
                .map(|t| t.to_string())
                .as_ref()
                .map(|t| t.as_bytes())
                .unwrap_or_default(),
        );
        mac.update(b"\n");

        mac.update(
            req.capability
                .as_ref()
                .map(|c| c.as_bytes())
                .unwrap_or_default(),
        );
        mac.update(b"\n");

        mac.update(
            req.client_id
                .as_ref()
                .map(|c| c.as_bytes())
                .unwrap_or_default(),
        );
        mac.update(b"\n");

        let timestamp_ms =
            req.timestamp.timestamp() * 1000 + req.timestamp.timestamp_subsec_millis() as i64;
        mac.update(timestamp_ms.to_string().as_bytes());
        mac.update(b"\n");

        mac.update(req.nonce.as_bytes());
        mac.update(b"\n");

        Ok(base64::encode(mac.finalize().into_bytes()))
    }
}

/// A builder to create a signed TokenRequest.
pub struct CreateTokenRequestBuilder {
    key: Option<Key>,
    params: TokenParams,
}

impl CreateTokenRequestBuilder {
    fn new() -> Self {
        Self {
            key: None,
            params: TokenParams::default(),
        }
    }

    /// Set the key to use to sign the TokenRequest.
    pub fn key(mut self, key: Key) -> Self {
        self.key = Some(key);
        self
    }

    /// Set the desired capability.
    pub fn capability(mut self, capability: &str) -> Self {
        self.params.capability = Some(capability.to_string());
        self
    }

    /// Set the desired client_id.
    pub fn client_id(mut self, client_id: &str) -> Self {
        self.params.client_id = Some(client_id.to_string());
        self
    }

    /// Set the desired TTL.
    pub fn ttl(mut self, ttl: i64) -> Self {
        self.params.ttl = Some(ttl);
        self
    }

    /// Set the timestamp.
    pub fn timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.params.timestamp = Some(timestamp);
        self
    }

    /// Sign and return the TokenRequest.
    pub fn sign(self) -> Result<TokenRequest> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| error!(40106, "API key is required to create signed token requests"))?;
        self.params.sign(key)
    }
}

/// A builder to request a token.
pub struct RequestTokenBuilder<'a> {
    rest: &'a rest::Rest,
    callback: Option<Box<dyn AuthCallback>>,
    params: TokenParams,
}

impl<'a> RequestTokenBuilder<'a> {
    fn new(rest: &'a rest::Rest) -> Self {
        Self {
            rest,
            callback: None,
            params: TokenParams::default(),
        }
    }

    /// Use a key as the AuthCallback.
    pub fn key(self, key: Key) -> Self {
        self.auth_callback(key)
    }

    /// Use a token as the AuthCallback.
    pub fn token(self, token: Token) -> Self {
        self.auth_callback(token)
    }

    /// Use a URL as the AuthCallback.
    pub fn auth_url(self, url: impl Into<AuthUrl>) -> Self {
        let callback = AuthUrlCallback::new(url.into());
        self.auth_callback(callback)
    }

    /// Use a custom AuthCallback.
    pub fn auth_callback(mut self, callback: impl AuthCallback + 'static) -> Self {
        self.callback = Some(Box::new(callback));
        self
    }

    /// Set the TokenParams.
    pub fn params(mut self, params: TokenParams) -> Self {
        self.params = params;
        self
    }

    /// Set the desired capability.
    pub fn capability(mut self, capability: &str) -> Self {
        self.params.capability = Some(capability.to_string());
        self
    }

    /// Set the desired client_id.
    pub fn client_id(mut self, client_id: &str) -> Self {
        self.params.client_id = Some(client_id.to_string());
        self
    }

    /// Set the desired TTL.
    pub fn ttl(mut self, ttl: i64) -> Self {
        self.params.ttl = Some(ttl);
        self
    }

    /// Set the timestamp.
    pub fn timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.params.timestamp = Some(timestamp);
        self
    }

    /// Request a token response from the configured AuthCallback.
    ///
    /// If the response is a TokenRequest, exchange it for a token.
    pub async fn send(self) -> Result<TokenDetails> {
        let callback = self
            .callback
            .as_ref()
            .ok_or_else(|| error!(40171, "no means provided to renew auth token"))?;

        let details = match callback.token(self.rest, self.params.clone()).await {
            // The callback may either:
            // - return a TokenRequest which we'll exchange for a TokenDetails
            // - return a token literal which we'll wrap in a TokenDetails
            // - return a TokenDetails which we'll just return as is
            Ok(token) => match token {
                Token::Request(req) => self.exchange(&req).await?,
                Token::Literal(token) => TokenDetails::from(token),
                Token::Details(details) => details,
            },
            Err(mut err) => {
                // Normalise auth error according to RSA4e.
                if err.code == 40000 {
                    err.code = 40170;
                    err.status_code = Some(401);
                }
                return Err(err);
            }
        };

        // Reject tokens with size greater than 128KiB (RSA4f).
        if details.token.len() > MAX_TOKEN_LENGTH {
            return Err(error!(
                40170,
                format!(
                    "Token string exceeded max permitted length (was {} bytes)",
                    details.token.len()
                ),
                401
            ));
        }

        Ok(details)
    }

    /// Exchange a TokenRequest for a token by making a HTTP request to the
    /// [requestToken endpoint] in the Ably REST API.
    ///
    /// Returns a boxed future rather than using async since this is both
    /// called from and calls out to RequestBuilder.send, and recursive
    /// async functions are not supported.
    ///
    /// [requestToken endpoint]: https://docs.ably.io/rest-api/#request-token
    fn exchange(
        &self,
        req: &TokenRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TokenDetails>> + Send + 'a>> {
        let req = self
            .rest
            .request(
                http::Method::POST,
                &format!("/keys/{}/requestToken", req.key_name),
            )
            .authenticate(false)
            .body(req);

        Box::pin(async move { req.send().await?.body().await.map_err(Into::into) })
    }
}

/// An AuthCallback which requests tokens from a URL.
#[derive(Clone, Debug)]
pub struct AuthUrlCallback {
    url: AuthUrl,
}

impl AuthUrlCallback {
    fn new(url: AuthUrl) -> Self {
        Self { url }
    }

    /// Request a token from the URL.
    async fn request(&self, rest: &rest::Rest, _params: TokenParams) -> Result<Token> {
        let res = self.url.request(rest).authenticate(false).send().await?;

        // Parse the token response based on the Content-Type header.
        let content_type = res
            .content_type()
            .ok_or_else(|| error!(40170, "authUrl response is missing a content-type header"))?;
        match content_type.essence_str() {
            "application/json" => {
                // Expect a JSON encoded TokenRequest or TokenDetails, and just
                // let serde figure out which Token variant to decode the JSON
                // response into.
                res.json().await
            },

            "text/plain" | "application/jwt" => {
                // Expect a literal token string.
                let token = res.text().await?;
                Ok(Token::Literal(token))
            },

            // Anything else is an error.
            _ => Err(error!(40170, format!("authUrl responded with unacceptable content-type {}, should be either text/plain, application/jwt or application/json", content_type))),
        }
    }
}

impl AuthCallback for AuthUrlCallback {
    fn token<'a>(&'a self, rest: &'a rest::Rest, params: TokenParams) -> TokenFuture<'a> {
        Box::pin(self.request(rest, params))
    }
}

#[derive(Clone, Debug)]
/// A URL to request a token from, along with the HTTP method, headers, and
/// query params to include in the request.
pub struct AuthUrl {
    pub url: reqwest::Url,
    pub method: http::Method,
    pub headers: Option<http::HeaderMap>,
    pub params: Option<http::UrlQuery>,
}

impl AuthUrl {
    fn request<'a>(&self, rest: &'a rest::Rest) -> http::RequestBuilder<'a> {
        let mut req = rest.request_url(self.method.clone(), self.url.clone());

        if let Some(ref headers) = self.headers {
            req = req.headers(headers.clone());
        }

        if let Some(ref params) = self.params {
            req = req.params(params);
        }

        req
    }
}

impl From<reqwest::Url> for AuthUrl {
    fn from(url: reqwest::Url) -> Self {
        Self {
            url,
            method: http::Method::GET,
            headers: None,
            params: None,
        }
    }
}

/// An Ably [TokenParams] object.
///
/// [TokenParams]: https://docs.ably.io/realtime/types/#token-params
#[derive(Clone, Debug, Default)]
pub struct TokenParams {
    pub capability: Option<String>,
    pub client_id: Option<String>,
    pub nonce: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub ttl: Option<i64>,
}

impl TokenParams {
    /// Generate a signed TokenRequest for these TokenParams using the steps
    /// described in the [REST API Token Request Spec].
    ///
    /// [REST API Token Request Spec]: https://ably.com/documentation/rest-api/token-request-spec
    pub fn sign(self, key: &Key) -> Result<TokenRequest> {
        // if client_id is set, it must be a non-empty string
        if let Some(ref client_id) = self.client_id {
            if client_id.is_empty() {
                return Err(error!(40012, "client_id can’t be an empty string"));
            }
        }

        let mut req = TokenRequest {
            key_name: key.name.clone(),
            timestamp: self.timestamp.unwrap_or_else(Utc::now),
            capability: self.capability,
            client_id: self.client_id,
            nonce: self.nonce.unwrap_or_else(Auth::generate_nonce),
            ttl: self.ttl,
            mac: None,
        };

        req.mac = Some(Auth::compute_mac(key, &req)?);

        Ok(req)
    }
}

/// An Ably [TokenRequest] object.
///
/// [TokenRequest]: https://docs.ably.io/realtime/types/#token-request
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenRequest {
    pub key_name: String,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub nonce: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<i64>,
}

/// The token details returned in a successful response from the [REST
/// requestToken endpoint].
///
/// [REST requestToken endpoint]: https://docs.ably.io/rest-api/#request-token
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenDetails {
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(with = "chrono::serde::ts_milliseconds_option")]
    pub expires: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(with = "chrono::serde::ts_milliseconds_option")]
    pub issued: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

impl From<String> for TokenDetails {
    fn from(token: String) -> Self {
        Self {
            token,
            ..Default::default()
        }
    }
}

/// A future returned from an AuthCallback which resolves to a Token.
pub type TokenFuture<'a> = Pin<Box<dyn Future<Output = Result<Token>> + Send + 'a>>;

/// An AuthCallback is used to provide a Token during a call to
/// auth::request_token.
pub trait AuthCallback: DynClone + std::fmt::Debug + Send + Sync {
    fn token<'a>(&'a self, rest: &'a rest::Rest, params: TokenParams) -> TokenFuture<'a>;
}

dyn_clone::clone_trait_object!(AuthCallback);

impl AuthCallback for Box<dyn AuthCallback> {
    fn token<'a>(&'a self, rest: &'a rest::Rest, params: TokenParams) -> TokenFuture<'a> {
        self.as_ref().token(rest, params)
    }
}

/// A response from requesting a token from an AuthCallback.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Token {
    Request(TokenRequest),
    Details(TokenDetails),
    Literal(String),
}

impl From<TokenRequest> for Token {
    fn from(t: TokenRequest) -> Self {
        Self::Request(t)
    }
}

impl From<TokenDetails> for Token {
    fn from(t: TokenDetails) -> Self {
        Self::Details(t)
    }
}

impl<T: Into<String>> From<T> for Token {
    fn from(s: T) -> Self {
        Self::Literal(s.into())
    }
}

impl AuthCallback for Token {
    fn token<'a>(&'a self, _rest: &'a rest::Rest, _params: TokenParams) -> TokenFuture<'a> {
        let token = self.clone();
        Box::pin(async move { Ok(token) })
    }
}
