#[macro_use]
extern crate async_trait;

use std::convert::Infallible;
use std::error::Error as StdError;
use std::fmt::Formatter;
use std::io;
use std::pin::Pin;
use std::str::FromStr;

use acme::AcmeClientError;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bollard::Docker;
use futures::prelude::*;
use serde::{Deserialize, Deserializer, Serialize};
use shuttle_common::models::error::{ApiError, ErrorKind};
use tokio::sync::mpsc::error::SendError;
use tracing::error;

pub mod acme;
pub mod api;
pub mod args;
pub mod auth;
pub mod project;
pub mod proxy;
pub mod service;
pub mod task;
pub mod tls;
pub mod worker;

use crate::service::{ContainerSettings, GatewayService};

/// Server-side errors that do not have to do with the user runtime
/// should be [`Error`]s.
///
/// All [`Error`] have an [`ErrorKind`] and an (optional) source.

/// [`Error] is safe to be used as error variants to axum endpoints
/// return types as their [`IntoResponse`] implementation does not
/// leak any sensitive information.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    source: Option<Box<dyn StdError + Sync + Send + 'static>>,
}

impl Error {
    pub fn source<E: StdError + Sync + Send + 'static>(kind: ErrorKind, err: E) -> Self {
        Self {
            kind,
            source: Some(Box::new(err)),
        }
    }

    pub fn custom<S: AsRef<str>>(kind: ErrorKind, message: S) -> Self {
        Self {
            kind,
            source: Some(Box::new(io::Error::new(
                io::ErrorKind::Other,
                message.as_ref().to_string(),
            ))),
        }
    }

    pub fn from_kind(kind: ErrorKind) -> Self {
        Self { kind, source: None }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Self::from_kind(kind)
    }
}

impl<T> From<SendError<T>> for Error {
    fn from(_: SendError<T>) -> Self {
        Self::from(ErrorKind::ServiceUnavailable)
    }
}

impl From<io::Error> for Error {
    fn from(_: io::Error) -> Self {
        Self::from(ErrorKind::Internal)
    }
}

impl From<AcmeClientError> for Error {
    fn from(error: AcmeClientError) -> Self {
        Self::source(ErrorKind::Internal, error)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        error!(error = %self, "request had an error");

        let error: ApiError = self.kind.into();

        (error.status(), Json(error)).into_response()
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind)?;
        if let Some(source) = self.source.as_ref() {
            write!(f, ": ")?;
            source.fmt(f)?;
        }
        Ok(())
    }
}

impl StdError for Error {}

#[derive(Debug, sqlx::Type, Serialize, Clone, PartialEq, Eq, Hash)]
#[sqlx(transparent)]
pub struct ProjectName(String);

impl ProjectName {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn is_valid(&self) -> bool {
        let name = self.0.clone();

        fn is_valid_char(byte: u8) -> bool {
            matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-')
        }

        // each label in a hostname can be between 1 and 63 chars
        let is_invalid_length = name.len() > 63;

        !(name.bytes().any(|byte| !is_valid_char(byte))
            || name.ends_with('-')
            || name.starts_with('-')
            || name.is_empty()
            || is_invalid_length)
    }
}

impl<'de> Deserialize<'de> for ProjectName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(<D::Error as serde::de::Error>::custom)
    }
}

impl FromStr for ProjectName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<shuttle_common::project::ProjectName>()
            .map_err(|_| Error::from_kind(ErrorKind::InvalidProjectName))
            .map(|pn| Self(pn.to_string()))
    }
}

impl std::fmt::Display for ProjectName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type, Serialize)]
#[sqlx(transparent)]
pub struct AccountName(String);

impl FromStr for AccountName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for AccountName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for AccountName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(|_err| todo!())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDetails {
    pub project_name: ProjectName,
    pub account_name: AccountName,
}

impl From<ProjectDetails> for shuttle_common::models::project::AdminResponse {
    fn from(project: ProjectDetails) -> Self {
        Self {
            project_name: project.project_name.to_string(),
            account_name: project.account_name.to_string(),
        }
    }
}

pub trait DockerContext: Send + Sync {
    fn docker(&self) -> &Docker;

    fn container_settings(&self) -> &ContainerSettings;
}

#[async_trait]
pub trait Service {
    type Context;

    type State: EndState<Self::Context>;

    type Error;

    /// Asks for the latest available context for task execution
    fn context(&self) -> Self::Context;

    /// Commit a state update to persistence
    async fn update(&self, state: &Self::State) -> Result<(), Self::Error>;
}

/// A generic state which can, when provided with a [`Context`], do
/// some work and advance itself
#[async_trait]
pub trait State<Ctx>: Send {
    type Next;

    type Error;

    async fn next(self, ctx: &Ctx) -> Result<Self::Next, Self::Error>;
}

pub type StateTryStream<'c, St, Err> = Pin<Box<dyn Stream<Item = Result<St, Err>> + Send + 'c>>;

pub trait EndState<Ctx>
where
    Self: State<Ctx, Error = Infallible, Next = Self>,
{
    fn is_done(&self) -> bool;
}

pub trait EndStateExt<Ctx>: TryState + EndState<Ctx>
where
    Ctx: Sync,
    Self: Clone,
{
    /// Convert the state into a [`TryStream`] that yields
    /// the generated states.
    ///
    /// This stream will not end.
    fn into_stream<'c>(self, ctx: &'c Ctx) -> StateTryStream<'c, Self, Self::ErrorVariant>
    where
        Self: 'c,
    {
        Box::pin(stream::try_unfold((self, ctx), |(state, ctx)| async move {
            state
                .next(ctx)
                .await
                .unwrap() // EndState's `next` is Infallible
                .into_result()
                .map(|state| Some((state.clone(), (state, ctx))))
        }))
    }
}

impl<Ctx, S> EndStateExt<Ctx> for S
where
    S: Clone + TryState + EndState<Ctx>,
    Ctx: Send + Sync,
{
}

/// A [`State`] which contains all its transitions, including
/// failures
pub trait TryState: Sized {
    type ErrorVariant;

    fn into_result(self) -> Result<Self, Self::ErrorVariant>;
}

pub trait IntoTryState<S>
where
    S: TryState,
{
    fn into_try_state(self) -> Result<S, Infallible>;
}

impl<S, F, Err> IntoTryState<S> for Result<F, Err>
where
    S: TryState + From<F> + From<Err>,
{
    fn into_try_state(self) -> Result<S, Infallible> {
        self.map(|s| S::from(s)).or_else(|err| Ok(S::from(err)))
    }
}

#[async_trait]
pub trait Refresh<Ctx>: Sized {
    type Error: StdError;

    async fn refresh(self, ctx: &Ctx) -> Result<Self, Self::Error>;
}

#[cfg(test)]
pub mod tests {
    use std::env;
    use std::io::Read;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{anyhow, Context as AnyhowContext};
    use axum::headers::Authorization;
    use bollard::Docker;
    use fqdn::FQDN;
    use futures::prelude::*;
    use hyper::client::HttpConnector;
    use hyper::http::uri::Scheme;
    use hyper::http::Uri;
    use hyper::{Body, Client as HyperClient, Request, Response, StatusCode};
    use rand::distributions::{Alphanumeric, DistString, Distribution, Uniform};
    use shuttle_common::deployment::State;
    use shuttle_common::log;
    use shuttle_common::models::{deployment, project, service, user};
    use sqlx::SqlitePool;
    use tokio::sync::mpsc::channel;

    use crate::acme::AcmeClient;
    use crate::api::latest::ApiBuilder;
    use crate::args::{ContextArgs, StartArgs, UseTls};
    use crate::auth::User;
    use crate::proxy::UserServiceBuilder;
    use crate::service::{ContainerSettings, GatewayService, MIGRATIONS};
    use crate::worker::Worker;
    use crate::DockerContext;

    macro_rules! value_block_helper {
        ($next:ident, $block:block) => {
            $block
        };
        ($next:ident,) => {
            $next
        };
    }

    macro_rules! assert_stream_matches {
        (
            $stream:ident,
            $(#[assertion = $assert:literal])?
                $($pattern:pat_param)|+ $(if $guard:expr)? $(=> $more:block)?,
        ) => {{
            let next = ::futures::stream::StreamExt::next(&mut $stream)
                .await
                .expect("Stream ended before the last of assertions");

            match &next {
                $($pattern)|+ $(if $guard)? => {
                    print!("{}", ::colored::Colorize::green(::colored::Colorize::bold("[ok]")));
                    $(print!(" {}", $assert);)?
                        print!("\n");
                    crate::tests::value_block_helper!(next, $($more)?)
                },
                _ => {
                    eprintln!("{} {:#?}", ::colored::Colorize::red(::colored::Colorize::bold("[err]")), next);
                    eprint!("{}", ::colored::Colorize::red(::colored::Colorize::bold("Assertion failed")));
                    $(eprint!(": {}", $assert);)?
                        eprint!("\n");
                    panic!("State mismatch")
                }
            }
        }};
        (
            $stream:ident,
            $(#[$($meta:tt)*])*
                $($pattern:pat_param)|+ $(if $guard:expr)? $(=> $more:block)?,
            $($(#[$($metas:tt)*])* $($patterns:pat_param)|+ $(if $guards:expr)? $(=> $mores:block)?,)+
        ) => {{
            assert_stream_matches!(
                $stream,
                $(#[$($meta)*])* $($pattern)|+ $(if $guard)? => {
                    $($more)?
                        assert_stream_matches!(
                            $stream,
                            $($(#[$($metas)*])* $($patterns)|+ $(if $guards)? $(=> $mores)?,)+
                        )
                },
            )
        }};
    }

    macro_rules! assert_matches {
        {
            $ctx:ident,
            $state:expr,
            $($(#[$($meta:tt)*])* $($patterns:pat_param)|+ $(if $guards:expr)? $(=> $mores:block)?,)+
        } => {{
            let state = $state;
            let mut stream = crate::EndStateExt::into_stream(state, &$ctx);
            assert_stream_matches!(
                stream,
                $($(#[$($meta)*])* $($patterns)|+ $(if $guards)? $(=> $mores)?,)+
            )
        }}
    }

    macro_rules! assert_err_kind {
        {
            $left:expr, ErrorKind::$right:ident
        } => {{
            let left: Result<_, crate::Error> = $left;
            assert_eq!(
                left.map_err(|err| err.kind()),
                Err(crate::ErrorKind::$right)
            );
        }};
    }

    macro_rules! timed_loop {
        (wait: $wait:literal$(, max: $max:literal)?, $block:block) => {{
            #[allow(unused_mut)]
            #[allow(unused_variables)]
            let mut tries = 0;
            loop {
                $block
                    tries += 1;
                $(if tries > $max {
                    panic!("timed out in the loop");
                })?
                    ::tokio::time::sleep(::std::time::Duration::from_secs($wait)).await;
            }
        }};
    }

    pub(crate) use {assert_err_kind, assert_matches, assert_stream_matches, value_block_helper};

    mod request_builder_ext {
        pub trait Sealed {}

        impl Sealed for axum::http::request::Builder {}

        impl<'r> Sealed for &'r mut axum::headers::HeaderMap {}

        impl<B> Sealed for axum::http::Request<B> {}
    }

    pub trait RequestBuilderExt: Sized + request_builder_ext::Sealed {
        fn with_header<H: axum::headers::Header>(self, header: &H) -> Self;
    }

    impl RequestBuilderExt for axum::http::request::Builder {
        fn with_header<H: axum::headers::Header>(mut self, header: &H) -> Self {
            self.headers_mut().unwrap().with_header(header);
            self
        }
    }

    impl<'r> RequestBuilderExt for &'r mut axum::headers::HeaderMap {
        fn with_header<H: axum::headers::Header>(self, header: &H) -> Self {
            let mut buf = vec![];
            header.encode(&mut buf);
            self.append(H::name(), buf.pop().unwrap());
            self
        }
    }

    impl<B> RequestBuilderExt for Request<B> {
        fn with_header<H: axum::headers::Header>(mut self, header: &H) -> Self {
            self.headers_mut().with_header(header);
            self
        }
    }

    pub struct Client<C = HttpConnector, B = Body> {
        target: SocketAddr,
        hyper: Option<HyperClient<C, B>>,
    }

    impl<C, B> Client<C, B> {
        pub fn new<A: Into<SocketAddr>>(target: A) -> Self {
            Self {
                target: target.into(),
                hyper: None,
            }
        }

        pub fn with_hyper_client(mut self, client: HyperClient<C, B>) -> Self {
            self.hyper = Some(client);
            self
        }
    }

    impl Client<HttpConnector, Body> {
        pub async fn request(
            &self,
            mut req: Request<Body>,
        ) -> Result<Response<Vec<u8>>, hyper::Error> {
            if req.uri().authority().is_none() {
                let mut uri = req.uri().clone().into_parts();
                uri.scheme = Some(Scheme::HTTP);
                uri.authority = Some(self.target.to_string().parse().unwrap());
                *req.uri_mut() = Uri::from_parts(uri).unwrap();
            }
            self.hyper
                .as_ref()
                .unwrap()
                .request(req)
                .and_then(|mut resp| async move {
                    let body = resp
                        .body_mut()
                        .try_fold(Vec::new(), |mut acc, x| async move {
                            acc.extend(x);
                            Ok(acc)
                        })
                        .await?;
                    let (parts, _) = resp.into_parts();
                    Ok(Response::from_parts(parts, body))
                })
                .await
        }
    }

    pub struct World {
        docker: Docker,
        settings: ContainerSettings,
        args: StartArgs,
        hyper: HyperClient<HttpConnector, Body>,
        pool: SqlitePool,
        acme_client: AcmeClient,
    }

    #[derive(Clone)]
    pub struct WorldContext {
        pub docker: Docker,
        pub container_settings: ContainerSettings,
        pub hyper: HyperClient<HttpConnector, Body>,
    }

    impl World {
        pub async fn new() -> Self {
            let docker = Docker::connect_with_local_defaults().unwrap();

            docker
                .list_images::<&str>(None)
                .await
                .context(anyhow!("A docker daemon does not seem accessible",))
                .unwrap();

            let control: i16 = Uniform::from(9000..10000).sample(&mut rand::thread_rng());
            let user = control + 1;
            let bouncer = user + 1;
            let control = format!("127.0.0.1:{control}").parse().unwrap();
            let user = format!("127.0.0.1:{user}").parse().unwrap();
            let bouncer = format!("127.0.0.1:{bouncer}").parse().unwrap();

            let prefix = format!(
                "shuttle_test_{}_",
                Alphanumeric.sample_string(&mut rand::thread_rng(), 4)
            );

            let image = env::var("SHUTTLE_TESTS_RUNTIME_IMAGE")
                .unwrap_or_else(|_| "public.ecr.aws/shuttle/deployer:latest".to_string());

            let network_name =
                env::var("SHUTTLE_TESTS_NETWORK").unwrap_or_else(|_| "shuttle_default".to_string());

            let provisioner_host = "provisioner".to_string();

            let docker_host = "/var/run/docker.sock".to_string();

            let args = StartArgs {
                control,
                user,
                bouncer,
                use_tls: UseTls::Disable,
                context: ContextArgs {
                    docker_host,
                    image,
                    prefix,
                    provisioner_host,
                    network_name,
                    proxy_fqdn: FQDN::from_str("test.shuttleapp.rs").unwrap(),
                },
            };

            let settings = ContainerSettings::builder().from_args(&args.context).await;

            let hyper = HyperClient::builder().build(HttpConnector::new());

            let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
            MIGRATIONS.run(&pool).await.unwrap();

            let acme_client = AcmeClient::new();

            Self {
                docker,
                settings,
                args,
                hyper,
                pool,
                acme_client,
            }
        }

        pub fn args(&self) -> ContextArgs {
            self.args.context.clone()
        }

        pub fn pool(&self) -> SqlitePool {
            self.pool.clone()
        }

        pub fn client<A: Into<SocketAddr>>(&self, addr: A) -> Client {
            Client::new(addr).with_hyper_client(self.hyper.clone())
        }

        pub fn fqdn(&self) -> FQDN {
            self.args().proxy_fqdn
        }

        pub fn acme_client(&self) -> AcmeClient {
            self.acme_client.clone()
        }
    }

    impl World {
        pub fn context(&self) -> WorldContext {
            WorldContext {
                docker: self.docker.clone(),
                container_settings: self.settings.clone(),
                hyper: self.hyper.clone(),
            }
        }
    }

    impl DockerContext for WorldContext {
        fn docker(&self) -> &Docker {
            &self.docker
        }

        fn container_settings(&self) -> &ContainerSettings {
            &self.container_settings
        }
    }

    #[tokio::test]
    async fn end_to_end() {
        let world = World::new().await;
        let service = Arc::new(GatewayService::init(world.args(), world.pool()).await);
        let worker = Worker::new();

        let (log_out, mut log_in) = channel(256);
        tokio::spawn({
            let sender = worker.sender();
            async move {
                while let Some(work) = log_in.recv().await {
                    sender
                        .send(work)
                        .await
                        .map_err(|_| "could not send work")
                        .unwrap();
                }
            }
        });

        let base_port = loop {
            let port = portpicker::pick_unused_port().unwrap();
            if portpicker::is_free_tcp(port + 1) {
                break port;
            }
        };

        let api_addr = format!("127.0.0.1:{}", base_port).parse().unwrap();
        let api_client = world.client(api_addr);
        let api = ApiBuilder::new()
            .with_service(Arc::clone(&service))
            .with_sender(log_out)
            .with_default_routes()
            .binding_to(api_addr);

        let user_addr: SocketAddr = format!("127.0.0.1:{}", base_port + 1).parse().unwrap();
        let proxy_client = world.client(user_addr);
        let user = UserServiceBuilder::new()
            .with_service(Arc::clone(&service))
            .with_public(world.fqdn())
            .with_user_proxy_binding_to(user_addr);

        let _gateway = tokio::spawn(async move {
            tokio::select! {
                _ = worker.start() => {},
                _ = api.serve() => {},
                _ = user.serve() => {}
            }
        });

        let User { key, name, .. } = service.create_user("neo".parse().unwrap()).await.unwrap();
        service.set_super_user(&name, true).await.unwrap();

        println!("Creating trinity user");
        let user::Response { key, .. } = api_client
            .request(
                Request::post("/users/trinity")
                    .with_header(&Authorization::bearer(key.as_str()).unwrap())
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
                serde_json::from_slice(resp.body()).unwrap()
            })
            .await
            .unwrap();

        let authorization = Authorization::bearer(key.as_str()).unwrap();

        println!("Creating the matrix project");
        api_client
            .request(
                Request::post("/projects/matrix")
                    .with_header(&authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();

        timed_loop!(wait: 1, max: 12, {
            let project: project::Response = api_client
                .request(
                    Request::get("/projects/matrix")
                        .with_header(&authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .map_ok(|resp| {
                    assert_eq!(resp.status(), StatusCode::OK);
                    serde_json::from_slice(resp.body()).unwrap()
                })
                .await
                .unwrap();

            if project.state == project::State::Ready {
                break;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        println!("get matrix project status");
        api_client
            .request(
                Request::get("/projects/matrix/status")
                    .with_header(&authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| assert_eq!(resp.status(), StatusCode::OK))
            .await
            .unwrap();

        // === deployment test BEGIN ===
        println!("deploy the matrix project");
        api_client
            .request({
                let mut data = Vec::new();
                let mut f = std::fs::File::open("tests/hello_world.crate").unwrap();
                f.read_to_end(&mut data).unwrap();
                Request::post("/projects/matrix/services/matrix")
                    .with_header(&authorization)
                    .body(Body::from(data))
                    .unwrap()
            })
            .map_ok(|resp| assert_eq!(resp.status(), StatusCode::OK))
            .await
            .unwrap();

        timed_loop!(wait: 1, max: 600, {
            let service: service::Detailed = api_client
                .request(
                    Request::get("/projects/matrix/services/matrix")
                        .with_header(&authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .map_ok(|resp| {
                    assert_eq!(resp.status(), StatusCode::OK);
                    serde_json::from_slice(resp.body()).unwrap()
                })
                .await
                .unwrap();

                match service.deployments.first() {
                    Some(deployment::Response{ state: State::Running, .. }) => break,
                    Some(deployment::Response{ state: State::Crashed, id, .. }) => {
                        let logs: Vec<log::Item> = api_client
                            .request(
                                Request::get(format!("/projects/matrix/deployments/{id}/logs"))
                                    .with_header(&authorization)
                                    .body(Body::empty())
                                    .unwrap(),
                            )
                            .map_ok(|resp| {
                                assert_eq!(resp.status(), StatusCode::OK);
                                serde_json::from_slice(resp.body()).unwrap()
                            })
                            .await
                            .unwrap();

                        for log in logs {
                            println!("{log}");
                        }

                        panic!("deployment failed");
                    },
                    _ => {},
            }
        });

        println!("make request on the matrix project");
        proxy_client
            .request(
                Request::get("/hello")
                    .header("Host", "matrix.test.shuttleapp.rs")
                    .header("x-shuttle-project", "matrix")
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
                assert_eq!(
                    String::from_utf8(resp.into_body()).unwrap().as_str(),
                    "Hello, world!"
                );
            })
            .await
            .unwrap();
        // === deployment test END ===

        println!("delete matrix project");
        api_client
            .request(
                Request::delete("/projects/matrix")
                    .with_header(&authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| assert_eq!(resp.status(), StatusCode::OK))
            .await
            .unwrap();

        timed_loop!(wait: 1, max: 20, {
            let resp = api_client
                .request(
                    Request::get("/projects/matrix")
                        .with_header(&authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let resp = serde_json::from_slice::<project::Response>(resp.body().as_slice()).unwrap();
            if matches!(resp.state, project::State::Destroyed) {
                break;
            }
        });

        // Attempting to delete already Destroyed project will return Destroyed
        api_client
            .request(
                Request::delete("/projects/matrix")
                    .with_header(&authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
                let resp =
                    serde_json::from_slice::<project::Response>(resp.body().as_slice()).unwrap();
                assert_eq!(resp.state, project::State::Destroyed);
            })
            .await
            .unwrap();
    }
}
