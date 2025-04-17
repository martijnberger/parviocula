use crate::asgi::AsgiService;
use axum::body::Body as AxumBody;
use http_body_util::BodyExt;
use http::{Request, Response};
use std::convert::Infallible;
use std::pin::Pin;
use std::task::Poll;
use tower_service::Service;
use futures::future::Future;

/// Converts between crate::http::Body and axum::body::Body
pub trait IntoBody {
    type Target;
    
    fn into_body(self) -> Self::Target;
}

impl IntoBody for crate::http::Body {
    type Target = AxumBody;
    
    fn into_body(self) -> Self::Target {
        // Convert our internal Body to axum's Body
        // We need to use a new approach since Body's internals are private
        AxumBody::new(
            http_body_util::BodyExt::map_err(self, |e| 
                Box::new(e) as Box<dyn std::error::Error + Send + Sync>
            )
        )
    }
}

impl IntoBody for AxumBody {
    type Target = crate::http::Body;
    
    fn into_body(self) -> Self::Target {
        // Convert axum's Body to our internal Body type
        // We'll collect the body and convert it to our Body type
        // Create a default empty body in case of error
        let default_body = crate::http::Body::from("");
        
        // Return the body or default in case of error
        futures::executor::block_on(async {
            match self.collect().await {
                Ok(collected) => {
                    let bytes = collected.to_bytes();
                    crate::http::Body::from(bytes)
                },
                Err(_) => default_body,
            }
        })
    }
}

impl Service<Request<AxumBody>> for AsgiService {
    type Response = Response<AxumBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<AxumBody>) -> Self::Future {
        // Convert the request body to our internal Body type
        let (parts, body) = req.into_parts();
        let body = body.into_body();
        let req = Request::from_parts(parts, body);
        
        // Call the tower_service implementation from the trait
        let mut this = self.clone();
        Box::pin(async move {
            match <AsgiService as Service<Request<crate::http::Body>>>::call(&mut this, req).await {
                Ok(response) => {
                    // Convert the response body to axum's Body type
                    let (parts, body) = response.into_parts();
                    let axum_body = body.into_body();
                    let axum_response = Response::from_parts(parts, axum_body);
                    Ok(axum_response)
                }
                Err(_err) => {
                    // Convert the error to Infallible (this should never happen in practice)
                    #[cfg(feature = "tracing")]
                    tracing::error!("Unexpected error in AsgiService: {:?}", _err);
                    
                    // Return a 500 response
                    let response = Response::builder()
                        .status(500)
                        .body(AxumBody::from("Internal Server Error"))
                        .unwrap();
                    Ok(response)
                }
            }
        })
    }
}