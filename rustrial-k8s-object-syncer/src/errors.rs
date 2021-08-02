/// Extension methods for Kubernetes API errors.
pub(crate) trait ExtKubeApiError {
    fn is_not_found(&self) -> bool;
    fn is_conflict(&self) -> bool;
}

impl ExtKubeApiError for kube::Error {
    fn is_not_found(&self) -> bool {
        match self {
            kube::Error::Api(e) if e.code == 404 || e.code == 410 => true,
            _ => false,
        }
    }

    fn is_conflict(&self) -> bool {
        match self {
            kube::Error::Api(e) if e.code == 409 => true,
            _ => false,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum ControllerError {
    /// Failed to discover API Group, Kind or preferred Version of a Kind.
    /// This might be a temporary failure caused by not yet installed or
    /// removed CustomResourceDefinitions (CRDs) or by temporary API connectivity
    /// issues.
    #[error("{0}")]
    ApiDiscoveryError(String),
    /// Cluster scoped resources cannot be synced.
    #[error("{0}")]
    ClusterScopedResource(String),
    /// Failed to remove some destinations
    #[error("{0}")]
    DestinationRemovalError(String),
    /// Kubernetes API error
    #[error("{0}")]
    KubeApi(#[from] kube::Error),
    /// Serialization errors
    #[error("{0}")]
    Serde(#[from] serde_json::Error),
    /// Any other kind of errors
    #[error("{0}")]
    Any(#[from] anyhow::Error),
}

impl ControllerError {
    pub(crate) fn is_temporary(&self) -> bool {
        match self {
            ControllerError::ClusterScopedResource(_) => false,
            _ => true,
        }
    }
}
