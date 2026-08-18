use std::sync::Arc;

use futures::{AsyncReadExt as _, future::BoxFuture};
use gpui_http_client::{
    AsyncBody, HttpClient, Request, Response, Url,
    http::{HeaderValue, header},
};

pub struct NativeHttpClient {
    client: reqwest::Client,
    runtime: Arc<tokio::runtime::Runtime>,
    user_agent: HeaderValue,
}

impl NativeHttpClient {
    pub fn new() -> anyhow::Result<Arc<Self>> {
        let user_agent = HeaderValue::from_static("Pocket-YTM/0.1 GPUI");
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
        );
        let client = {
            let _guard = runtime.enter();
            reqwest::Client::builder()
                .user_agent(user_agent.to_str()?)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()?
        };
        Ok(Arc::new(Self {
            client,
            runtime,
            user_agent,
        }))
    }
}

impl HttpClient for NativeHttpClient {
    fn type_name(&self) -> &'static str {
        "PocketYtmNativeHttpClient"
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        Some(&self.user_agent)
    }

    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let client = self.client.clone();
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let (parts, mut body) = request.into_parts();
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).await?;

            let (status, version, headers, bytes) = runtime
                .spawn(async move {
                    let mut outgoing = client.request(parts.method, parts.uri.to_string());
                    for (name, value) in &parts.headers {
                        if name != header::HOST {
                            outgoing = outgoing.header(name, value);
                        }
                    }
                    if !bytes.is_empty() {
                        outgoing = outgoing.body(bytes);
                    }

                    let response = outgoing.send().await?;
                    let status = response.status();
                    let version = response.version();
                    let headers = response.headers().clone();
                    let bytes = response.bytes().await?;
                    Ok::<_, anyhow::Error>((status, version, headers, bytes))
                })
                .await??;
            let mut result = Response::builder().status(status).version(version);
            *result.headers_mut().expect("response builder headers") = headers;
            Ok(result.body(AsyncBody::from_bytes(bytes))?)
        })
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }
}
