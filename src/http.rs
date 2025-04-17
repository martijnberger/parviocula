use bytes::Bytes;
use http_body_util::{BodyExt, Limited};

type BoxBody = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

#[derive(Debug)]
pub struct Body(BoxBody);

impl http_body::Body for Body {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_frame(cx)
    }
}

impl From<&str> for Body {
    fn from(s: &str) -> Self {
        Body(
            http_body_util::Full::new(Bytes::from(s.to_string()))
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "body error"))
                .boxed_unsync(),
        )
    }
}

impl From<String> for Body {
    fn from(s: String) -> Self {
        Body(
            http_body_util::Full::new(Bytes::from(s))
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "body error"))
                .boxed_unsync(),
        )
    }
}

impl From<Bytes> for Body {
    fn from(bytes: Bytes) -> Self {
        Body(
            http_body_util::Full::new(bytes)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "body error"))
                .boxed_unsync(),
        )
    }
}

impl From<Vec<u8>> for Body {
    fn from(vec: Vec<u8>) -> Self {
        Body(
            http_body_util::Full::new(Bytes::from(vec))
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "body error"))
                .boxed_unsync(),
        )
    }
}

pub async fn to_bytes(body: Body, limit: usize) -> Result<Bytes, std::io::Error> {
    Limited::new(body, limit)
        .collect()
        .await
        .map(|col| col.to_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "body error"))
}
