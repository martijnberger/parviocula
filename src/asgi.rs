use crate::Sender;
use crate::BoxError;
use futures::future::BoxFuture;
use http::status::StatusCode;
use pyo3::types::{PyBytes, PyDict, PyInt, PyString};
use pyo3::{
    exceptions::PyRuntimeError,
    prelude::*,
    types::{PyList, PySequence},
    DowncastError, DowncastIntoError,
};
use tower::Layer;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver},
    Mutex,
};

use bytes::Bytes;

use crate::http::{to_bytes, Body};

#[derive(Clone)]
pub struct ASGILayer {
    app: Arc<PyObject>,
    locals: Arc<pyo3_async_runtimes::TaskLocals>,
}

impl ASGILayer {
    pub fn new(asgi_service: AsgiService) -> Self {
        ASGILayer { 
            app: asgi_service.app,
            locals: asgi_service.locals,
         }
    }
}

impl<S> Layer<S> for ASGILayer {
    type Service = AsgiService;
    
    fn layer(&self, _service: S) -> Self::Service {
        AsgiService {
            app: self.app.clone(),
            locals: self.locals.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AsgiService {
    app: Arc<PyObject>,
    locals: Arc<pyo3_async_runtimes::TaskLocals>,
}

impl AsgiService {
    pub fn new_with_locals(
        app: Arc<PyObject>,
        locals: Arc<pyo3_async_runtimes::TaskLocals>,
    ) -> AsgiService {
        AsgiService { app, locals }
    }
}

#[derive(Debug)]
enum AsgiError {
    PyErr(PyErr),
    InvalidHttpVersion,
    ExpectedResponseStart,
    MissingResponse,
    ExpectedResponseBody,
    FailedToCreateResponse,
    InvalidHeader,
    InvalidUtf8InPath,
}

impl From<PyErr> for AsgiError {
    fn from(e: PyErr) -> Self {
        AsgiError::PyErr(e)
    }
}

impl<'a, 'b> From<DowncastError<'a, 'b>> for AsgiError {
    fn from(_e: DowncastError<'a, 'b>) -> Self {
        AsgiError::PyErr(PyErr::new::<PyRuntimeError, _>("failed to downcast type"))
    }
}

impl<'a> From<DowncastIntoError<'a>> for AsgiError {
    fn from(_e: DowncastIntoError<'a>) -> Self {
        AsgiError::PyErr(PyErr::new::<PyRuntimeError, _>("failed to downcast type"))
    }
}

impl AsgiError {
    fn into_response(self) -> http::Response<Body> {
        let (status, body) = match self {
            AsgiError::InvalidHttpVersion => (StatusCode::BAD_REQUEST, "Unsupported HTTP version"),
            AsgiError::InvalidUtf8InPath => (StatusCode::BAD_REQUEST, "Invalid Utf8 in path"),
            AsgiError::PyErr(_)
            | AsgiError::ExpectedResponseStart
            | AsgiError::MissingResponse
            | AsgiError::ExpectedResponseBody
            | AsgiError::FailedToCreateResponse
            | AsgiError::InvalidHeader => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
            }
        };

        http::Response::builder()
            .status(status)
            .body(Body::from(body))
            .unwrap_or_else(|_| {
                http::Response::builder()
                    .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("Failed to build error response"))
                    .unwrap()
            })
    }
}

/// Used to set the HttpReceiver's disconnected flag when the connection is closed
struct SetTrueOnDrop(Arc<AtomicBool>);

impl Drop for SetTrueOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[pyclass]
struct HttpReceiver {
    disconnected: Arc<AtomicBool>,
    rx: Arc<Mutex<UnboundedReceiver<Option<Body>>>>,
    locals: Arc<pyo3_async_runtimes::TaskLocals>,
}

#[pymethods]
impl HttpReceiver {
    fn __call__<'a>(&'a self, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let rx = self.rx.clone();
        let disconnected = self.disconnected.clone();
        pyo3_async_runtimes::tokio::future_into_py_with_locals(
            py,
            self.locals.clone_ref(py),
            async move {
                let next = rx.lock().await.recv().await;

                if next.is_none() || disconnected.load(Ordering::SeqCst) {
                    Python::with_gil(|py| {
                        let scope = PyDict::new(py);
                        scope.set_item("type", "http.disconnect")?;
                        Ok::<_, PyErr>(scope.into())
                    })
                } else if let Some(Some(body)) = next {
                    const MAX_BODY_SIZE: usize = 4 * 1024 * 1024; // 4MB
                    let bytes = to_bytes(body, MAX_BODY_SIZE)
                        .await
                        .map_err(|_e| PyErr::new::<PyRuntimeError, _>("failed to fetch data"))?;
                    Python::with_gil(|py| {
                        let bytes = PyBytes::new(py, &bytes[..]);
                        let scope = PyDict::new(py);
                        scope.set_item("type", "http.request")?;
                        scope.set_item("body", bytes)?;
                        let scope: Py<PyDict> = scope.into();
                        Ok::<_, PyErr>(scope)
                    })
                } else {
                    Python::with_gil(|py| {
                        let scope = PyDict::new(py);
                        scope.set_item("type", "http.request")?;
                        Ok::<_, PyErr>(scope.into())
                    })
                }
            },
        )
    }
}

impl<R> tower_service::Service<http::Request<R>> for AsgiService
where
    R: Into<Body> + Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = Box<dyn BoxError>;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The service is always ready to handle requests
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<R>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let app = this.app.clone();
            let (http_sender, mut http_sender_rx) = Sender::new(this.locals.clone());
            let disconnected = Arc::new(AtomicBool::new(false));
            let (receiver_tx, receiver_rx) = mpsc::unbounded_channel();
            let receiver = HttpReceiver {
                rx: Arc::new(Mutex::new(receiver_rx)),
                disconnected: disconnected.clone(),
                locals: this.locals.clone(),
            };

            // Convert request body to Body
            let (req_parts, body) = req.into_parts();
            let body: Body = body.into();
            let req = http::Request::from_parts(req_parts, body);
            let (req, body) = req.into_parts();

            receiver_tx.send(Some(body)).unwrap();
            let _disconnected = SetTrueOnDrop(disconnected);
            
            let result = match Python::with_gil(|py| {
                let asgi = PyDict::new(py);
                asgi.set_item("spec_version", "2.0")?;
                asgi.set_item("version", "2.0")?;
                let scope = PyDict::new(py);
                scope.set_item("type", "http")?;
                scope.set_item("asgi", asgi)?;
                scope.set_item(
                    "http_version",
                    match req.version {
                        http::Version::HTTP_10 => "1.0",
                        http::Version::HTTP_11 => "1.1",
                        http::Version::HTTP_2 => "2",
                        _ => return Err(AsgiError::InvalidHttpVersion),
                    },
                )?;
                scope.set_item("method", req.method.as_str())?;
                scope.set_item("scheme", req.uri.scheme_str().unwrap_or("http"))?;
                if let Some(path_and_query) = req.uri.path_and_query() {
                    let path = path_and_query.path();
                    let raw_path = path.as_bytes();
                    // the spec requires this to be percent decoded at this point
                    // https://asgi.readthedocs.io/en/latest/specs/www.html#http-connection-scope
                    let path = percent_encoding::percent_decode(raw_path)
                        .decode_utf8()
                        .map_err(|_| AsgiError::InvalidUtf8InPath)?;
                    scope.set_item("path", path)?;
                    let raw_path_bytes = PyBytes::new(py, path_and_query.path().as_bytes());
                    scope.set_item("raw_path", raw_path_bytes)?;
                    if let Some(query) = path_and_query.query() {
                        let qs_bytes = PyBytes::new(py, query.as_bytes());
                        scope.set_item("query_string", qs_bytes)?;
                    } else {
                        let qs_bytes = PyBytes::new(py, "".as_bytes());
                        scope.set_item("query_string", qs_bytes)?;
                    }
                } else {
                    // TODO: is it even possible to get here?
                    // we have to set these to something as they're not optional in the spec
                    scope.set_item("path", "")?;
                    let raw_path_bytes = PyBytes::new(py, "".as_bytes());
                    scope.set_item("raw_path", raw_path_bytes)?;
                    let qs_bytes = PyBytes::new(py, "".as_bytes());
                    scope.set_item("query_string", qs_bytes)?;
                }
                scope.set_item("root_path", "")?;

                let headers = req
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        let name_bytes = PyBytes::new(py, name.as_str().as_bytes());
                        let value_bytes = PyBytes::new(py, value.as_bytes());
                        // This unwrap() is safe because PyList::new only fails if there's a Python
                        // exception during list creation, which won't happen for a simple list of
                        // two PyBytes objects that were just successfully created
                        PyList::new(py, [name_bytes, value_bytes]).unwrap()
                    })
                    .collect::<Vec<_>>();
                // This unwrap() is safe because PyList::new only fails if there's a Python
                // exception during list creation, which won't happen for a simple list of
                // PyList objects that were already successfully created above
                let headers = PyList::new(py, headers).unwrap();
                scope.set_item("headers", headers)?;
                // TODO: client/server args
                let sender = Py::new(py, http_sender)?;
                let receiver = Py::new(py, receiver)?;
                let args = (scope, receiver, sender);
                let res = app.call_method1(py, "__call__", args)?;
                let fut = res.extract(py)?;
                let coro = pyo3_async_runtimes::into_future_with_locals(&this.locals, fut)?;
                Ok::<_, AsgiError>(coro)
            }) {
                Ok(http_coro) => {
                    tokio::spawn(async move {
                        if let Err(_e) = http_coro.await {
                            #[cfg(feature = "tracing")]
                            tracing::error!("error handling request: {_e}");
                        }
                    });

                    let mut response = http::Response::builder();

                    if let Some(resp) = http_sender_rx.recv().await {
                        let (status, headers) = match Python::with_gil(|py| {
                            let dict: Bound<'_, PyDict> = resp.into_bound(py);
                            if let Ok(Some(value)) = dict.get_item("type") {
                                let value: Bound<'_, PyString> = value.downcast_into()?;
                                let value = value.to_str()?;
                                if value == "http.response.start" {
                                    let value: Bound<'_, PyInt> = dict
                                        .get_item("status")
                                        .and_then(|opt| {
                                            opt.ok_or_else(|| {
                                                PyErr::new::<PyRuntimeError, _>(
                                                    "Missing status in http.response.start",
                                                )
                                            })
                                        })?
                                        .downcast_into()?;
                                    let status: u16 = value.extract()?;

                                    let headers = if let Ok(Some(raw)) = dict.get_item("headers") {
                                        let outer: Bound<'_, PySequence> = raw.downcast_into()?;
                                        Some(
                                            outer
                                                .try_iter()?
                                                .map(|item| {
                                                    item.and_then(|item| {
                                                        let seq: Bound<'_, PySequence> =
                                                            item.downcast_into()?;
                                                        let header: Vec<u8> =
                                                            seq.get_item(0)?.extract()?;
                                                        let value: Vec<u8> =
                                                            seq.get_item(1)?.extract()?;
                                                        Ok((header, value))
                                                    })
                                                })
                                                .collect::<PyResult<Vec<_>>>()?,
                                        )
                                    } else {
                                        None
                                    };
                                    Ok((status, headers))
                                } else {
                                    Err(AsgiError::ExpectedResponseStart)
                                }
                            } else {
                                Err(AsgiError::ExpectedResponseStart)
                            }
                        }) {
                            Ok((status, headers)) => (status, headers),
                            Err(e) => {
                                return Ok(e.into_response());
                            }
                        };
                        response = response.status(status);
                        if let Some(pyheaders) = headers {
                            let headers = response.headers_mut().unwrap();
                            for (name, value) in pyheaders {
                                let name = match http::HeaderName::from_bytes(&name) {
                                    Ok(name) => name,
                                    Err(_e) => {
                                        return Ok(AsgiError::InvalidHeader.into_response());
                                    }
                                };
                                let value = match http::HeaderValue::from_bytes(&value) {
                                    Ok(value) => value,
                                    Err(_e) => {
                                        return Ok(AsgiError::InvalidHeader.into_response());
                                    }
                                };
                                headers.append(name, value);
                            }
                        }
                    } else {
                        return Ok(AsgiError::MissingResponse.into_response());
                    }

                    let mut body = Vec::new();
                    while let Some(resp) = http_sender_rx.recv().await {
                        let (bytes, more_body) = match Python::with_gil(|py| {
                            let dict: Bound<'_, PyDict> = resp.into_bound(py);
                            if let Ok(Some(value)) = dict.get_item("type") {
                                let value: Bound<'_, PyString> =
                                    value.downcast_into().map_err(|_| {
                                        AsgiError::PyErr(PyErr::new::<PyRuntimeError, _>(
                                            "failed to downcast type",
                                        ))
                                    })?;
                                let value = value.to_str()?;
                                if value == "http.response.body" {
                                    let more_body =
                                        if let Ok(Some(raw)) = dict.get_item("more_body") {
                                            raw.extract::<bool>()?
                                        } else {
                                            false
                                        };
                                    if let Ok(Some(raw)) = dict.get_item("body") {
                                        Ok((raw.extract::<Vec<u8>>()?, more_body))
                                    } else {
                                        Ok((Vec::new(), more_body))
                                    }
                                } else {
                                    Err(AsgiError::ExpectedResponseBody)
                                }
                            } else {
                                Err(AsgiError::ExpectedResponseBody)
                            }
                        }) {
                            Ok((bytes, more_body)) => (bytes, more_body),
                            Err(e) => {
                                return Ok(e.into_response());
                            }
                        };
                        body.extend(bytes);
                        if !more_body {
                            break;
                        }
                    }

                    let body = Body::from(Bytes::from(body));
                    match response.body(body) {
                        Ok(response) => Ok(response),
                        Err(_e) => {
                            #[cfg(feature = "tracing")]
                            tracing::error!("Failed to create response: {_e}");
                            Ok(AsgiError::FailedToCreateResponse.into_response())
                        }
                    }
                }
                Err(e) => {
                    #[cfg(feature = "tracing")]
                    tracing::error!("Error preparing request scope: {e:?}");
                    Ok(e.into_response())
                }
            };
            
            result
        })
    }
}
