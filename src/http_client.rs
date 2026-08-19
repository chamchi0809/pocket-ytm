use std::{io::Cursor, sync::Arc};

use futures::{AsyncReadExt as _, future::BoxFuture};
use gpui_http_client::{
    AsyncBody, HttpClient, Request, Response, Url,
    http::{HeaderValue, header},
};
use image::{GenericImageView as _, ImageFormat, codecs::jpeg::JpegEncoder};

const MAX_ARTWORK_EDGE: u32 = 384;

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

            let (status, version, mut headers, bytes) = runtime
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
            let bytes = if headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("image/jpeg"))
            {
                match resize_jpeg(&bytes) {
                    Ok(Some(resized)) => {
                        headers.insert(
                            header::CONTENT_LENGTH,
                            HeaderValue::from_str(&resized.len().to_string())?,
                        );
                        headers.remove(header::CONTENT_ENCODING);
                        resized.into()
                    }
                    Ok(None) => bytes,
                    Err(error) => {
                        log::debug!("artwork resize skipped: {error:#}");
                        bytes
                    }
                }
            } else {
                bytes
            };
            let mut result = Response::builder().status(status).version(version);
            *result.headers_mut().expect("response builder headers") = headers;
            Ok(result.body(AsyncBody::from_bytes(bytes))?)
        })
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }
}

fn resize_jpeg(bytes: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let image = image::load_from_memory_with_format(bytes, ImageFormat::Jpeg)?;
    let (width, height) = image.dimensions();
    if width <= MAX_ARTWORK_EDGE && height <= MAX_ARTWORK_EDGE {
        return Ok(None);
    }
    let resized = image.thumbnail(MAX_ARTWORK_EDGE, MAX_ARTWORK_EDGE);
    let mut encoded = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut encoded, 86).encode_image(&resized)?;
    Ok(Some(encoded.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(RgbImage::new(width, height));
        let mut bytes = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut bytes, 90)
            .encode_image(&image)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn oversized_artwork_is_resized_before_gpui_decodes_it() {
        let resized = resize_jpeg(&jpeg(800, 600)).unwrap().unwrap();
        let dimensions = image::load_from_memory(&resized).unwrap().dimensions();

        assert_eq!(dimensions, (384, 288));
        assert!(resize_jpeg(&jpeg(320, 240)).unwrap().is_none());
    }
}
