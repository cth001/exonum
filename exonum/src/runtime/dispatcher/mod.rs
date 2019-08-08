// Copyright 2019 The Exonum Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub use self::{error::Error, schema::Schema};

use exonum_merkledb::{Fork, IndexAccess, Snapshot};
use futures::{future, Future};

use std::{cell::RefCell, collections::HashMap, panic};

use crate::{
    api::ServiceApiBuilder,
    blockchain::{FatalError, IndexCoordinates, IndexOwner},
    crypto::{Hash, PublicKey, SecretKey},
    helpers::ValidateInput,
    messages::{AnyTx, Verified},
    node::ApiSender,
    proto::Any,
};

use super::{
    error::{catch_panic, ExecutionError},
    ArtifactId, ArtifactInfo, CallInfo, Caller, ExecutionContext, InstanceSpec, Runtime,
    ServiceInstanceId,
};

mod error;
mod schema;

/// Max instance identifier for builtin service.
///
/// By analogy with network's privileged ports, we use a range 0..1023 of instance identifiers
/// for built in services which can be created only during the blockchain genesis block creation.
pub const MAX_BUILTIN_INSTANCE_ID: ServiceInstanceId = 1024;

#[derive(Default)]
pub struct Dispatcher {
    runtimes: HashMap<u32, Box<dyn Runtime>>,
    runtime_lookup: HashMap<ServiceInstanceId, u32>,
    modified: Option<()>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("runtimes", &self.runtimes)
            .finish()
    }
}

impl Dispatcher {
    /// Creates a new dispatcher with the specified runtimes.
    pub(crate) fn with_runtimes(
        runtimes: impl IntoIterator<Item = (u32, Box<dyn Runtime>)>,
    ) -> Self {
        Self {
            runtimes: runtimes.into_iter().collect(),
            runtime_lookup: HashMap::default(),
            modified: None,
        }
    }

    /// Restores dispatcher from the state which saved in the specified snapshot.
    pub(crate) fn restore_state(
        &mut self,
        snapshot: impl IndexAccess,
    ) -> Result<(), ExecutionError> {
        let schema = Schema::new(snapshot);
        // Restores information about the deployed services.
        for (artifact, spec) in schema.artifacts_with_spec() {
            self.deploy_artifact(artifact.clone(), spec).wait()?;
        }
        // Restarts active service instances.
        for instance in schema.service_instances().values() {
            self.restart_service(&instance)?;
        }
        Ok(())
    }

    /// Adds built-in service with predefined identifier.
    ///
    /// # Panics
    ///
    /// * If instance spec contains invalid service name or artifact id.
    /// * If instance id is greater than [`MAX_BUILTIN_INSTANCE_ID`]
    ///
    /// [`MAX_BUILTIN_INSTANCE_ID`]: constant.MAX_BUILTIN_INSTANCE_ID.html
    pub(crate) fn add_builtin_service(
        &mut self,
        fork: &Fork,
        spec: InstanceSpec,
        constructor: Any,
    ) -> Result<(), ExecutionError> {
        assert!(
            spec.id < MAX_BUILTIN_INSTANCE_ID,
            "Instance identifier for builtin service should be lesser than {}",
            MAX_BUILTIN_INSTANCE_ID
        );
        // Built-in services should not have an additional specification.
        let artifact_spec = Any::default();
        // Register service artifact in the runtime.
        self.deploy_and_register_artifact(fork, &spec.artifact, artifact_spec)?;
        // Start the built-in service instance.
        self.start_service(fork, spec, constructor)
    }

    pub(crate) fn state_hash(
        &self,
        access: &dyn Snapshot,
    ) -> impl IntoIterator<Item = (IndexCoordinates, Hash)> {
        let mut aggregator = HashMap::new();
        aggregator.extend(
            // Inserts state hashes for the dispatcher.
            IndexCoordinates::locate(IndexOwner::Dispatcher, Schema::new(access).state_hash()),
        );
        // Inserts state hashes for the runtimes.
        for (runtime_id, runtime) in &self.runtimes {
            let state = runtime.state_hashes(access);
            aggregator.extend(
                // Runtime state hash.
                IndexCoordinates::locate(IndexOwner::Runtime(*runtime_id), state.runtime),
            );
            for (instance_id, instance_hashes) in state.instances {
                aggregator.extend(
                    // Instance state hashes.
                    IndexCoordinates::locate(IndexOwner::Service(instance_id), instance_hashes),
                );
            }
        }
        aggregator
    }

    pub(crate) fn services_api(&self) -> Vec<(String, ServiceApiBuilder)> {
        self.runtimes
            .iter()
            .fold(Vec::new(), |mut api, (_, runtime)| {
                api.append(&mut runtime.services_api());
                api
            })
    }

    /// Initiates deploy artifact procedure in the corresponding runtime.
    ///
    /// # Panics
    ///
    /// * If artifact identifier is invalid.
    pub(crate) fn deploy_artifact(
        &mut self,
        artifact: ArtifactId,
        spec: impl Into<Any>,
    ) -> Box<dyn Future<Item = (), Error = ExecutionError>> {
        debug_assert!(artifact.validate().is_ok());

        if let Some(runtime) = self.runtimes.get_mut(&artifact.runtime_id) {
            runtime.deploy_artifact(artifact, spec.into())
        } else {
            Box::new(future::err(Error::IncorrectRuntime.into()))
        }
    }

    /// Registers deployed artifact in the dispatcher's information schema.
    /// Make sure that you successfully complete the deploy artifact procedure.
    ///
    /// # Panics
    ///
    /// * If artifact identifier is invalid.
    /// * If artifact was not deployed.
    pub(crate) fn register_artifact(
        &mut self,
        fork: &Fork,
        artifact: &ArtifactId,
        spec: impl Into<Any>,
    ) -> Result<(), ExecutionError> {
        debug_assert!(artifact.validate().is_ok(), "{:?}", artifact.validate());
        debug_assert!(
            self.is_deployed(&artifact),
            "An attempt to register artifact which is not be deployed: {:?}",
            artifact
        );

        Schema::new(fork).add_artifact(artifact, spec.into())?;
        info!(
            "Registered artifact {} in runtime with id {}",
            artifact.name, artifact.runtime_id
        );
        Ok(())
    }

    pub(crate) fn deploy_and_register_artifact(
        &mut self,
        fork: &Fork,
        artifact: &ArtifactId,
        spec: impl Into<Any>,
    ) -> Result<(), ExecutionError> {
        let spec = spec.into();
        self.deploy_artifact(artifact.clone(), spec.clone())
            .wait()?;
        self.register_artifact(fork, &artifact, spec)
    }

    /// Start and configure a new service instance. After that, write the information about the
    /// service instance to the dispatcher's information schema.
    ///
    /// # Panics
    ///
    /// * If instance spec contains invalid service name.
    pub(crate) fn start_service(
        &mut self,
        fork: &Fork,
        spec: InstanceSpec,
        constructor: Any,
    ) -> Result<(), ExecutionError> {
        debug_assert!(spec.validate().is_ok(), "{:?}", spec.validate());

        // Check that service doesn't use existing identifiers.
        if self.runtime_lookup.contains_key(&spec.id) {
            return Err(Error::ServiceIdExists.into());
        }
        // Try to start and configure the service instance.
        self.runtimes
            .get_mut(&spec.artifact.runtime_id)
            .ok_or(Error::IncorrectRuntime)
            .map_err(ExecutionError::from)
            .and_then(|runtime| {
                runtime.start_service(&spec)?;
                // Try to configure the started instance of the service, otherwise stop.
                Self::configure_service(runtime.as_ref(), fork, &spec, constructor).map_err(|e| {
                    error!(
                        "An error occurred while configuring the service {}: {}",
                        spec.name, e
                    );
                    if let Err(e) = runtime.stop_service(&spec) {
                        panic!(FatalError::new(e.to_string()))
                    }
                    e
                })
            })?;
        self.register_running_service(&spec);
        // Add service instance to the dispatcher schema.
        Schema::new(fork).add_service_instance(spec)?;
        Ok(())
    }

    // TODO documentation [ECR-3275]
    pub(crate) fn execute(
        &mut self,
        fork: &Fork,
        tx_id: Hash,
        tx: &Verified<AnyTx>,
    ) -> Result<(), ExecutionError> {
        let mut context = ExecutionContext::new(
            fork,
            Caller::Transaction {
                author: tx.author(),
                hash: tx_id,
            },
        );
        self.call(&mut context, tx.as_ref().call_info, &tx.as_ref().arguments)?;

        let actions = context.take_actions();
        // Mark the dispatcher as modified if actions are not empty.
        let is_modified = !actions.is_empty();
        // Execute pending dispatcher actions.
        for action in actions {
            action.execute(self, fork)?;
        }

        if is_modified {
            self.mark_as_modified();
        }
        Ok(())
    }

    /// Calls the corresponding runtime method.
    pub(crate) fn call(
        &self,
        context: &mut ExecutionContext,
        call_info: CallInfo,
        arguments: &[u8],
    ) -> Result<(), ExecutionError> {
        let runtime_id = self
            .runtime_lookup
            .get(&call_info.instance_id)
            .ok_or(Error::IncorrectInstanceId)?;

        let runtime = self
            .runtimes
            .get(&runtime_id)
            .ok_or(Error::IncorrectRuntime)?;

        runtime.execute(self, context, call_info, arguments)
    }

    pub(crate) fn before_commit(&self, fork: &mut Fork) {
        for runtime in self.runtimes.values() {
            runtime.before_commit(self, fork);
        }
    }

    pub(crate) fn after_commit(
        &mut self,
        snapshot: impl AsRef<dyn Snapshot>,
        service_keypair: &(PublicKey, SecretKey),
        tx_sender: &ApiSender,
    ) {
        let channel = DispatcherSender::new();
        self.runtimes.values().for_each(|runtime| {
            runtime.after_commit(&channel, snapshot.as_ref(), &service_keypair, &tx_sender)
        });

        for request in channel.take_deploy_requests() {
            match self
                .deploy_artifact(request.artifact.clone(), request.spec.clone())
                .wait()
            {
                Ok(_) => request.completed(),
                Err(e) => warn!(
                    "An error during deploy artifact {:?} occurred {:?}",
                    request.artifact, e
                ),
            }
        }
    }

    // Try to configure the started instance of the service and catch panic if it occurs.
    pub(crate) fn configure_service(
        runtime: &(dyn Runtime + 'static),
        fork: &Fork,
        spec: &InstanceSpec,
        constructor: Any,
    ) -> Result<(), ExecutionError> {
        catch_panic(|| runtime.configure_service(fork, &spec, constructor))
    }

    /// Return additional information about the artifact if it is deployed.
    pub(crate) fn artifact_info(&self, id: &ArtifactId) -> Option<ArtifactInfo> {
        self.runtimes.get(&id.runtime_id)?.artifact_info(id)
    }

    /// Return true if the artifact with the given identifier is deployed.
    pub(crate) fn is_deployed(&self, id: &ArtifactId) -> bool {
        self.artifact_info(id).is_some()
    }

    /// Take the modified state and mark the dispatcher as unmodified.
    pub(crate) fn take_modified_state(&mut self) -> bool {
        self.modified.take().is_some()
    }

    /// Mark the dispatcher as modified.
    fn mark_as_modified(&mut self) {
        trace!("Dispatcher state is modified");
        self.modified = Some(());
    }

    /// Register the service instance in the runtime lookup table.
    fn register_running_service(&mut self, instance: &InstanceSpec) {
        info!("Running service instance {:?}", instance);
        self.runtime_lookup
            .insert(instance.id, instance.artifact.runtime_id);
    }

    /// Start a new service instance.
    fn restart_service(&mut self, instance: &InstanceSpec) -> Result<(), ExecutionError> {
        let runtime = self
            .runtimes
            .get_mut(&instance.artifact.runtime_id)
            .ok_or(Error::IncorrectRuntime)?;
        runtime.start_service(instance)?;
        self.register_running_service(&instance);
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum Action {
    /// This action registers the deployed artifact in the dispatcher.
    /// Make sure that you successfully complete the deploy artifact procedure.
    RegisterArtifact { artifact: ArtifactId, spec: Any },
    /// This action starts the service instance with the specified params.
    /// Make sure that the artifact is deployed.
    StartService {
        artifact: ArtifactId,
        instance_name: String,
        config: Any,
    },
}

impl Action {
    fn execute(self, dispatcher: &mut Dispatcher, fork: &Fork) -> Result<(), ExecutionError> {
        match self {
            Action::RegisterArtifact { artifact, spec } => dispatcher
                .register_artifact(fork, &artifact, spec)
                .map_err(From::from),

            Action::StartService {
                artifact,
                instance_name,
                config,
            } => dispatcher
                .start_service(
                    fork,
                    InstanceSpec {
                        artifact,
                        name: instance_name,
                        id: Schema::new(fork).assign_instance_id(),
                    },
                    config,
                )
                .map_err(From::from),
        }
    }
}

struct DeployArtifactRequest {
    artifact: ArtifactId,
    spec: Any,
    /// The operation to be performed if this request was successfully processed.
    and_then: Box<dyn FnOnce() + 'static>,
}

impl DeployArtifactRequest {
    /// Invokes request callback.
    fn completed(self) {
        (self.and_then)();
    }
}

impl std::fmt::Debug for DeployArtifactRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DeployArtifactRequest")
            .field(&self.artifact)
            .finish()
    }
}

// TODO Implement proper pending deploy logic [ECR-3291]

/// Channel to communicate with the dispatcher.
#[derive(Debug)]
pub struct DispatcherSender {
    deploy_request: RefCell<Vec<DeployArtifactRequest>>,
}

impl DispatcherSender {
    /// Creates a new instance.
    fn new() -> Self {
        Self {
            deploy_request: RefCell::default(),
        }
    }

    /// Requests an artifact deployment and invokes the callback if the deployment
    /// was successfully completed.
    pub(super) fn request_deploy_artifact<F>(&self, artifact: ArtifactId, spec: Any, and_then: F)
    where
        F: FnOnce() + 'static,
    {
        self.deploy_request
            .borrow_mut()
            .push(DeployArtifactRequest {
                artifact,
                spec,
                and_then: Box::new(and_then),
            })
    }

    /// Takes requests from this channel.
    fn take_deploy_requests(self) -> Vec<DeployArtifactRequest> {
        self.deploy_request.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use exonum_merkledb::{Database, TemporaryDB};
    use futures::IntoFuture;

    use crate::{
        crypto::PublicKey,
        runtime::{
            rust::{Error as RustRuntimeError, RustRuntime},
            ArtifactInfo, MethodId, RuntimeIdentifier, ServiceInstanceId, StateHashAggregator,
        },
    };

    use super::*;

    enum SampleRuntimes {
        First = 2,
        Second = 3,
    }

    #[derive(Debug)]
    pub struct DispatcherBuilder {
        dispatcher: Dispatcher,
    }

    impl DispatcherBuilder {
        fn new() -> Self {
            Self {
                dispatcher: Dispatcher::default(),
            }
        }

        /// Adds additional runtime.
        fn with_runtime(mut self, id: u32, runtime: impl Into<Box<dyn Runtime>>) -> Self {
            self.dispatcher.runtimes.insert(id, runtime.into());
            self
        }

        fn finalize(self) -> Dispatcher {
            self.dispatcher
        }
    }

    impl Default for DispatcherBuilder {
        fn default() -> Self {
            Self::new()
        }
    }

    #[derive(Debug)]
    struct SampleRuntime {
        runtime_type: u32,
        instance_id: ServiceInstanceId,
        method_id: MethodId,
    }

    #[derive(Debug, IntoExecutionError)]
    #[exonum(crate = "crate")]
    enum SampleError {
        Foo = 15,
    }

    impl SampleRuntime {
        fn new(runtime_type: u32, instance_id: ServiceInstanceId, method_id: MethodId) -> Self {
            Self {
                runtime_type,
                instance_id,
                method_id,
            }
        }
    }

    impl Runtime for SampleRuntime {
        fn deploy_artifact(
            &mut self,
            artifact: ArtifactId,
            _spec: Any,
        ) -> Box<dyn Future<Item = (), Error = ExecutionError>> {
            Box::new(
                if artifact.runtime_id == self.runtime_type {
                    Ok(())
                } else {
                    Err(Error::IncorrectRuntime.into())
                }
                .into_future(),
            )
        }

        fn start_service(&mut self, spec: &InstanceSpec) -> Result<(), ExecutionError> {
            if spec.artifact.runtime_id == self.runtime_type {
                Ok(())
            } else {
                Err(Error::IncorrectRuntime.into())
            }
        }

        fn stop_service(&mut self, spec: &InstanceSpec) -> Result<(), ExecutionError> {
            if spec.artifact.runtime_id == self.runtime_type {
                Ok(())
            } else {
                Err(Error::IncorrectRuntime.into())
            }
        }

        fn configure_service(
            &self,
            _fork: &Fork,
            spec: &InstanceSpec,
            _parameters: Any,
        ) -> Result<(), ExecutionError> {
            if spec.artifact.runtime_id == self.runtime_type {
                Ok(())
            } else {
                Err(Error::IncorrectRuntime.into())
            }
        }

        fn execute(
            &self,
            _: &Dispatcher,
            _: &mut ExecutionContext,
            call_info: CallInfo,
            _: &[u8],
        ) -> Result<(), ExecutionError> {
            if call_info.instance_id == self.instance_id && call_info.method_id == self.method_id {
                Ok(())
            } else {
                Err(SampleError::Foo.into())
            }
        }

        fn state_hashes(&self, _snapshot: &dyn Snapshot) -> StateHashAggregator {
            StateHashAggregator::default()
        }

        fn before_commit(&self, _dispatcher: &Dispatcher, _fork: &mut Fork) {}

        fn after_commit(
            &self,
            _dispatcher: &DispatcherSender,
            _snapshot: &dyn Snapshot,
            _service_keypair: &(PublicKey, SecretKey),
            _tx_sender: &ApiSender,
        ) {
        }

        fn artifact_info(&self, _id: &ArtifactId) -> Option<ArtifactInfo> {
            Some(ArtifactInfo::default())
        }
    }

    #[test]
    fn test_builder() {
        let runtime_a = SampleRuntime::new(SampleRuntimes::First as u32, 0, 0);
        let runtime_b = SampleRuntime::new(SampleRuntimes::Second as u32, 1, 0);

        let dispatcher = DispatcherBuilder::new()
            .with_runtime(runtime_a.runtime_type, runtime_a)
            .with_runtime(runtime_b.runtime_type, runtime_b)
            .finalize();

        assert!(dispatcher
            .runtimes
            .get(&(SampleRuntimes::First as u32))
            .is_some());
        assert!(dispatcher
            .runtimes
            .get(&(SampleRuntimes::Second as u32))
            .is_some());
    }

    #[test]
    fn test_dispatcher_simple() {
        const RUST_SERVICE_ID: ServiceInstanceId = 2;
        const JAVA_SERVICE_ID: ServiceInstanceId = 3;
        const RUST_SERVICE_NAME: &str = "rust-service";
        const JAVA_SERVICE_NAME: &str = "java-service";
        const RUST_METHOD_ID: MethodId = 0;
        const JAVA_METHOD_ID: MethodId = 1;

        // Create dispatcher and test data.
        let db = TemporaryDB::new();

        let runtime_a = SampleRuntime::new(
            SampleRuntimes::First as u32,
            RUST_SERVICE_ID,
            RUST_METHOD_ID,
        );
        let runtime_b = SampleRuntime::new(
            SampleRuntimes::Second as u32,
            JAVA_SERVICE_ID,
            JAVA_METHOD_ID,
        );

        let mut dispatcher = DispatcherBuilder::new()
            .with_runtime(runtime_a.runtime_type, runtime_a)
            .with_runtime(runtime_b.runtime_type, runtime_b)
            .finalize();

        let sample_rust_spec = ArtifactId {
            runtime_id: SampleRuntimes::First as u32,
            name: "first".into(),
        };
        let sample_java_spec = ArtifactId {
            runtime_id: SampleRuntimes::Second as u32,
            name: "second".into(),
        };

        // Check if the services are ready for deploy.
        let fork = db.fork();
        dispatcher
            .deploy_and_register_artifact(&fork, &sample_rust_spec, Any::default())
            .unwrap();
        dispatcher
            .deploy_and_register_artifact(&fork, &sample_java_spec, Any::default())
            .unwrap();

        // Check if the services are ready for initiation.
        dispatcher
            .start_service(
                &fork,
                InstanceSpec {
                    artifact: sample_rust_spec.clone(),
                    id: RUST_SERVICE_ID,
                    name: RUST_SERVICE_NAME.into(),
                },
                Any::default(),
            )
            .expect("start_service failed for rust");
        dispatcher
            .start_service(
                &fork,
                InstanceSpec {
                    artifact: sample_java_spec.clone(),
                    id: JAVA_SERVICE_ID,
                    name: JAVA_SERVICE_NAME.into(),
                },
                Any::default(),
            )
            .expect("start_service failed for java");

        // Check if transactions are ready for execution.
        let tx_payload = [0x00_u8; 1];

        let mut context = ExecutionContext::new(&fork, Caller::Blockchain);
        dispatcher
            .call(
                &mut context,
                CallInfo::new(RUST_SERVICE_ID, RUST_METHOD_ID),
                &tx_payload,
            )
            .expect("Correct tx rust");

        dispatcher
            .call(
                &mut context,
                CallInfo::new(RUST_SERVICE_ID, JAVA_METHOD_ID),
                &tx_payload,
            )
            .expect_err("Incorrect tx rust");

        dispatcher
            .call(
                &mut context,
                CallInfo::new(JAVA_SERVICE_ID, JAVA_METHOD_ID),
                &tx_payload,
            )
            .expect("Correct tx java");

        dispatcher
            .call(
                &mut context,
                CallInfo::new(JAVA_SERVICE_ID, RUST_METHOD_ID),
                &tx_payload,
            )
            .expect_err("Incorrect tx java");
    }

    #[test]
    fn test_dispatcher_rust_runtime_no_service() {
        const RUST_SERVICE_ID: ServiceInstanceId = 2;
        const RUST_SERVICE_NAME: &str = "rust-service";
        const RUST_METHOD_ID: MethodId = 0;

        // Create dispatcher and test data.
        let db = TemporaryDB::new();

        let mut dispatcher = DispatcherBuilder::default()
            .with_runtime(RuntimeIdentifier::Rust as u32, RustRuntime::default())
            .finalize();

        let sample_rust_spec =
            ArtifactId::new(RuntimeIdentifier::Rust as u32, "foo:1.0.0").unwrap();

        // Check deploy.
        assert_eq!(
            dispatcher
                .deploy_artifact(sample_rust_spec.clone(), Any::default())
                .wait()
                .expect_err("deploy artifact succeed"),
            RustRuntimeError::UnableToDeploy.into()
        );

        // Check if the services are ready to start.
        let fork = db.fork();

        assert_eq!(
            dispatcher
                .start_service(
                    &fork,
                    InstanceSpec {
                        artifact: sample_rust_spec.clone(),
                        id: RUST_SERVICE_ID,
                        name: RUST_SERVICE_NAME.into()
                    },
                    Any::default()
                )
                .expect_err("start service succeed"),
            Error::ArtifactNotDeployed.into()
        );

        // Check if transactions are ready for execution.
        let tx_payload = [0x00_u8; 1];

        let mut context = ExecutionContext::new(&fork, Caller::Blockchain);
        dispatcher
            .call(
                &mut context,
                CallInfo::new(RUST_SERVICE_ID, RUST_METHOD_ID),
                &tx_payload,
            )
            .expect_err("execute succeed");
    }
}