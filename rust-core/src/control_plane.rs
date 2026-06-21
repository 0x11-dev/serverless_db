use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

pub trait ControlPlane {
    fn upsert_project(&self, project: ProjectCatalogEntry) -> Result<(), ControlPlaneError>;
    fn project(&self, project_id: &str) -> Result<ProjectCatalogEntry, ControlPlaneError>;
    fn register_api_key(
        &self,
        api_key: impl Into<String>,
        record: ApiKeyRecord,
    ) -> Result<(), ControlPlaneError>;
    fn resolve_api_key(&self, api_key: &str) -> Result<ApiKeyRoute, ControlPlaneError>;
    fn register_worker(&self, endpoint: WorkerEndpoint) -> Result<(), ControlPlaneError>;
    fn set_worker_health(&self, worker_id: &str, healthy: bool) -> Result<(), ControlPlaneError>;
    fn decide_placement(
        &self,
        request: PlacementRequest,
    ) -> Result<PlacementDecision, ControlPlaneError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCatalogEntry {
    pub project_id: String,
    pub object_store_prefix: String,
    pub default_region: Option<String>,
    pub serving_state: ProjectServingState,
}

impl ProjectCatalogEntry {
    pub fn active(
        project_id: impl Into<String>,
        object_store_prefix: impl Into<String>,
        default_region: Option<String>,
        primary_worker_id: impl Into<String>,
        replica_worker_ids: Vec<String>,
    ) -> Self {
        Self {
            project_id: project_id.into(),
            object_store_prefix: object_store_prefix.into(),
            default_region,
            serving_state: ProjectServingState::Active {
                primary_worker_id: primary_worker_id.into(),
                replica_worker_ids,
            },
        }
    }

    pub fn cold(
        project_id: impl Into<String>,
        object_store_prefix: impl Into<String>,
        default_region: Option<String>,
        wakeup_region: Option<String>,
    ) -> Self {
        Self {
            project_id: project_id.into(),
            object_store_prefix: object_store_prefix.into(),
            default_region,
            serving_state: ProjectServingState::Cold { wakeup_region },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectServingState {
    Active {
        primary_worker_id: String,
        replica_worker_ids: Vec<String>,
    },
    Cold {
        wakeup_region: Option<String>,
    },
}

impl ProjectServingState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    pub fn is_cold(&self) -> bool {
        matches!(self, Self::Cold { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub project_id: String,
    pub key_id: String,
    pub kind: ApiKeyKind,
}

impl ApiKeyRecord {
    pub fn anon(project_id: impl Into<String>, key_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            key_id: key_id.into(),
            kind: ApiKeyKind::Anon,
        }
    }

    pub fn service_role(project_id: impl Into<String>, key_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            key_id: key_id.into(),
            kind: ApiKeyKind::ServiceRole,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiKeyKind {
    Anon,
    ServiceRole,
}

impl ApiKeyKind {
    pub fn is_service_role(self) -> bool {
        matches!(self, Self::ServiceRole)
    }

    pub fn is_browser_safe(self) -> bool {
        matches!(self, Self::Anon)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyRoute {
    pub project_id: String,
    pub key_id: String,
    pub kind: ApiKeyKind,
    pub service_role: bool,
    pub browser_safe: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerEndpoint {
    pub worker_id: String,
    pub region: Option<String>,
    pub url: String,
    pub healthy: bool,
    pub can_start_cold_project: bool,
}

impl WorkerEndpoint {
    pub fn new(
        worker_id: impl Into<String>,
        region: Option<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            region,
            url: url.into(),
            healthy: true,
            can_start_cold_project: true,
        }
    }

    pub fn unhealthy(mut self) -> Self {
        self.healthy = false;
        self
    }

    pub fn without_cold_start(mut self) -> Self {
        self.can_start_cold_project = false;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRequest {
    pub project_id: String,
    pub intent: PlacementIntent,
    pub preferred_region: Option<String>,
}

impl PlacementRequest {
    pub fn read(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            intent: PlacementIntent::Read,
            preferred_region: None,
        }
    }

    pub fn write(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            intent: PlacementIntent::Write,
            preferred_region: None,
        }
    }

    pub fn preferred_region(mut self, region: impl Into<String>) -> Self {
        self.preferred_region = Some(region.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlacementIntent {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementDecision {
    pub project_id: String,
    pub project_state: ProjectServingState,
    pub action: PlacementAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlacementAction {
    Route {
        endpoint: WorkerEndpoint,
        role: PlacementRole,
    },
    WakeProject {
        endpoint: WorkerEndpoint,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlacementRole {
    Primary,
    Replica,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPlaneError {
    ProjectNotFound(String),
    ApiKeyNotFound,
    WorkerNotFound(String),
    NoHealthyWorker { project_id: String },
}

impl fmt::Display for ControlPlaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectNotFound(project_id) => write!(f, "project not found: {project_id}"),
            Self::ApiKeyNotFound => write!(f, "api key not found"),
            Self::WorkerNotFound(worker_id) => write!(f, "worker not found: {worker_id}"),
            Self::NoHealthyWorker { project_id } => {
                write!(f, "no healthy worker available for project: {project_id}")
            }
        }
    }
}

impl std::error::Error for ControlPlaneError {}

#[derive(Debug, Clone, Default)]
pub struct InMemoryControlPlane {
    state: Arc<Mutex<ControlPlaneState>>,
}

#[derive(Debug, Default)]
struct ControlPlaneState {
    projects: BTreeMap<String, ProjectCatalogEntry>,
    api_keys: BTreeMap<String, ApiKeyRecord>,
    workers: BTreeMap<String, WorkerEndpoint>,
}

impl InMemoryControlPlane {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ControlPlane for InMemoryControlPlane {
    fn upsert_project(&self, project: ProjectCatalogEntry) -> Result<(), ControlPlaneError> {
        self.state
            .lock()
            .unwrap()
            .projects
            .insert(project.project_id.clone(), project);
        Ok(())
    }

    fn project(&self, project_id: &str) -> Result<ProjectCatalogEntry, ControlPlaneError> {
        self.state
            .lock()
            .unwrap()
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::ProjectNotFound(project_id.to_string()))
    }

    fn register_api_key(
        &self,
        api_key: impl Into<String>,
        record: ApiKeyRecord,
    ) -> Result<(), ControlPlaneError> {
        self.project(&record.project_id)?;
        self.state
            .lock()
            .unwrap()
            .api_keys
            .insert(api_key.into(), record);
        Ok(())
    }

    fn resolve_api_key(&self, api_key: &str) -> Result<ApiKeyRoute, ControlPlaneError> {
        let state = self.state.lock().unwrap();
        let record = state
            .api_keys
            .get(api_key)
            .ok_or(ControlPlaneError::ApiKeyNotFound)?;
        if !state.projects.contains_key(&record.project_id) {
            return Err(ControlPlaneError::ProjectNotFound(
                record.project_id.clone(),
            ));
        }
        Ok(ApiKeyRoute {
            project_id: record.project_id.clone(),
            key_id: record.key_id.clone(),
            kind: record.kind,
            service_role: record.kind.is_service_role(),
            browser_safe: record.kind.is_browser_safe(),
        })
    }

    fn register_worker(&self, endpoint: WorkerEndpoint) -> Result<(), ControlPlaneError> {
        self.state
            .lock()
            .unwrap()
            .workers
            .insert(endpoint.worker_id.clone(), endpoint);
        Ok(())
    }

    fn set_worker_health(&self, worker_id: &str, healthy: bool) -> Result<(), ControlPlaneError> {
        let mut state = self.state.lock().unwrap();
        let endpoint = state
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| ControlPlaneError::WorkerNotFound(worker_id.to_string()))?;
        endpoint.healthy = healthy;
        Ok(())
    }

    fn decide_placement(
        &self,
        request: PlacementRequest,
    ) -> Result<PlacementDecision, ControlPlaneError> {
        let state = self.state.lock().unwrap();
        let project = state
            .projects
            .get(&request.project_id)
            .ok_or_else(|| ControlPlaneError::ProjectNotFound(request.project_id.clone()))?;
        let action =
            match &project.serving_state {
                ProjectServingState::Active {
                    primary_worker_id,
                    replica_worker_ids,
                } => match request.intent {
                    PlacementIntent::Write => {
                        let endpoint = healthy_worker(&state.workers, primary_worker_id)
                            .ok_or_else(|| ControlPlaneError::NoHealthyWorker {
                                project_id: project.project_id.clone(),
                            })?;
                        PlacementAction::Route {
                            endpoint,
                            role: PlacementRole::Primary,
                        }
                    }
                    PlacementIntent::Read => {
                        if let Some(endpoint) = choose_worker(
                            &state.workers,
                            replica_worker_ids,
                            request.preferred_region.as_deref(),
                        ) {
                            PlacementAction::Route {
                                endpoint,
                                role: PlacementRole::Replica,
                            }
                        } else {
                            let endpoint = healthy_worker(&state.workers, primary_worker_id)
                                .ok_or_else(|| ControlPlaneError::NoHealthyWorker {
                                    project_id: project.project_id.clone(),
                                })?;
                            PlacementAction::Route {
                                endpoint,
                                role: PlacementRole::Primary,
                            }
                        }
                    }
                },
                ProjectServingState::Cold { wakeup_region } => {
                    let preferred_region = request
                        .preferred_region
                        .as_deref()
                        .or(wakeup_region.as_deref())
                        .or(project.default_region.as_deref());
                    let endpoint = choose_cold_start_worker(&state.workers, preferred_region)
                        .ok_or_else(|| ControlPlaneError::NoHealthyWorker {
                            project_id: project.project_id.clone(),
                        })?;
                    PlacementAction::WakeProject { endpoint }
                }
            };
        Ok(PlacementDecision {
            project_id: project.project_id.clone(),
            project_state: project.serving_state.clone(),
            action,
        })
    }
}

fn healthy_worker(
    workers: &BTreeMap<String, WorkerEndpoint>,
    worker_id: &str,
) -> Option<WorkerEndpoint> {
    workers
        .get(worker_id)
        .filter(|endpoint| endpoint.healthy)
        .cloned()
}

fn choose_worker(
    workers: &BTreeMap<String, WorkerEndpoint>,
    worker_ids: &[String],
    preferred_region: Option<&str>,
) -> Option<WorkerEndpoint> {
    let endpoints = worker_ids
        .iter()
        .filter_map(|worker_id| healthy_worker(workers, worker_id))
        .collect::<Vec<_>>();
    choose_endpoint(endpoints, preferred_region)
}

fn choose_cold_start_worker(
    workers: &BTreeMap<String, WorkerEndpoint>,
    preferred_region: Option<&str>,
) -> Option<WorkerEndpoint> {
    let endpoints = workers
        .values()
        .filter(|endpoint| endpoint.healthy && endpoint.can_start_cold_project)
        .cloned()
        .collect::<Vec<_>>();
    choose_endpoint(endpoints, preferred_region)
}

fn choose_endpoint(
    endpoints: Vec<WorkerEndpoint>,
    preferred_region: Option<&str>,
) -> Option<WorkerEndpoint> {
    preferred_region
        .filter(|region| !region.is_empty())
        .and_then(|region| {
            endpoints
                .iter()
                .find(|endpoint| endpoint.region.as_deref() == Some(region))
                .cloned()
        })
        .or_else(|| endpoints.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_plane() -> InMemoryControlPlane {
        let control = InMemoryControlPlane::new();
        control
            .upsert_project(ProjectCatalogEntry::cold(
                "demo",
                "projects/demo",
                Some("iad".to_string()),
                Some("iad".to_string()),
            ))
            .unwrap();
        control
    }

    #[test]
    fn resolves_anon_and_service_role_keys_to_projects() {
        let control = control_plane();
        control
            .register_api_key("anon-demo", ApiKeyRecord::anon("demo", "anon-key"))
            .unwrap();
        control
            .register_api_key(
                "service-demo",
                ApiKeyRecord::service_role("demo", "service-key"),
            )
            .unwrap();

        let anon = control.resolve_api_key("anon-demo").unwrap();
        assert_eq!(anon.project_id, "demo");
        assert_eq!(anon.kind, ApiKeyKind::Anon);
        assert!(!anon.service_role);
        assert!(anon.browser_safe);

        let service = control.resolve_api_key("service-demo").unwrap();
        assert_eq!(service.project_id, "demo");
        assert_eq!(service.kind, ApiKeyKind::ServiceRole);
        assert!(service.service_role);
        assert!(!service.browser_safe);
    }

    #[test]
    fn chooses_replica_for_regional_reads_and_primary_for_writes() {
        let control = InMemoryControlPlane::new();
        control
            .register_worker(WorkerEndpoint::new(
                "worker-primary",
                Some("iad".to_string()),
                "http://primary:8765",
            ))
            .unwrap();
        control
            .register_worker(WorkerEndpoint::new(
                "worker-replica-a",
                Some("sfo".to_string()),
                "http://replica-a:8765",
            ))
            .unwrap();
        control
            .register_worker(WorkerEndpoint::new(
                "worker-replica-b",
                Some("iad".to_string()),
                "http://replica-b:8765",
            ))
            .unwrap();
        control
            .upsert_project(ProjectCatalogEntry::active(
                "demo",
                "projects/demo",
                Some("iad".to_string()),
                "worker-primary",
                vec![
                    "worker-replica-a".to_string(),
                    "worker-replica-b".to_string(),
                ],
            ))
            .unwrap();

        let read = control
            .decide_placement(PlacementRequest::read("demo").preferred_region("iad"))
            .unwrap();
        assert_eq!(
            read.action,
            PlacementAction::Route {
                endpoint: WorkerEndpoint::new(
                    "worker-replica-b",
                    Some("iad".to_string()),
                    "http://replica-b:8765",
                ),
                role: PlacementRole::Replica,
            }
        );

        let write = control
            .decide_placement(PlacementRequest::write("demo").preferred_region("sfo"))
            .unwrap();
        assert_eq!(
            write.action,
            PlacementAction::Route {
                endpoint: WorkerEndpoint::new(
                    "worker-primary",
                    Some("iad".to_string()),
                    "http://primary:8765",
                ),
                role: PlacementRole::Primary,
            }
        );
    }

    #[test]
    fn expresses_active_and_cold_project_states() {
        let control = InMemoryControlPlane::new();
        control
            .register_worker(WorkerEndpoint::new(
                "worker-wakeup",
                Some("iad".to_string()),
                "http://worker-wakeup:8765",
            ))
            .unwrap();
        control
            .upsert_project(ProjectCatalogEntry::cold(
                "cold-project",
                "projects/cold-project",
                Some("iad".to_string()),
                Some("iad".to_string()),
            ))
            .unwrap();
        control
            .upsert_project(ProjectCatalogEntry::active(
                "active-project",
                "projects/active-project",
                Some("iad".to_string()),
                "worker-wakeup",
                Vec::new(),
            ))
            .unwrap();

        let cold = control.project("cold-project").unwrap();
        assert!(cold.serving_state.is_cold());
        let cold_decision = control
            .decide_placement(PlacementRequest::read("cold-project"))
            .unwrap();
        assert_eq!(
            cold_decision.action,
            PlacementAction::WakeProject {
                endpoint: WorkerEndpoint::new(
                    "worker-wakeup",
                    Some("iad".to_string()),
                    "http://worker-wakeup:8765",
                )
            }
        );

        let active = control.project("active-project").unwrap();
        assert!(active.serving_state.is_active());
        let active_decision = control
            .decide_placement(PlacementRequest::read("active-project"))
            .unwrap();
        assert_eq!(
            active_decision.action,
            PlacementAction::Route {
                endpoint: WorkerEndpoint::new(
                    "worker-wakeup",
                    Some("iad".to_string()),
                    "http://worker-wakeup:8765",
                ),
                role: PlacementRole::Primary,
            }
        );
    }
}
