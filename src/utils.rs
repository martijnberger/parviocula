use tower::ServiceBuilder;

/// Creates a ServiceBuilder that can be used to add middleware to the AsgiHandler
///
/// This is useful for adding tower middleware to the ASGI handler.
pub fn create_asgi_service_builder() -> ServiceBuilder<tower::layer::util::Identity> {
    ServiceBuilder::new()
}
